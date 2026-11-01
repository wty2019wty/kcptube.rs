// SPDX-License-Identifier: GPL-3.0-only

//! mux 隧道（对应 C++ `networks/mux_tunnel.cpp`）。
//!
//! 一条 mux KCP 通道上复用多条本地连接，每条连接用 `connection_id` 区分：
//!
//! ```text
//! data_layer(mux_transfer, tcp|udp) + mux_data_wrapper(connection_id) + 负载
//! ```
//!
//! 收发分工：
//! - [`MuxTunnel::run`] 独占 KCP 通道，负责收发数据报、驱动 `update`、发送保活；
//! - 收到的 `mux_transfer` / `mux_cancel` / `pre_connect_custom_address`
//!   通过 [`MuxEvent`] 队列交给上层转发任务处理，避免 KCP 循环被本地 IO 阻塞；
//! - 上层要发数据时调用 [`MuxTunnel::send`]，命令经 [`MuxCommand`] 队列串行化。
//!
//! 与 C++ 的差异：C++ 用 `deque` + `pause()` 实现背压，这里用有界 `mpsc`
//! 队列，队满时发送方等待，效果等价（本地读变慢即背压）。

use crate::udp::UdpEndpoint;
use crate::{KcpChannel, KeepAlive, MAX_INNER_MESSAGE, TransportError, now_ms, now_secs};
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::PACKET_LENGTH;
use kcptube_protocol::packet::{
    Feature, PreConnectCustomAddress, ProtocolType, create_keep_alive_response_packet,
    create_mux_data_packet, extract_mux_cancel, extract_mux_data, extract_mux_pre_connect,
    inform_mux_cancel_packet, unpack_inner,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, interval};

/// 服务端对已取消连接的保留时长（秒），对应 C++ `gbv_cleanup_waits`。
pub const EXPIRING_WAITS: i64 = 4;

/// 一条 mux 命令队列的容量。
const COMMAND_CAPACITY: usize = 1024;
/// 事件队列容量（本地写变慢时对 KCP 形成背压）。
const EVENT_CAPACITY: usize = 256;

/// 本地 TCP 写入端。
pub struct MuxTcpSink {
    data: mpsc::Sender<Vec<u8>>,
    closed: watch::Sender<bool>,
}

impl MuxTcpSink {
    /// 创建写入端，返回 `(写入端, 数据接收端, 关闭通知)`。
    pub fn new(capacity: usize) -> (Arc<Self>, mpsc::Receiver<Vec<u8>>, watch::Receiver<bool>) {
        let (data_tx, data_rx) = mpsc::channel(capacity);
        let (closed_tx, closed_rx) = watch::channel(false);
        (
            Arc::new(Self {
                data: data_tx,
                closed: closed_tx,
            }),
            data_rx,
            closed_rx,
        )
    }

    /// 订阅关闭通知（可多次调用）。
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
}

/// 记录指向的「本地端」：收到的对端数据要写到这里。
pub enum MuxSink {
    /// 写入本地 TCP 连接（客户端是接入连接，服务端是到目标的连接）
    Tcp(Arc<MuxTcpSink>),
    /// 向本地 UDP 对端发送（客户端是本地源地址，服务端是目标地址）
    Udp {
        socket: Arc<UdpEndpoint>,
        peer: SocketAddr,
    },
}

impl MuxSink {
    /// 投递一段数据；返回 `false` 表示本地 TCP 端已不可写。
    ///
    /// UDP 发送失败（例如 Windows 的 `WSAECONNRESET`）只丢弃当前数据报，
    /// 记录本身交给空闲超时回收（与 C++ `async_send_out` 不检查错误一致）。
    pub async fn deliver(&self, payload: &[u8]) -> bool {
        match self {
            MuxSink::Tcp(sink) => sink.data.send(payload.to_vec()).await.is_ok(),
            MuxSink::Udp { socket, peer } => {
                let _ = socket.try_send_to(payload, *peer);
                true
            }
        }
    }
}

/// 一条 mux 连接记录。
pub struct MuxRecord {
    pub connection_id: u32,
    pub protocol: ProtocolType,
    pub sink: MuxSink,
    /// 所属隧道（弱引用，避免与隧道记录表形成循环）
    tunnel: std::sync::Weak<MuxTunnel>,
    last_activity: AtomicI64,
    closed: AtomicBool,
}

