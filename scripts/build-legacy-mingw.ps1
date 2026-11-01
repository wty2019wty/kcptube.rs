# 构建 legacy-cpp（C++ 参考实现）——仅为互通验收，不修改仓库内的 legacy-cpp/
#
# 用法：
#   pwsh -File scripts/build-legacy-mingw.ps1
#   pwsh -File scripts/build-legacy-mingw.ps1 -MingwRoot "C:\msys64\mingw64"
#
# 产物：build/legacy/kcptube.exe
# 说明：legacy-cpp/ 保持只读；所有 MinGW 兼容补丁只作用于副本 build/legacy-src/。

[CmdletBinding()]
param(
    [string]$Source = "legacy-cpp",
    [string]$WorkSource = "build/legacy-src",
    [string]$BuildDir = "build/legacy",
    [string]$MingwRoot = "D:\msys64\mingw64"
)

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not (Test-Path (Join-Path $MingwRoot "bin\g++.exe"))) {
    throw "找不到 MinGW：$MingwRoot\bin\g++.exe"
}

Write-Host "[1/4] 复制参考实现到 $WorkSource（不改动 $Source）"
if (Test-Path $WorkSource) {
    Remove-Item -Recurse -Force $WorkSource
}
New-Item -ItemType Directory -Force $WorkSource | Out-Null
Copy-Item -Path (Join-Path $Source "*") -Destination $WorkSource -Recurse -Force

Write-Host "[2/4] 应用 MinGW 兼容补丁"
$dns = Join-Path $WorkSource "src/networks/dns_helper.cpp"
$text = Get-Content -Raw -Encoding UTF8 $dns
# 补丁 1：MinGW 没有独立的 windnsdef.h，定义已在 windns.h 中
$text = [regex]::Replace($text, '#include <windnsdef\.h>\r?\n', '')
# 补丁 2：仅当 MinGW 把 DNS_TXT_DATAW 定义成 ANSI 指针时才转换
$windns = Join-Path $MingwRoot "include\windns.h"
$windnsText = Get-Content -Raw -Encoding UTF8 $windns
if ($windnsText -match '(?s)DNS_TXT_DATAW.*?PSTR\s+pStringArray') {
    Write-Host "      - DNS_TXT_DATAW 使用 PSTR，追加宽字符转换"
    $text = $text.Replace(
        'std::wstring ip_address_and_port = pRecord->Data.Txt.pStringArray[i];',
        'const char *txt = pRecord->Data.Txt.pStringArray[i]; std::wstring ip_address_and_port(txt, txt + std::strlen(txt));'
    )
    if ($text -notmatch '#include <cstring>') {
        $text = $text.Replace('#include <sstream>', "#include <sstream>`r`n#include <cstring>")
    }
} else {
    Write-Host "      - DNS_TXT_DATAW 已是 PWSTR，跳过 PSTR 补丁"
}
Set-Content -Path $dns -Value $text -Encoding UTF8 -NoNewline

Write-Host "[3/4] 配置 CMake（$BuildDir）"
$env:PATH = (Join-Path $MingwRoot "bin") + ";" + $env:PATH
if (Test-Path $BuildDir) {
    Remove-Item -Recurse -Force $BuildDir
}
cmake -S $WorkSource -B $BuildDir -G Ninja `
    -DCMAKE_BUILD_TYPE=Release `
    -DCMAKE_CXX_FLAGS="-I$(Join-Path $MingwRoot 'include/botan-3')" `
    -DCMAKE_CXX_STANDARD_LIBRARIES=-ldnsapi
if ($LASTEXITCODE -ne 0) { throw "CMake 配置失败" }

Write-Host "[4/4] 编译"
cmake --build $BuildDir -j
if ($LASTEXITCODE -ne 0) { throw "编译失败" }

$exe = Join-Path $BuildDir "kcptube.exe"
if (-not (Test-Path $exe)) { throw "未找到产物 $exe" }
Write-Host "完成：$exe"
Write-Host "运行前请把 $MingwRoot\bin 加入 PATH（Botan / libstdc++ 等 DLL）。"
