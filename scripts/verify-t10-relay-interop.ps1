# T10：Rust relay 与 C++ 客户端 / 服务端的互通验收入口。
#
# 默认使用桌面上的 C++ 原版：
#   C:\Users\Administrator\Desktop\kcptube.exe
# 也可以用 -CppExe 指向 scripts/build-legacy-mingw.ps1 的产物。
#
# 场景：
#   core     C++ client  -> Rust relay -> C++ server   （TCP / UDP / 32 KiB）
#   reverse  Rust client  -> C++ relay  -> Rust server  （反向验证 C++ relay）
#   fec      core + 两段 fec=10:3
#   mux      core + client mux_tunnels=2
#   all      以上全部
#
# 用法：
#   cargo build -p kcptube
#   pwsh -File scripts/verify-t10-relay-interop.ps1
#   pwsh -File scripts/verify-t10-relay-interop.ps1 -Scenarios core,fec
#
# 日志与生成的配置：build/t10/<时间戳>/<场景>/

[CmdletBinding()]
param(
    [string]$CppExe = "C:\Users\Administrator\Desktop\kcptube.exe",
    [string]$RustExe = "target/debug/kcptube.exe",
    [string]$MingwRoot = "D:\msys64\mingw64",
    [string[]]$Scenarios = @("core", "fec", "mux", "reverse")
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not (Test-Path $CppExe)) { throw "找不到 C++ 原版：$CppExe" }
if (-not (Test-Path $RustExe)) { throw "找不到 $RustExe，请先执行：cargo build -p kcptube" }
$CppExe = (Resolve-Path $CppExe).Path
$RustExe = (Resolve-Path $RustExe).Path

# 自行构建的 C++ 产物依赖 MinGW DLL，桌面原版通常不需要；存在就统一加上
if (Test-Path (Join-Path $MingwRoot "bin\g++.exe")) {
    $env:PATH = (Join-Path $MingwRoot "bin") + ";" + $env:PATH
}

if ($Scenarios -contains "all") {
    $Scenarios = @("core", "fec", "mux", "reverse")
}
# 兼容 -Scenarios fec,mux,reverse 被 PowerShell 当成单个字符串传入
$Scenarios = $Scenarios | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne "" }

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

function Invoke-Probe([string]$Mode, [int]$Port, [int]$Count, [int]$Size = 0) {
    $arguments = @(
        "scripts/net_probe.py", $Mode,
        "--host", "127.0.0.1", "--port", "$Port",
        "--count", "$Count", "--timeout", "20"
    )
    if ($Size -gt 0) { $arguments += @("--size", "$Size") } else { $arguments += @("--text", "t10") }
    $output = & python @arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$output" }
    Write-Host "    $output"
}

function Invoke-ProbeRetry([string]$Mode, [int]$Port, [int]$Count, [int]$Size = 0) {
    $last = $null
    for ($attempt = 1; $attempt -le 8; $attempt++) {
        try {
            Invoke-Probe $Mode $Port $Count $Size
            return
        } catch {
            $last = $_.Exception.Message
            Start-Sleep -Milliseconds 800
        }
    }
    throw "探测多次失败（$Mode :$Port）：$last"
}

function Get-ScenarioExecutables([string]$Tag) {
    switch ($Tag) {
        "core"    { return @{ Client = $CppExe; Relay = $RustExe; Server = $CppExe; Fec = $false; Mux = $false; RelayListenAtRoot = $false; SameCredentials = $false } }
        "fec"     { return @{ Client = $CppExe; Relay = $RustExe; Server = $CppExe; Fec = $true;  Mux = $false; RelayListenAtRoot = $false; SameCredentials = $false } }
        "mux"     { return @{ Client = $CppExe; Relay = $RustExe; Server = $CppExe; Fec = $false; Mux = $true;  RelayListenAtRoot = $false; SameCredentials = $false } }
        # C++ relay 在 Windows 上对 [listener] listen_on 会触发访问违例（0xC0000005），
        # 只能把 listen_on 放在根配置；Rust relay 不读取根的 listen_on，仍放段内。
        # 另外 C++ relay 的 egress 发送误用了 ingress 的加密参数（relay.cpp:1369），
        # 因此两段必须使用相同的密码与算法，否则数据发不出去。
        "reverse" { return @{ Client = $RustExe; Relay = $CppExe; Server = $RustExe; Fec = $false; Mux = $false; RelayListenAtRoot = $true; SameCredentials = $true } }
        default   { throw "未知场景：$Tag" }
    }
}