impl MuxRecord {
    pub fn new(
        connection_id: u32,
        protocol: ProtocolType,
        sink: MuxSink,
        tunnel: std::sync::Weak<MuxTunnel>,
    ) -> Self {
        Self {
            connection_id,
            protocol,
            sink,
            tunnel,
            last_activity: AtomicI64::new(now_secs()),
            closed: AtomicBool::new(false),
        }
    }

    /// 记录所属的隧道。
    pub fn tunnel(&self) -> Option<Arc<MuxTunnel>> {
        self.tunnel.upgrade()
    }

    /// 刷新最近一次数据交换时刻。
    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    pub fn last_activity(&self) -> i64 {
        self.last_activity.load(Ordering::Relaxed)
    }

    /// 通知本地读写任务退出（对应 C++ 的 `stop()`）。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let MuxSink::Tcp(sink) = &self.sink {
            let _ = sink.closed.send(true);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// 上层的发送命令。
pub enum MuxCommand {
    /// 发送 `mux_transfer`
    Transfer {
        protocol: ProtocolType,
        connection_id: u32,
        payload: Vec<u8>,
    },
    /// 发送 `mux_cancel`
    Cancel {
        protocol: ProtocolType,
        connection_id: u32,
    },
    /// 发送已拼好的明文包（预连接、保活回复等）
    Send(Vec<u8>),
    /// 结束通道
    Shutdown,
}

/// 收到的 mux 事件，交给上层转发任务。
pub enum MuxEvent {
    Transfer {
        protocol: ProtocolType,
        connection_id: u32,
        payload: Vec<u8>,
    },
    Cancel {
        protocol: ProtocolType,
        connection_id: u32,
    },
    PreConnect {
        protocol: ProtocolType,
        address: PreConnectCustomAddress,
    },
}

/// 一条 mux 隧道。
pub struct MuxTunnel {
    channel: Arc<KcpChannel>,
    conv: u32,
    commands: mpsc::Sender<MuxCommand>,
    events: mpsc::Sender<MuxEvent>,
    records: StdMutex<HashMap<u32, Arc<MuxRecord>>>,
    /// 服务端：已取消但仍在保留期的 `connection_id` → 取消时刻
    expiring: StdMutex<HashMap<u32, i64>>,
}

impl MuxTunnel {
    /// 建立隧道，返回 `(隧道, 命令接收端, 事件接收端)`。
    pub fn new(
        channel: Arc<KcpChannel>,
        conv: u32,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<MuxCommand>,
        mpsc::Receiver<MuxEvent>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let tunnel = Arc::new(Self {
            channel,
            conv,
            commands: command_tx,
            events: event_tx,
            records: StdMutex::new(HashMap::new()),
            expiring: StdMutex::new(HashMap::new()),
        });
        (tunnel, command_rx, event_rx)
    }

    pub fn channel(&self) -> &Arc<KcpChannel> {
        &self.channel
    }

    pub fn conv(&self) -> u32 {
        self.conv
    }

    /// 发送一段 mux 数据（对端无记录时上层会回 `mux_cancel`）。
    pub async fn send(&self, protocol: ProtocolType, connection_id: u32, payload: &[u8]) -> bool {
        self.commands
            .send(MuxCommand::Transfer {
                protocol,
                connection_id,
                payload: payload.to_vec(),
            })
            .await
            .is_ok()
    }

    /// 发送一段 mux 数据；队列满时直接丢弃（UDP 语义）。
    pub fn try_send(&self, protocol: ProtocolType, connection_id: u32, payload: Vec<u8>) -> bool {
        self.commands
            .try_send(MuxCommand::Transfer {
                protocol,
                connection_id,
                payload,
            })
            .is_ok()
    }

    /// 发送 `mux_cancel`。
    pub async fn send_cancel(&self, protocol: ProtocolType, connection_id: u32) -> bool {
        self.commands
            .send(MuxCommand::Cancel {
                protocol,
                connection_id,
            })
            .await
            .is_ok()
    }

