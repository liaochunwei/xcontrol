# scripts/windows/firewall.ps1 — XControl Windows 防火墙预配置
#
# 依据：docs/02 §2.3（防火墙注意，逐条）· docs/05 §2 打包行 · docs/10-install.md §2.3/§2.4
# 目的：在安装/首次运行前预授权入站规则。Windows 会在应用首次监听端口时弹出
#       授权框，用户误点"取消"将静默生成阻止规则且难以排查——docs/02 §2.3 明确
#       要求安装器必须预加防火墙入站规则（docs/05 §2 打包行同源）。
#
# 规则面（docs/02 §2.3 逐条）：
#   - 端口/协议：TCP 4243 + UDP 4243（会话走 TCP，发现 beacon/应答走 UDP；
#     端口可配置——改配置端口时须同步调整规则，docs/02 §1）。
#   - 方向：入站（dir=in action=allow）。
#   - Profile：仅 private（专用网络）。注意：网卡的网络位置必须为"专用网络"，
#     公用网络（public）下本规则不生效、发现与连接均不可用（docs/02 §2.3 原文；
#     检查/切换见 docs/10-install.md §2.2）。domain profile 不预置（企业域环境
#     接入属 M6 打包评估项）。
#
# 运行要求：
#   - 需管理员权限（netsh advfirewall 写入系统防火墙策略；非管理员运行被拒绝）。
#   - MSI 安装器集成（M6，docs/10-install.md §2.4）：安装器以提升权限执行本脚本
#     或等效 netsh 命令；本脚本是该集成的命令级蓝本，幂等语义（查重后 add）必须
#     在 custom action 中保留。
#
# 幂等性：netsh add 对同名规则不查重（同名规则会叠加），因此先经
#         Get-NetFirewallRule -DisplayName 查重，已存在即跳过——重复运行无害。
#         （netsh 的 name= 写入的是规则显示名称 DisplayName，内部名称由系统生成
#         GUID，查重必须按 DisplayName。）
#
# 验证方式（11-17-m3-packaging）：开发机为 macOS，无 PowerShell 运行环境——
# 本脚本经静态自查（结构/括号/引号配对、参数逐项对照 netsh 语法）+ 逻辑自查；
# Windows 首次实机执行属移交项（docs/10-install.md §2.6）。

param(
    # 卸载开关（MSI 卸载 custom action 复用同一脚本——对称清理，docs/10 §2.4）。
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$Port        = 4243
$RuleTcpName = 'XControl (TCP 4243)'
$RuleUdpName = 'XControl (UDP 4243)'

# --- 管理员检查（需管理员，docs/02 §2.3） ------------------------------------
$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error '需要管理员权限：请以管理员身份运行 PowerShell 后重试。（netsh advfirewall 写入需要提权；MSI 集成场景由安装器提升权限执行——docs/02 §2.3、docs/10-install.md §2.4。）'
    exit 1
}

function Add-XControlFirewallRule {
    param(
        [Parameter(Mandatory = $true)][string]$RuleName,
        [Parameter(Mandatory = $true)][ValidateSet('TCP', 'UDP')][string]$Protocol
    )

    # netsh 的 name= 参数写入的是规则的显示名称（DisplayName），内部名称（Name）
    # 由系统生成 GUID——幂等查重必须按 DisplayName。（规则名不含 PowerShell
    # 通配符字符 * ? [ ]，-DisplayName 的通配语义在此安全。）
    $existing = Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue
    if ($null -ne $existing) {
        Write-Host "已存在，跳过：$RuleName"
        return
    }

    $output = netsh advfirewall firewall add rule name="$RuleName" dir=in action=allow protocol=$Protocol localport=$Port profile=private
    if ($LASTEXITCODE -ne 0) {
        Write-Error "netsh 添加规则失败（$RuleName）：$($output -join ' ')"
        exit 1
    }
    Write-Host "已添加：$RuleName（入站 $Protocol $Port，profile=private）"
}

function Remove-XControlFirewallRule {
    param(
        [Parameter(Mandatory = $true)][string]$RuleName
    )

    # 对称幂等：规则不存在时 netsh delete 报非零——先查重，不存在即跳过（卸载可重入）。
    $existing = Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue
    if ($null -eq $existing) {
        Write-Host "不存在，跳过：$RuleName"
        return
    }

    $output = netsh advfirewall firewall delete rule name="$RuleName"
    if ($LASTEXITCODE -ne 0) {
        Write-Error "netsh 删除规则失败（$RuleName）：$($output -join ' ')"
        exit 1
    }
    Write-Host "已删除：$RuleName"
}

if ($Uninstall) {
    Write-Host 'XControl 防火墙清理（卸载对称面，docs/10 §2.4）：删除入站 TCP+UDP 4243 规则'
    Remove-XControlFirewallRule -RuleName $RuleTcpName
    Remove-XControlFirewallRule -RuleName $RuleUdpName
} else {
    Write-Host 'XControl 防火墙预配置：入站 TCP+UDP 4243，Private profile（docs/02 §2.3）'
    Add-XControlFirewallRule -RuleName $RuleTcpName -Protocol TCP
    Add-XControlFirewallRule -RuleName $RuleUdpName -Protocol UDP
}
Write-Host ''
Write-Host '提醒：规则仅在"专用网络"Profile 生效——确认本机网卡网络位置为专用网络（docs/02 §2.3）。'
Write-Host '验证规则：Get-NetFirewallRule -DisplayName "XControl*" | Format-Table DisplayName,Enabled,Direction,Profile'
﻿