# scripts/windows/msi/build.ps1 — XControl MSI 构建驱动（Windows）
#
# 依据：docs/08 §7（产物 msi）、docs/10-install.md §2.4（安装器防火墙预配置蓝本）。
# 移交项（11-19-m6-release）：本仓库无 Windows 构建机——脚本 + wxs 交付，
# Windows 侧按本脚本实跑验证（release.yml 扩展同步移交，见 workflow 头注）。
#
# 前置：
#   1. Rust 工具链（rust-toolchain.toml 锁 1.99）+ MSVC + Windows SDK；
#   2. WiX Toolset v3（candle.exe / light.exe 在 PATH；choco install wixtoolset）
#      或 WiX v4（dotnet tool install --global wix——v4 需把 wxs 升级到 v4 schema，
#      本目录 wxs 按 v3 schema 交付，wixl/msitools 亦可语法校验，见下）。
#
# 做什么：
#   cargo build --release -p xcontrol-app -p xcontrol-cli --features windows
#     → candle/light 出 XControl-<版本>-x64.msi（内嵌 cab）
#     → 输出文件清单 + 版本断言。
#
# 用法（管理员非必需——MSI 内的防火墙 custom action 在安装时才提权）：
#   powershell -ExecutionPolicy Bypass -File scripts\windows\msi\build.ps1

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

# 1) workspace 版本（[workspace.package] version，与 deb/dmg 脚本同一事实源）。
$CargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
if ($CargoToml -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw "无法从 Cargo.toml 解析 workspace 版本"
}
$Version = $Matches[1]
Write-Host "==> 版本：$Version"

# 2) release 构建（平台 feature 固定组合 windows——docs/08 §3）。
Write-Host "==> cargo build --release -p xcontrol-app -p xcontrol-cli --features windows"
cargo build --release -p xcontrol-app -p xcontrol-cli --features windows
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

$BinDir = Join-Path $RepoRoot "target\release"
foreach ($bin in @("xcontrol-app.exe", "xcontrol-cli.exe")) {
    if (-not (Test-Path (Join-Path $BinDir $bin))) { throw "构建产物缺失：$bin" }
}

# 3) WiX 编译（candle + light；ProductVersion/BinDir/RepoRoot 预处理器变量注入）。
$MsiDir = Join-Path $RepoRoot "target\pkg\windows"
New-Item -ItemType Directory -Force -Path $MsiDir | Out-Null
$Wxs = Join-Path $PSScriptRoot "xcontrol.wxs"

Write-Host "==> candle（编译 wxs）"
candle -arch x64 `
    -dProductVersion="$Version" `
    -dBinDir="$BinDir" `
    -dRepoRoot="$RepoRoot" `
    -out "$MsiDir\xcontrol.wixobj" $Wxs
if ($LASTEXITCODE -ne 0) { throw "candle 失败" }

Write-Host "==> light（链接出 MSI，内嵌 cab）"
light -out "$MsiDir\XControl-$Version-x64.msi" `
    -ext WixUIExtension `
    "$MsiDir\xcontrol.wixobj"
if ($LASTEXITCODE -ne 0) { throw "light 失败" }

# 4) 产物断言（版本 + 大小）。
$Msi = Get-Item "$MsiDir\XControl-$Version-x64.msi"
Write-Host "==> 产物：$($Msi.FullName)（$([math]::Round($Msi.Length / 1MB, 2)) MB）"
Write-Host "安装验证（有 Windows 的机器上执行，docs/10 §2/§5）："
Write-Host "  msiexec /i XControl-$Version-x64.msi        # 安装（自动提权 → firewall.ps1 幂等预配置）"
Write-Host "  Get-NetFirewallRule -DisplayName 'XControl*' | Format-Table DisplayName,Enabled,Profile"
Write-Host "  xcontrol-cli --status                        # 安装后验证（docs/10 §5）"
Write-Host "  msiexec /x XControl-$Version-x64.msi         # 卸载（对称清理防火墙规则）"

# 交叉校验提示：无 WiX 的 Linux 环境可用 wixl（msitools）做语法级校验——
#   wixl -a x64 scripts/windows/msi/xcontrol.wxs
# （仅结构校验；custom action 的 FileRef 引用在 wixl 下行为差异属已知边界，出包以 Windows 实跑为准。）