    /// 发送一个已拼好的明文包。
    pub async fn send_packet(&self, packet: Vec<u8>) -> bool {
        self.commands.send(MuxCommand::Send(packet)).await.is_ok()
    }

    /// 请求通道结束。
    pub fn shutdown(&self) {
        let _ = self.commands.try_send(MuxCommand::Shutdown);
    }

    pub fn insert_record(&self, record: Arc<MuxRecord>) {
        if let Ok(mut records) = self.records.lock() {
            records.insert(record.connection_id, record);
        }
    }

    /// 新建并登记一条记录（自动绑定所属隧道）。
    pub fn add_record(
        self: &Arc<Self>,
        connection_id: u32,
        protocol: ProtocolType,
        sink: MuxSink,
    ) -> Arc<MuxRecord> {
        let record = Arc::new(MuxRecord::new(
            connection_id,
            protocol,
            sink,
            Arc::downgrade(self),
        ));
        self.insert_record(record.clone());
        record
    }

    pub fn record(&self, connection_id: u32) -> Option<Arc<MuxRecord>> {
        self.records
            .lock()
            .ok()
            .and_then(|records| records.get(&connection_id).cloned())
    }

    pub fn remove_record(&self, connection_id: u32) -> Option<Arc<MuxRecord>> {
        self.records
            .lock()
            .ok()
            .and_then(|mut records| records.remove(&connection_id))
    }

    pub fn record_count(&self) -> usize {
        self.records
            .lock()
            .map(|records| records.len())
            .unwrap_or(0)
    }

    /// 按本地 UDP 源地址查找记录（客户端用，对应 `udp_map_to_mux_records`）。
    pub fn find_udp_record(&self, peer: SocketAddr) -> Option<Arc<MuxRecord>> {
        let records = self.records.lock().ok()?;
        records
            .values()
            .find(|record| {
                matches!(&record.sink, MuxSink::Udp { peer: target, .. } if *target == peer)
            })
            .cloned()
    }

    /// 记录一条已取消的连接（服务端用于拦截迟到的数据）。
    pub fn mark_expiring(&self, connection_id: u32) {
        if let Ok(mut expiring) = self.expiring.lock() {
            expiring.insert(connection_id, now_secs());
        }
    }

    pub fn is_expiring(&self, connection_id: u32) -> bool {
        self.expiring
            .lock()
            .map(|expiring| expiring.contains_key(&connection_id))
            .unwrap_or(false)
    }

    /// 清理超过保留期的取消记录，并回收空闲的 UDP 记录。
    ///
    /// 返回需要通知对端取消的 `(协议, connection_id)` 列表。
    pub fn expire_idle(&self, udp_timeout: i64) -> Vec<(ProtocolType, u32)> {
        let now = now_secs();
        if let Ok(mut expiring) = self.expiring.lock() {
            expiring.retain(|_, at| now - *at < EXPIRING_WAITS);
        }

        let mut expired = Vec::new();
        let mut records = match self.records.lock() {
            Ok(records) => records,
            Err(_) => return expired,
        };
        records.retain(|_, record| {
            if record.protocol != ProtocolType::Udp {
                return true;
            }
            if now - record.last_activity() < udp_timeout {
                return true;
            }
            expired.push((record.protocol, record.connection_id));
            record.close();
            false
        });
        expired
    }

    /// 断开所有本地连接（对应 C++ `delete_mux_records`）。
    pub fn clear_records(&self) {
        if let Ok(mut records) = self.records.lock() {
            for record in records.values() {
                record.close();
            }
            records.clear();
        }
        if let Ok(mut expiring) = self.expiring.lock() {
            expiring.clear();
        }
    }

