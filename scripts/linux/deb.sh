#!/usr/bin/env bash
# scripts/linux/deb.sh — XControl .deb 包构建（Linux）
#
# 依据：docs/08 §7（产物 deb，Linux）、docs/10-install.md §3（Linux 安装指引）、
#       docs/06 附录 A（gpui 运行链系统库）、docs/05 §4（GlobalShortcuts portal
#       需要 .desktop 桌面身份——11-19-m5-wayland 实测教训）。
#
# 做什么：
#   1. cargo build --release -p xcontrol-app -p xcontrol-cli --features "$FEATURES"
#      （docs/08 §3"单平台发布构建由打包脚本固定 --features"——无 build.rs 自动
#      选择，显式组合是唯一事实源）；
#   2. 组装 deb 目录结构（DEBIAN/control + /usr/bin 双二进制 + .desktop 桌面身份）；
#   3. dpkg-deb --build --root-owner-group 出包；
#   4. dpkg-deb --info / --contents 校验。
#
# 注意：
#   - Depends 只列 gpui 运行链确认项（fontconfig/xkbcommon/X11/Wayland client）；
#     uinput 权限（/dev/uinput 写）与 portal 授权属运行时权限路径（docs/05 §4/§5），
#     不是包依赖。
#   - 架构自动取 `dpkg --print-architecture`（amd64/arm64 均可出包）。
#   - 产物路径：target/pkg/linux/xcontrol_<版本>_<架构>.deb（target 已 gitignore）。
#
# 用法：scripts/linux/deb.sh [release|debug]   （默认 release）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

PROFILE="${1:-release}"
# 平台 feature 组合（docs/08 §3：打包脚本固定组合）。默认 x11——剪贴板 crate
# 单平台互斥（x11/wayland compile_error 门禁），Wayland 原生变体：
#   FEATURES=wayland bash scripts/linux/deb.sh   # 或 appimage.sh
#（KDE Wayland 已实测；GNOME/sway 矩阵移交 docs/05 §7 清单。Xwayland 兜底
# 由 x11 变体覆盖——桌面发行版默认携带 Xwayland。）
FEATURES="${FEATURES:-x11}"

VERSION="$(awk '/^\[workspace\.package\]/{flag=1;next} /^\[/{flag=0} flag && /^version *=/{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)"
if [ -z "$VERSION" ]; then
    echo "错误：无法从 Cargo.toml 解析 workspace 版本" >&2
    exit 1
fi
ARCH="$(dpkg --print-architecture)"

echo "==> cargo build（${PROFILE}，features=$FEATURES）"
case "$PROFILE" in
    release)
        cargo build --release -p xcontrol-app -p xcontrol-cli --features "$FEATURES"
        BIN_DIR="target/release"
        ;;
    debug)
        cargo build -p xcontrol-app -p xcontrol-cli --features "$FEATURES"
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

PKG_STEM="target/pkg/linux/deb/xcontrol-$VERSION"
echo "==> 组装 deb 目录：$PKG_STEM"
rm -rf "$PKG_STEM"
mkdir -p "$PKG_STEM/DEBIAN" \
         "$PKG_STEM/usr/bin" \
         "$PKG_STEM/usr/share/applications" \
         "$PKG_STEM/usr/share/icons/hicolor/scalable/apps" \
         "$PKG_STEM/usr/share/doc/xcontrol"

install -m 755 "$BIN_DIR/xcontrol-app" "$PKG_STEM/usr/bin/xcontrol-app"
install -m 755 "$BIN_DIR/xcontrol-cli" "$PKG_STEM/usr/bin/xcontrol-cli"

# .desktop 桌面身份（GlobalShortcuts portal 绑定前提，docs/05 §4.1；
# StartupWMClass 对齐窗口名，Wayland 下 xdg-activation 由 compositor 决定，docs/06 §2）。
cat > "$PKG_STEM/usr/share/applications/com.aixhc.XControl.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=XControl
Comment=LAN keyboard & mouse sharing and clipboard sync across machines
Exec=xcontrol-app
Terminal=false
Categories=Network;Utility;
StartupWMClass=XControl
Icon=xcontrol
DESKTOP

# 图标（SVG 矢量——环形加中心点，与托盘常态图标同形，docs/06 §5.1 M4 裁定）。
cat > "$PKG_STEM/usr/share/icons/hicolor/scalable/apps/xcontrol.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <circle cx="32" cy="32" r="22" fill="none" stroke="#4a90d9" stroke-width="7"/>
  <circle cx="32" cy="32" r="7" fill="#4a90d9"/>
</svg>
SVG

# 版权文件（deb 基础合规；许可证 = workspace license）。
cat > "$PKG_STEM/usr/share/doc/xcontrol/copyright" <<'COPYRIGHT'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: XControl
Source: https://github.com/aixhc/XControl

Files: *
Copyright: 2026 XControl contributors
License: MIT OR Apache-2.0
COPYRIGHT

# DEBIAN/control：Depends = gpui 运行链确认项（docs/06 附录 A；x11rb/wl-clipboard/
# evdev 均纯 Rust 无 C 依赖，不需要对应 -dev 包）。
cat > "$PKG_STEM/DEBIAN/control" <<CONTROL
Package: xcontrol
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: libfontconfig1, libfreetype6, libxkbcommon0, libxkbcommon-x11-0, libxcb1, libwayland-client0
Maintainer: XControl <xcontrol@aixhc.com>
Homepage: https://github.com/aixhc/XControl
Description: LAN keyboard & mouse sharing and clipboard sync across machines
 XControl shares one keyboard and mouse across macOS/Windows/Linux machines on
 a LAN: cursor edge switching, input forwarding, key remapping, clipboard sync.
 Encrypted (Noise XXpsk0), auto-discovery, tray-resident GUI plus headless mode.
CONTROL

DEB_OUT="target/pkg/linux/xcontrol_${VERSION}_${ARCH}.deb"
rm -f "$DEB_OUT"
echo "==> dpkg-deb --build --root-owner-group"
dpkg-deb --build --root-owner-group "$PKG_STEM" "$DEB_OUT"

echo "==> dpkg-deb --info 校验"
dpkg-deb --info "$DEB_OUT" | head -16
echo "==> dpkg-deb --contents（节选）"
dpkg-deb --contents "$DEB_OUT" | head -12

echo ""
echo "完成：$DEB_OUT"
ls -lh "$DEB_OUT"
echo "安装：sudo apt install ./$DEB_OUT （或 dpkg -i）"
echo "权限（docs/05 §4/§5）：uinput 注入需 /dev/uinput 写权限（input/uinput 组）；"
echo "Wayland portal 首次使用按 compositor 弹窗授权；GlobalShortcuts 依赖 .desktop 桌面身份（已随包安装）。"
