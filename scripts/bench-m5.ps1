# M5 性能基准：Rust 版与 C++ 参考版的 TCP echo 吞吐 / 延迟对比。
#
# 拓扑（每次场景相同，仅替换可执行文件）：
#   bench.py --TCP--> client(本地端口) ==KCP==> server ==TCP--> python echo
#
# 用法：
#   cargo build --release -p kcptube
#   pwsh -File scripts/bench-m5.ps1                     # 全部三个场景
#   pwsh -File scripts/bench-m5.ps1 -Scenario rust-rust # 只跑 Rust↔Rust
#
# 前提：
#   - C++ 参考版已构建：pwsh -File scripts/build-legacy-mingw.ps1
#   - C++ 版运行时需要 MinGW 的 DLL，脚本会把 $MingwRoot\bin 加进 PATH。
#
# 说明：
#   - 吞吐按「发送载荷 / 耗时」统计（回程会回显等量数据，双向合计约为其 2 倍），
#     场景之间口径一致；
#   - 延迟是 64 字节包的 ping-pong 往返，P99 取不小于 99% 样本的最小值；
#   - 两侧都不开 FEC / mux / 日志写盘，KCP 预设统一 regular3，加密统一 AES-GCM。

[CmdletBinding()]
param(
    [string]$RustExe = "target/release/kcptube.exe",
    [string]$CppExe = "build/legacy/kcptube.exe",
    [string]$MingwRoot = "D:\msys64\mingw64",
    [ValidateSet("all", "rust-rust", "cpp-cpp", "rust-client-cpp-server", "cpp-client-rust-server")]
    [string[]]$Scenario = @("all"),
    [double]$ThroughputMib = 32,
    [int]$ThroughputRepeat = 3,
    [int]$LatencyCount = 300,
    [switch]$SkipLatency
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not (Test-Path $RustExe)) {
    throw "找不到 $RustExe，请先执行：cargo build --release -p kcptube"
}
$RustExe = (Resolve-Path $RustExe).Path
if (-not (Test-Path $CppExe)) {
    throw "找不到 $CppExe，请先执行：pwsh -File scripts/build-legacy-mingw.ps1"
}
$CppExe = (Resolve-Path $CppExe).Path
if (-not (Test-Path (Join-Path $MingwRoot "bin"))) {
    throw "找不到 MinGW：$MingwRoot\bin"
}

# C++ 版依赖 libstdc++ / Botan 等 DLL。
$env:PATH = (Join-Path $MingwRoot "bin") + ";" + $env:PATH

if ($Scenario -contains "all") {
    $Scenario = @("rust-rust", "cpp-cpp", "rust-client-cpp-server", "cpp-client-rust-server")
}

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
            # UDP 同端口被占用，换一个
        }
    }
}

