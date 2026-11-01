# 构建 kcptube 发布产物（Windows + Linux musl），并生成校验和。
#
# 用法：
#   pwsh -File scripts/build-release.ps1                     # 两个平台都构建（需 zig）
#   pwsh -File scripts/build-release.ps1 -Targets windows    # 只构建当前平台
#   pwsh -File scripts/build-release.ps1 -Targets linux
#
# 产物（dist/）：
#   kcptube-<版本>-windows-x64.zip
#   kcptube-<版本>-linux-x64.tar.gz
#   SHA256SUMS.txt
#
# 说明：
#   - Windows 使用当前 Rust 工具链（本机为 x86_64-pc-windows-gnu；CI 为 msvc）。
#   - Linux 使用 cargo-zigbuild 交叉编译到 x86_64-unknown-linux-musl（静态链接）。
#   - zig 路径可用 -ZigPath 指定，默认取 $env:CARGO_ZIGBUILD_ZIG_PATH。

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$OutDir = "dist",
    [string]$ZigPath = $env:CARGO_ZIGBUILD_ZIG_PATH,
    [ValidateSet("all", "windows", "linux")]
    [string]$Targets = "all"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if ([string]::IsNullOrWhiteSpace($Version)) {
    $match = [regex]::Match(
        (Get-Content -Raw Cargo.toml),
        '(?ms)^\[workspace\.package\].*?^\s*version\s*=\s*"([^"]+)"'
    )
    if (-not $match.Success) {
        throw "无法从 Cargo.toml 的 [workspace.package] 读取版本号"
    }
    $Version = $match.Groups[1].Value
}
Write-Host "版本：$Version"

$dist = Join-Path $repo $OutDir
$buildRoot = Join-Path $repo "build/release"
New-Item -ItemType Directory -Force $dist | Out-Null
New-Item -ItemType Directory -Force $buildRoot | Out-Null

function New-StagingDirectory([string]$Name, [string]$Exe, [string]$ExeName) {
    $stage = Join-Path $buildRoot $Name
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null

    Copy-Item $Exe (Join-Path $stage $ExeName)
    foreach ($file in @("README.md", "LICENSE", "THIRD-PARTY.md")) {
        Copy-Item $file $stage
    }
    # 发布包只带面向使用者的文档，不带 docs/dev。
    $docs = Join-Path $stage "docs"
    New-Item -ItemType Directory -Force $docs | Out-Null
    Copy-Item (Join-Path "docs" "*.md") $docs
    Copy-Item "service" (Join-Path $stage "service") -Recurse
    return $stage
}

function Write-Checksum([string]$Path) {
    Write-Host "  已生成 $(Split-Path -Leaf $Path)"
}

$checksumFile = Join-Path $dist "SHA256SUMS.txt"
if (Test-Path $checksumFile) { Remove-Item -Force $checksumFile }

if ($Targets -eq "all" -or $Targets -eq "windows") {
    # 静态 CRT：发布包不依赖 VC++ 运行时 / MinGW DLL（msvc 与 gnu 均支持该 flag）。
    Write-Host "[windows] cargo build --release（-C target-feature=+crt-static）"
    $savedRustFlags = $env:RUSTFLAGS
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
    try {
        cargo build --release -p kcptube
        if ($LASTEXITCODE -ne 0) { throw "Windows release 构建失败" }

        $exe = Join-Path $repo "target/release/kcptube.exe"
        if (-not (Test-Path $exe)) { throw "未找到 $exe" }
        $stage = New-StagingDirectory "windows-x64" $exe "kcptube.exe"
        $zip = Join-Path $dist "kcptube-$Version-windows-x64.zip"
        if (Test-Path $zip) { Remove-Item -Force $zip }
        Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
        Write-Checksum $zip
    } finally {
        $env:RUSTFLAGS = $savedRustFlags
    }
}

if ($Targets -eq "all" -or $Targets -eq "linux") {
    $isWindowsHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )
    if ($isWindowsHost) {
        # Windows 交叉编译到 musl 需要 zig 作为链接器。
        if ([string]::IsNullOrWhiteSpace($ZigPath) -or -not (Test-Path $ZigPath)) {
            throw "找不到 zig，请安装后设置 CARGO_ZIGBUILD_ZIG_PATH 或用 -ZigPath 指定"
        }
        $env:CARGO_ZIGBUILD_ZIG_PATH = $ZigPath
        Write-Host "[linux] cargo zigbuild --release --target x86_64-unknown-linux-musl"
        cargo zigbuild --release --target x86_64-unknown-linux-musl -p kcptube
    } else {
        # Linux 主机（CI）用原生 musl 工具链，不需要 zig：
        #   rustup target add x86_64-unknown-linux-musl
        #   apt-get install -y musl-tools
        Write-Host "[linux] cargo build --release --target x86_64-unknown-linux-musl"
        cargo build --release --target x86_64-unknown-linux-musl -p kcptube
    }
    if ($LASTEXITCODE -ne 0) { throw "Linux release 构建失败" }

    $exe = Join-Path $repo "target/x86_64-unknown-linux-musl/release/kcptube"
    if (-not (Test-Path $exe)) { throw "未找到 $exe" }
    $stage = New-StagingDirectory "linux-x64" $exe "kcptube"
    $tar = Join-Path $dist "kcptube-$Version-linux-x64.tar.gz"
    if (Test-Path $tar) { Remove-Item -Force $tar }
    tar -czf $tar -C $stage .
    if ($LASTEXITCODE -ne 0) { throw "打包 $tar 失败" }
    Write-Checksum $tar
}

Write-Host "`n完成，产物："
Get-ChildItem $dist | Select-Object Name, Length | Format-Table -AutoSize

# 统一生成校验和，覆盖 dist 中当前版本的全部产物（单目标构建也能保持一致）。
foreach ($package in Get-ChildItem $dist -File | Where-Object { $_.Name -like "kcptube-$Version-*" }) {
    $hash = (Get-FileHash -Path $package.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    Add-Content -Path $checksumFile -Value "$hash  $($package.Name)"
}
Write-Host "校验和：$checksumFile"