    /// 独占运行 KCP 通道，直到收到断开或发送方全部关闭。
    pub async fn run(
        &self,
        commands: &mut mpsc::Receiver<MuxCommand>,
        keep_alive: u16,
        udp_timeout: u16,
    ) -> Result<(), TransportError> {
        let mut ticker = interval(Duration::from_millis(5));
        let mut datagram = vec![0u8; PACKET_LENGTH + 256];
        let mut inner = vec![0u8; MAX_INNER_MESSAGE];
        let keep_alive = i64::from(keep_alive);
        let udp_timeout = i64::from(udp_timeout);
        let mut keep_alive = KeepAlive::new(keep_alive, now_secs());
        let mut next_expire = now_secs() + 1;

        loop {
            // 每次循环重新取当前 socket，端口跳跃后自动使用新 socket
            let socket = self.channel.socket();
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        MuxCommand::Transfer { protocol, connection_id, payload } => {
                            let packet = create_mux_data_packet(protocol, connection_id, &payload);
                            self.channel.send(&packet).await?;
                        }
                        MuxCommand::Cancel { protocol, connection_id } => {
                            let packet = inform_mux_cancel_packet(protocol, connection_id);
                            self.channel.send(&packet).await?;
                        }
                        MuxCommand::Send(packet) => self.channel.send(&packet).await?,
                        MuxCommand::Shutdown => break,
                    }
                }
                result = socket.recv_from(&mut datagram) => {
                    if let Ok((size, _)) = result {
                        let _ = self.channel.process_datagram(&datagram[..size]).await;
                    }
                }
                _ = ticker.tick() => {
                    let now = now_secs();
                    keep_alive.tick(&self.channel, ProtocolType::Mux, now).await?;
                    if now >= next_expire {
                        next_expire = now + 1;
                        for (protocol, connection_id) in self.expire_idle(udp_timeout) {
                            let packet = inform_mux_cancel_packet(protocol, connection_id);
                            self.channel.send(&packet).await?;
                            self.mark_expiring(connection_id);
                        }
                    }
                    self.channel.update(now_ms()).await?;
                }
            }

            loop {
                let size = match self.channel.receive(&mut inner).await {
                    Ok(size) => size,
                    Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                    Err(error) => return Err(error),
                };
                let Ok((feature, protocol, body)) = unpack_inner(&inner[..size]) else {
                    continue;
                };
                match feature {
                    Feature::MuxTransfer => {
                        if let Ok((connection_id, payload)) = extract_mux_data(body) {
                            let event = MuxEvent::Transfer {
                                protocol,
                                connection_id,
                                payload: payload.to_vec(),
                            };
                            if self.events.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Feature::MuxCancel => {
                        if let Ok(connection_id) = extract_mux_cancel(body) {
                            let event = MuxEvent::Cancel {
                                protocol,
                                connection_id,
                            };
                            if self.events.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Feature::PreConnectCustomAddress => {
                        if let Ok(address) = extract_mux_pre_connect(body) {
                            let event = MuxEvent::PreConnect { protocol, address };
                            if self.events.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Feature::KeepAlive => {
                        let response = create_keep_alive_response_packet(protocol);
                        self.channel.send(&response).await?;
                    }
                    Feature::KeepAliveResponse => {
                        self.channel.mark_keep_alive_response(now_secs());
                    }
                    Feature::Disconnect | Feature::Failure => return Ok(()),
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_inserted_and_removed() {
        // 仅验证记录表增删与关闭通知，不建立真实通道
        let records: StdMutex<HashMap<u32, Arc<MuxRecord>>> = StdMutex::new(HashMap::new());
        let (sink, _rx, _closed) = MuxTcpSink::new(4);
        let record = Arc::new(MuxRecord::new(
            9,
            ProtocolType::Tcp,
            MuxSink::Tcp(sink),
            std::sync::Weak::new(),
        ));
        records.lock().expect("测试锁不应中毒").insert(9, record);
        assert_eq!(records.lock().expect("测试锁不应中毒").len(), 1);
        let removed = records
            .lock()
            .expect("测试锁不应中毒")
            .remove(&9)
            .expect("记录应存在");
        removed.close();
        assert!(removed.is_closed());
        assert!(records.lock().expect("测试锁不应中毒").is_empty());
    }

    #[test]
    fn close_notifies_tcp_sink() {
        let (sink, _rx, closed) = MuxTcpSink::new(4);
        let record = MuxRecord::new(
            1,
            ProtocolType::Tcp,
            MuxSink::Tcp(sink),
            std::sync::Weak::new(),
        );
        assert!(!*closed.borrow());
        record.close();
        assert!(*closed.borrow());
    }
}