function Invoke-Scenario([string]$Tag) {
    $spec = Get-ScenarioExecutables $Tag
    $scenarioDir = Join-Path $script:LogRoot $Tag
    New-Item -ItemType Directory -Force $scenarioDir | Out-Null
    $script:Nodes = @()

    $echoPort = New-FreePort
    $serverPort = New-FreePort
    $relayPort = New-FreePort
    $clientPort = New-FreePort

    $fecLine = if ($spec.Fec) { "fec=10:3" } else { "" }
    $muxLine = if ($spec.Mux) { "mux_tunnels=2" } else { "" }
    $relayListenLine = if ($spec.RelayListenAtRoot) { "" } else { "listen_on=127.0.0.1" }
    $relayRootListen = if ($spec.RelayListenAtRoot) { "listen_on=127.0.0.1" } else { "" }
    $ingressPassword = if ($spec.SameCredentials) { "relay-pass" } else { "client-pass" }
    $egressPassword = if ($spec.SameCredentials) { "relay-pass" } else { "server-pass" }
    $ingressAlgorithm = "AES-GCM"
    $egressAlgorithm = if ($spec.SameCredentials) { "AES-GCM" } else { "AES-OCB" }

    Write-Host ""
    Write-Host "=== 场景 $Tag ==="
    Write-Host "客户端：$($spec.Client)"
    Write-Host "relay ：$($spec.Relay)"
    Write-Host "服务端：$($spec.Server)"
    Write-Host "端口：echo=$echoPort server=$serverPort relay=$relayPort client=$clientPort"

    Write-Conf (Join-Path $scenarioDir "server.conf") @"
mode=server
listen_on=127.0.0.1
listen_port=$serverPort
destination_address=127.0.0.1
destination_port=$echoPort
encryption_password=$egressPassword
encryption_algorithm=$egressAlgorithm
kcp=regular3
keep_alive=5
$fecLine
"@

    Write-Conf (Join-Path $scenarioDir "relay.conf") @"
mode=relay
$relayRootListen

[listener]
$relayListenLine
listen_port=$relayPort
encryption_password=$ingressPassword
encryption_algorithm=$ingressAlgorithm
kcp=regular3
keep_alive=5
$fecLine

[forwarder]
destination_address=127.0.0.1
destination_port=$serverPort
encryption_password=$egressPassword
encryption_algorithm=$egressAlgorithm
kcp=fast3
keep_alive=5
$fecLine
"@

    Write-Conf (Join-Path $scenarioDir "client.conf") @"
mode=client
listen_on=127.0.0.1
listen_port=$clientPort
destination_address=127.0.0.1
destination_port=$relayPort
encryption_password=$ingressPassword
encryption_algorithm=$ingressAlgorithm
kcp=regular3
keep_alive=5
$fecLine
$muxLine
"@

    $echo = Start-Process -FilePath "python" -ArgumentList @("scripts/net_echo.py", "$echoPort") -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $scenarioDir "echo.stdout.log") `
        -RedirectStandardError (Join-Path $scenarioDir "echo.stderr.log")
    Start-Sleep -Milliseconds 500
    if ($echo.HasExited) { throw "回显服务器启动失败，见 $scenarioDir/echo.stderr.log" }

    try {
        Start-Node $spec.Server (Join-Path $scenarioDir "server.conf") "server" $scenarioDir
        Start-Node $spec.Relay  (Join-Path $scenarioDir "relay.conf")  "relay"  $scenarioDir
        Start-Node $spec.Client (Join-Path $scenarioDir "client.conf") "client" $scenarioDir
        Start-Sleep -Milliseconds 1000

        Write-Host "  [1/3] TCP 短消息"
        Invoke-ProbeRetry "tcp" $clientPort 3
        Write-Host "  [2/3] UDP 短消息"
        Invoke-ProbeRetry "udp" $clientPort 3
        Write-Host "  [3/3] TCP 32 KiB"
        Invoke-ProbeRetry "tcp" $clientPort 1 32768

        foreach ($node in $script:Nodes) {
            if ($node.Process.HasExited) {
                throw "$($node.Name) 在探测后退出，见 $($node.Stdout) / $($node.Stderr)"
            }
        }
        Write-Host "  $Tag PASS"
    } catch {
        Write-Host "  $Tag FAIL：$($_.Exception.Message)" -ForegroundColor Red
        foreach ($node in $script:Nodes) {
            Write-Host "  --- $($node.Name) stdout ---"
            if (Test-Path $node.Stdout) { Get-Content $node.Stdout -Tail 25 }
            Write-Host "  --- $($node.Name) stderr ---"
            if (Test-Path $node.Stderr) { Get-Content $node.Stderr -Tail 25 }
        }
        throw
    } finally {
        Stop-Nodes
        if (-not $echo.HasExited) { Stop-Process -Id $echo.Id -Force -ErrorAction SilentlyContinue }
    }
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$script:LogRoot = Join-Path $repo "build/t10/$stamp"
New-Item -ItemType Directory -Force $script:LogRoot | Out-Null
Write-Host "T10 日志目录：$script:LogRoot"

$failed = @()
foreach ($tag in $Scenarios) {
    try {
        Invoke-Scenario $tag
    } catch {
        Write-Host "  $tag 错误：$($_.Exception.Message)" -ForegroundColor Red
        $failed += $tag
    }
}

Write-Host ""
if ($failed.Count -eq 0) {
    Write-Host "T10 PASS：$($Scenarios -join ', ')"
    Write-Host "日志目录：$script:LogRoot"
} else {
    Write-Host "T10 FAIL：$($failed -join ', ')" -ForegroundColor Red
    Write-Host "日志目录：$script:LogRoot"
    exit 1
}
