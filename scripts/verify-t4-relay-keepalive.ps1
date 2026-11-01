# T4：relay 主动保活，让「客户端不配 keep_alive」的空闲连接跨过 255 秒阈值。
#
# 拓扑（全 Rust）：client -- relay -- server -- echo
# 关键配置：client / server 都不配 keep_alive，只有 relay 的 [listener] 与 [forwarder] 配 keep_alive=1。
# 如果没有 relay 主动保活，relay 会在 255 秒后按空闲回收会话，第二次探测必然失败。
#
# 用法：
#   pwsh -File scripts/verify-t4-relay-keepalive.ps1                 # 默认空闲 300 秒
#   pwsh -File scripts/verify-t4-relay-keepalive.ps1 -IdleSeconds 30 # 冒烟（不覆盖 255 秒）
#
# 前提：已执行 cargo build -p kcptube

[CmdletBinding()]
param(
    [string]$RustExe = "target/debug/kcptube.exe",
    [int]$IdleSeconds = 300
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not (Test-Path $RustExe)) {
    throw "找不到 $RustExe，请先执行：cargo build -p kcptube"
}
$RustExe = (Resolve-Path $RustExe).Path

function New-FreePort {
    while ($true) {
        $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $port = $listener.LocalEndpoint.Port
        $listener.Stop()
        try {
            $udp = [System.Net.Sockets.UdpClient]::new($port)
            $udp.Dispose()
            return $port
        } catch {
            # 该端口 UDP 被占用，换一个
        }
    }
}

function Write-Conf([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

$script:Nodes = @()
function Start-Node([string]$Exe, [string]$Conf, [string]$Name, [string]$LogDir) {
    $stdout = Join-Path $LogDir "$Name.stdout.log"
    $stderr = Join-Path $LogDir "$Name.stderr.log"
    $process = Start-Process -FilePath $Exe -ArgumentList @($Conf) -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $script:Nodes += [pscustomobject]@{ Name = $Name; Process = $process; Stdout = $stdout; Stderr = $stderr }
    Start-Sleep -Milliseconds 300
    if ($process.HasExited) {
        throw "$Name 启动后立即退出（exit=$($process.ExitCode)），日志：$stdout / $stderr"
    }
}

function Stop-Nodes {
    foreach ($node in $script:Nodes) {
        if (-not $node.Process.HasExited) {
            Stop-Process -Id $node.Process.Id -Force -ErrorAction SilentlyContinue
        }
    }
    $script:Nodes = @()
}

function Invoke-Probe([string]$Mode, [int]$Port, [int]$Count = 3) {
    $output = & python scripts/net_probe.py $Mode --host 127.0.0.1 --port $Port --count $Count --timeout 20 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "探测失败（$Mode :$Port）：$output"
    }
    Write-Host "    $output"
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$logDir = Join-Path $repo "build/t4/$stamp"
New-Item -ItemType Directory -Force $logDir | Out-Null

$echoPort = New-FreePort
$serverPort = New-FreePort
$relayPort = New-FreePort
$clientPort = New-FreePort

Write-Host "T4 配置：echo=$echoPort server=$serverPort relay=$relayPort client=$clientPort，空闲 ${IdleSeconds}s"

Write-Conf (Join-Path $logDir "server.conf") @"
mode=server
listen_on=127.0.0.1
listen_port=$serverPort
destination_address=127.0.0.1
destination_port=$echoPort
encryption_password=server-pass
encryption_algorithm=AES-GCM
kcp=regular3
log_path=$logDir
"@

Write-Conf (Join-Path $logDir "relay.conf") @"
mode=relay
log_path=$logDir

[listener]
listen_on=127.0.0.1
listen_port=$relayPort
encryption_password=client-pass
encryption_algorithm=AES-GCM
kcp=regular3
keep_alive=1

[forwarder]
destination_address=127.0.0.1
destination_port=$serverPort
encryption_password=server-pass
encryption_algorithm=AES-GCM
kcp=regular3
keep_alive=1
"@

Write-Conf (Join-Path $logDir "client.conf") @"
mode=client
listen_on=127.0.0.1
listen_port=$clientPort
destination_address=127.0.0.1
destination_port=$relayPort
encryption_password=client-pass
encryption_algorithm=AES-GCM
kcp=regular3
log_path=$logDir
"@

$echo = Start-Process -FilePath "python" -ArgumentList @("scripts/net_echo.py", "$echoPort") -PassThru -WindowStyle Hidden `
    -RedirectStandardOutput (Join-Path $logDir "echo.stdout.log") `
    -RedirectStandardError (Join-Path $logDir "echo.stderr.log")
Start-Sleep -Milliseconds 500
if ($echo.HasExited) { throw "回显服务器启动失败，见 $logDir/echo.stderr.log" }

try {
    Start-Node $RustExe (Join-Path $logDir "server.conf") "server" $logDir
    Start-Node $RustExe (Join-Path $logDir "relay.conf") "relay" $logDir
    Start-Node $RustExe (Join-Path $logDir "client.conf") "client" $logDir
    Start-Sleep -Milliseconds 800

    Write-Host "[1/3] 空闲前探测"
    Invoke-Probe "tcp" $clientPort 3
    Invoke-Probe "udp" $clientPort 3

    Write-Host "[2/3] 停止收发，空闲 ${IdleSeconds} 秒（阈值 255 秒）"
    $elapsed = 0
    while ($elapsed -lt $IdleSeconds) {
        Start-Sleep -Seconds ([Math]::Min(30, $IdleSeconds - $elapsed))
        $elapsed += 30
        if ($elapsed -gt $IdleSeconds) { $elapsed = $IdleSeconds }
        Write-Host "      已空闲 ${elapsed}s"
        foreach ($node in $script:Nodes) {
            if ($node.Process.HasExited) {
                throw "$($node.Name) 在空闲期间退出，见 $($node.Stdout) / $($node.Stderr)"
            }
        }
    }

    Write-Host "[3/3] 空闲后探测（若 relay 未主动保活，此时会话已被 255 秒规则回收）"
    Invoke-Probe "tcp" $clientPort 3
    Invoke-Probe "udp" $clientPort 3

    Write-Host "T4 PASS：客户端未配置 keep_alive，连接仍跨过 ${IdleSeconds} 秒空闲"
    Write-Host "日志目录：$logDir"
} catch {
    Write-Host "T4 FAIL：$($_.Exception.Message)" -ForegroundColor Red
    foreach ($node in $script:Nodes) {
        Write-Host "--- $($node.Name) stdout ---"
        if (Test-Path $node.Stdout) { Get-Content $node.Stdout -Tail 20 }
        Write-Host "--- $($node.Name) stderr ---"
        if (Test-Path $node.Stderr) { Get-Content $node.Stderr -Tail 20 }
    }
    throw
} finally {
    Stop-Nodes
    if (-not $echo.HasExited) { Stop-Process -Id $echo.Id -Force -ErrorAction SilentlyContinue }
}
