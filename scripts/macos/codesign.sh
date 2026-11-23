#!/usr/bin/env bash
# scripts/macos/codesign.sh — XControl macOS ad-hoc 自签
#
# 依据：docs/05 §1 打包行（"签名（自签起步）"）、docs/02 §2.3（macOS：正式/Ad-hoc
# 签名，否则应用防火墙反复拦截）、docs/10-install.md §1.3。
#
# 做什么：cargo 构建 xcontrol-app 产物 → codesign ad-hoc（`--force -s -`）→
#         `codesign -dv --verbose=2` 输出验证信息（Signature=adhoc 即成功凭证）。
#
# 为什么必须签：macOS 应用防火墙按签名身份记忆授权；未签名二进制每次监听都可能
# 被反复拦截/重复弹窗（docs/02 §2.3 原文）。ad-hoc 签名（identity "-"）让防火墙
# 能以固定代码签名识别本产物——本地开发与 M3 阶段足够。
# 正式签名（Developer ID + 公证）属 M6（docs/05 §1"签名（自签起步）"、docs/09 M6）。
#
# 注意：
#   - 产物是裸 Mach-O 二进制（非 .app bundle），无需 --deep（Apple 仅对含嵌套代码
#     的 bundle 建议 --deep；M6 出 .app bundle 时再启用）。
#   - xcontrol-cli 同样可作为监听端，如需一并签名：
#     codesign --force -s - target/release/xcontrol-cli
#   - 防火墙"阻止所有传入连接"开启时 mDNS/UDP 发现均不可用（docs/02 §2.3）——与
#     签名无关，需在系统设置单独关闭该开关。
#   - 重复运行无害：--force 覆盖既有签名（幂等）。
#
# 用法：scripts/macos/codesign.sh [release|debug]   （默认 release）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

PROFILE="${1:-release}"

case "$PROFILE" in
    release)
        echo "==> cargo build --release -p xcontrol-app"
        cargo build --release -p xcontrol-app
        BIN="target/release/xcontrol-app"
        ;;
    debug)
        echo "==> cargo build -p xcontrol-app（debug）"
        cargo build -p xcontrol-app
        BIN="target/debug/xcontrol-app"
        ;;
    *)
        echo "用法：$0 [release|debug]（默认 release）" >&2
        exit 2
        ;;
esac

if [ ! -f "$BIN" ]; then
    echo "错误：构建产物不存在：$BIN" >&2
    exit 1
fi

echo "==> ad-hoc 签名（codesign --force -s -）：$BIN"
codesign --force -s - "$BIN"

echo "==> 验证（codesign -dv --verbose=2）"
codesign -dv --verbose=2 "$BIN"

echo ""
echo "完成：$BIN 已 ad-hoc 签名。"
echo "未签名对照：无签名时 macOS 应用防火墙会在每次监听时反复拦截（docs/02 §2.3）；"
echo "分发签名（Developer ID + 公证）属 M6，见 docs/10-install.md §1.3。"
