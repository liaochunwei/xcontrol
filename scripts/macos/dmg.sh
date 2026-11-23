#!/usr/bin/env bash
# scripts/macos/dmg.sh — XControl macOS .app bundle + DMG 构建
#
# 依据：docs/08 §7（产物 dmg，macOS）、docs/10-install.md §1（安装指引）、
#       docs/06 附录 A（NFR-4 UI 支持矩阵 macOS 15+）、docs/05 §1 打包行。
#
# 做什么：
#   1. cargo build --release -p xcontrol-app -p xcontrol-cli（平台 feature 按脚本
#      固定组合 macos——docs/08 §3"单平台发布构建由打包脚本固定 --features"）；
#   2. 组装 XControl.app bundle（Info.plist + MacOS 双二进制）；
#   3. codesign ad-hoc 签名 bundle（--deep 覆盖嵌套二进制——M6 bundle 化后启用，
#      codesign.sh 头注预告项）；
#   4. hdiutil create UDZO 压缩 DMG（含 /Applications 链接——拖拽安装惯例）；
#   5. hdiutil verify 校验产物。
#
# 注意：
#   - 正式签名（Developer ID + 公证）不在本脚本（CI/发布流程移交项，docs/05 §1）；
#     ad-hoc 已满足本地/直发场景的防火墙授权记忆（docs/10 §1.3）。
#   - **固定路径安装**：.app 移动会破坏已授予的辅助功能权限（TCC 按路径记忆，
#     docs/10 §1.1）——安装后不要再挪动 bundle。
#   - 产物路径：target/pkg/macos/XControl-<版本>.dmg（target 已 gitignore）。
#
# 用法：scripts/macos/dmg.sh [release|debug]   （默认 release）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

PROFILE="${1:-release}"

# workspace 版本（[workspace.package] 段第一处 version，docs/08 §7 semver）。
VERSION="$(awk '/^\[workspace\.package\]/{flag=1;next} /^\[/{flag=0} flag && /^version *=/{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)"
if [ -z "$VERSION" ]; then
    echo "错误：无法从 Cargo.toml 解析 workspace 版本" >&2
    exit 1
fi

PKG_DIR="target/pkg/macos"
APP_NAME="XControl.app"
STAGING="$PKG_DIR/dmg-staging"
APP_BUNDLE="$STAGING/$APP_NAME"

echo "==> cargo build（${PROFILE}，features=macos）"
case "$PROFILE" in
    release)
        cargo build --release -p xcontrol-app -p xcontrol-cli --features macos
        BIN_DIR="target/release"
        ;;
    debug)
        cargo build -p xcontrol-app -p xcontrol-cli --features macos
        BIN_DIR="target/debug"
        ;;
    *)
        echo "用法：$0 [release|debug]（默认 release）" >&2
        exit 2
        ;;
esac

for bin in xcontrol-app xcontrol-cli; do
    if [ ! -f "$BIN_DIR/$bin" ]; then
        echo "错误：构建产物不存在：$BIN_DIR/$bin" >&2
        exit 1
    fi
done

echo "==> 组装 bundle：$APP_BUNDLE"
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"
cp "$BIN_DIR/xcontrol-app" "$BIN_DIR/xcontrol-cli" "$APP_BUNDLE/Contents/MacOS/"

# Info.plist：LSMinimumSystemVersion = 15.0（NFR-4 UI 完整体验矩阵，gpui-kit 运行时约束）；
# 不设 LSUIElement（Dock 图标在位——on_reopen 依赖 Dock 语义，docs/06 §2）。
cat > "$APP_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>XControl</string>
    <key>CFBundleDisplayName</key>
    <string>XControl</string>
    <key>CFBundleIdentifier</key>
    <string>com.aixhc.XControl</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>xcontrol-app</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>15.0</string>
    <key>NSHumanReadableDescription</key>
    <string>LAN keyboard &amp; mouse sharing and clipboard sync across machines.</string>
</dict>
</plist>
PLIST

echo "==> ad-hoc 签名 bundle（codesign --force --deep -s -）"
codesign --force --deep -s - "$APP_BUNDLE"
codesign -dv --verbose=2 "$APP_BUNDLE" | head -8

echo "==> 组装 DMG staging（XControl.app + /Applications 链接）"
rm -rf "$STAGING/Applications"
ln -s /Applications "$STAGING/Applications"

DMG_OUT="$PKG_DIR/XControl-$VERSION.dmg"
rm -f "$DMG_OUT"
echo "==> hdiutil create（UDZO 压缩）：$DMG_OUT"
hdiutil create -volname "XControl $VERSION" -srcfolder "$STAGING" -format UDZO -ov "$DMG_OUT"

echo "==> hdiutil verify"
hdiutil verify "$DMG_OUT"

echo ""
echo "完成：$DMG_OUT"
ls -lh "$DMG_OUT"
echo "安装注意（docs/10 §1.1）：.app 拖入 /Applications 后固定路径使用——移动 bundle 会破坏"
echo "已授予的辅助功能权限（TCC 按路径记忆）；正式签名/公证属 CI 发布流程移交项（docs/05 §1）。"