function Write-Conf([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

$script:Nodes = @()
function Start-Node([string]$Exe, [string]$Conf, [string]$Name, [string]$Dir) {
    $stdout = Join-Path $Dir "$Name.stdout.log"
    $stderr = Join-Path $Dir "$Name.stderr.log"
    $process = Start-Process -FilePath $Exe -ArgumentList @($Conf) -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $script:Nodes += [pscustomobject]@{ Name = $Name; Process = $process; Stdout = $stdout; Stderr = $stderr }
    Start-Sleep -Milliseconds 200
    if ($process.HasExited) {
        $tail = if (Test-Path $stderr) { Get-Content $stderr -Tail 10 } else { @() }
        throw "$Name 启动后立即退出（exit=$($process.ExitCode)）：$tail"
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

function Wait-Ready([int]$Port) {
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        & python scripts/net_probe.py tcp --port $Port --count 1 --timeout 2 *> $null
        if ($LASTEXITCODE -eq 0) { return }
        Start-Sleep -Milliseconds 250
    }
    throw "客户端端口 $Port 在 20 秒内未就绪"
}

function Invoke-Bench([string]$Metric, [int]$Port, [string[]]$Extra) {
    $output = & python scripts/bench.py $Metric --port $Port @Extra 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "bench.py $Metric 失败：$output"
    }
    return ($output | Select-Object -Last 1)
}

function Get-Median([double[]]$Values) {
    $sorted = $Values | Sort-Object
    return $sorted[[int][Math]::Floor($sorted.Count / 2)]
}

function Get-WorkingSetMb([System.Diagnostics.Process[]]$Processes) {
    $sum = 0
    foreach ($process in $Processes) {
        try {
            $process.Refresh() | Out-Null
            $sum += $process.WorkingSet64
        } catch { }
    }
    return [Math]::Round($sum / 1MB, 1)
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$root = Join-Path $repo "build/bench/$stamp"
New-Item -ItemType Directory -Force $root | Out-Null

$results = @()
foreach ($name in $Scenario) {
    $dir = Join-Path $root $name
    New-Item -ItemType Directory -Force $dir | Out-Null

    switch ($name) {
        "rust-rust" { $clientExe = $RustExe; $serverExe = $RustExe }
        "cpp-cpp" { $clientExe = $CppExe; $serverExe = $CppExe }
        "rust-client-cpp-server" { $clientExe = $RustExe; $serverExe = $CppExe }
        "cpp-client-rust-server" { $clientExe = $CppExe; $serverExe = $RustExe }
        default { throw "未知场景 $name" }
    }

    $echoPort = New-FreePort
    $serverPort = New-FreePort
    $clientPort = New-FreePort
    Write-Host "`n=== $name（echo=$echoPort server=$serverPort client=$clientPort）==="

    Write-Conf (Join-Path $dir "server.conf") @"
mode=server
listen_on=127.0.0.1
listen_port=$serverPort
destination_address=127.0.0.1
destination_port=$echoPort
encryption_password=bench-pass
encryption_algorithm=AES-GCM
kcp=regular3
"@

    Write-Conf (Join-Path $dir "client.conf") @"
mode=client
listen_on=127.0.0.1
listen_port=$clientPort
destination_address=127.0.0.1
destination_port=$serverPort
encryption_password=bench-pass
encryption_algorithm=AES-GCM
kcp=regular3
"@

    $echo = Start-Process -FilePath "python" -ArgumentList @("scripts/net_echo.py", "$echoPort") -PassThru `
        -WindowStyle Hidden -RedirectStandardOutput (Join-Path $dir "echo.stdout.log") `
        -RedirectStandardError (Join-Path $dir "echo.stderr.log")
    Start-Sleep -Milliseconds 500
    if ($echo.HasExited) { throw "回显服务器启动失败，见 $dir/echo.stderr.log" }

    try {
        Start-Node $serverExe (Join-Path $dir "server.conf") "server" $dir
        Start-Node $clientExe (Join-Path $dir "client.conf") "client" $dir
        Wait-Ready $clientPort

        $throughputs = @()
        for ($i = 1; $i -le $ThroughputRepeat; $i++) {
            $line = Invoke-Bench "throughput" $clientPort @("--size-mib", $ThroughputMib)
            $value = [double]([regex]::Match($line, "mib_s=([\d.]+)").Groups[1].Value)
            $throughputs += $value
            Add-Content (Join-Path $dir "raw.txt") $line
            Write-Host ("    吞吐 #{0}: {1:N2} MiB/s" -f $i, $value)
        }
        $throughput = Get-Median $throughputs

        $p50 = $null
        $p99 = $null
        if (-not $SkipLatency) {
            $line = Invoke-Bench "latency" $clientPort @("--count", $LatencyCount)
            Add-Content (Join-Path $dir "raw.txt") $line
            $p50 = [double]([regex]::Match($line, "ms_p50=([\d.]+)").Groups[1].Value)
            $p99 = [double]([regex]::Match($line, "ms_p99=([\d.]+)").Groups[1].Value)
            Write-Host ("    延迟 P50={0:N3} ms  P99={1:N3} ms" -f $p50, $p99)
        }

        $memory = Get-WorkingSetMb @($script:Nodes | ForEach-Object { $_.Process })
        Write-Host "    进程工作集（client+server，参考）：$memory MB"

        $results += [pscustomobject]@{
            Scenario   = $name
            Throughput = [Math]::Round($throughput, 2)
            P50Ms      = if ($null -eq $p50) { "-" } else { [Math]::Round($p50, 3) }
            P99Ms      = if ($null -eq $p99) { "-" } else { [Math]::Round($p99, 3) }
            WorkingSetMb = $memory
        }
    } finally {
        Stop-Nodes
        if (-not $echo.HasExited) { Stop-Process -Id $echo.Id -Force -ErrorAction SilentlyContinue }
    }
}

$report = Join-Path $root "report.md"
$lines = @(
    "# kcptube 性能基准（$stamp）",
    "",
    "| 场景 | TCP echo 吞吐 (MiB/s) | P50 (ms) | P99 (ms) | 进程工作集 (MB) |",
    "|---|---:|---:|---:|---:|"
)
foreach ($row in $results) {
    $lines += "| $($row.Scenario) | $($row.Throughput) | $($row.P50Ms) | $($row.P99Ms) | $($row.WorkingSetMb) |"
}
$lines += @(
    "",
    "测量口径：吞吐为「发送载荷 / 耗时」（64 KiB 一块，每轮 $ThroughputMib MiB，取 $ThroughputRepeat 轮中位数；",
    "延迟为 64 字节 ping-pong，$LatencyCount 次；两侧均关闭 FEC / mux / 日志写盘，统一 regular3 + AES-GCM。",
    "进程工作集在测量结束时读取 client、server 两个进程之和，仅作参考。"
)
[System.IO.File]::WriteAllLines($report, $lines, [System.Text.UTF8Encoding]::new($false))

Write-Host "`n结果："
$results | Format-Table -AutoSize
Write-Host "报告：$report"
