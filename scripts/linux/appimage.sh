#!/usr/bin/env bash
# scripts/linux/appimage.sh — XControl AppImage 构建（Linux）
#
# 依据：docs/08 §7（产物 AppImage，Linux）、docs/10-install.md §3。
#
# 做什么：
#   1. cargo build --release -p xcontrol-app -p xcontrol-cli --features "$FEATURES"
#      （与 deb.sh 同一 feature 组合纪律，docs/08 §3）；
#   2. 组装 AppDir（usr/bin 双二进制 + .desktop + 图标 + AppRun 启动器）；
#   3. appimagetool 打包 → .AppImage（工具按序探测：PATH → $APPIMAGETOOL 环境变量
#      指向的 AppImage（--appimage-extract-and-run 免 FUSE）→ GitHub 下载官方工具；
#      全部不可得时交付已校验的 AppDir 并打印安装步骤——handover 记录）。
#
# AppRun 语义：手写启动器（静态 AppDir 不捆绑系统库——Depends 式运行链由宿主
# 提供，与 deb 包同边界；完整捆绑走 linuxdeploy 的 AppImage 模式，后续打磨）。
#
# 产物路径：target/pkg/linux/XControl-<版本>-x86_64.AppImage（或 xcontrol.AppDir/）。
#
# 用法：scripts/linux/appimage.sh [release|debug]   （默认 release）
#       环境变量 APPIMAGETOOL=/path/to/appimagetool-x86_64.AppImage 可指定工具。

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
ARCH="$(uname -m)"

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

APPDIR="target/pkg/linux/xcontrol.AppDir"
echo "==> 组装 AppDir：$APPDIR"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -m 755 "$BIN_DIR/xcontrol-app" "$APPDIR/usr/bin/xcontrol-app"
install -m 755 "$BIN_DIR/xcontrol-cli" "$APPDIR/usr/bin/xcontrol-cli"

# .desktop（AppImage 规范要求：Name/Exec/Icon/Type 必备；桌面身份 = portal
# GlobalShortcuts 绑定前提，docs/05 §4.1）。
cat > "$APPDIR/usr/share/applications/com.aixhc.XControl.desktop" <<'DESKTOP'
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

cat > "$APPDIR/usr/share/icons/hicolor/scalable/apps/xcontrol.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <circle cx="32" cy="32" r="22" fill="none" stroke="#4a90d9" stroke-width="7"/>
  <circle cx="32" cy="32" r="7" fill="#4a90d9"/>
</svg>
SVG

# 256x256 位图图标：appimagetool 的 .DirIcon 只从 PNG/XPM 自动生成（SVG 不认），
# 缺位图 = AppImage 顶层无图标 + 工具告警（2026-11-28 打包实测）。与 .svg 同形：
# 蓝色圆环 + 圆心（#4a90d9）。base64 内嵌保证脚本自包含（不依赖 ImageMagick）。
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"
base64 -d > "$APPDIR/usr/share/icons/hicolor/256x256/apps/xcontrol.png" <<'ICON'
iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAYAAABccqhmAAADJ0lEQVR42u3UQQ0AIAwEwepDHfaqpljgxYObSc4AoVsFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEGLtHq8Anxzzq3ltCDl2UQAHLwjg4AUBHL0YgMMXAnD0YgAOXwjA4QsBOHwhwOGbEODwTQhw+CYEOH4TARy+CQGO30QAx28igMM3IcDxmwjg+E0EcPgmBDh+EwEcv4kAjt9EAMdvIoDjNxHA8ZsI4PhNBHD8JgI4fhMBHL+JAAJgAuD4zUTA8ZuJgOM3EwEBMBMAx28mAo7fTAQcv5kICIAJAI7fRADHbyKAAJgA4PhNBBAAEwAcv4kAAmACgOM3EUAATABw/CYCCIAJAI7fRAABMAHA8ZsIIAAmAAiACQCO30QAATABwPGbCOCjmQAIgJkAOH4zERAAMwEQADMBEAAzAXD8ZiIgAGYCIABmAiAAZgLg+M1EQADMBEAAzARAAMwEQADMBMDxm4mAAJgJgACYCYAAmAmAAJgJgACYCYAAmAmAAJgJgACYCYAAmLleATABQABMABAAEwAcv4kAImCOHwEwAUAATAAQABMABMAEAAEwARAAn8kEQADMBEAAzARAAMwEQADMBEAAzARAAMwEQATMHL8AmAmAAJgJgACYCYAAmAmACJg5fgEwEwABMBMAATATABEwc/wCYCYAAmAmAAJgJgAiYOb4BcBMAATATABEwMxVCoAJACJgjh8BMAFAAEwAEAFz/AiACQAiYI4fATABQATM8SMAJgCIgDl+BMAEABEwx48AmAAgAub4EQATAETAHD8iYI4fATABQATM8SMC5vgRAXP8CIAJgAj41Ob4RcDM8YuAmeMXADMBEAFz/IiAOX5EwBw/ImCOHxEwx48ImONHBMzxIwLm+BEBc/yIgDl+hMAcPiJgjh8RMMePEJjDRwTM8SMC5vgRAnP4iIA5foTAHD5CYA4fITCHjxA4fBAChw9C4PBBDBw9CIHDBzFw9CAIDh4EwcGDKDh24DYaXgEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAhyAJnACiP9ZVs2AAAAAElFTkSuQmCC
ICON
# 顶层副本：appimagetool 以 AppDir 根的 Icon=<name> 位图生成 .DirIcon
cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/xcontrol.png" "$APPDIR/xcontrol.png"

cp "$APPDIR/usr/share/applications/com.aixhc.XControl.desktop" "$APPDIR/com.aixhc.XControl.desktop"

# AppRun 启动器（规范入口；转发全部参数给 xcontrol-app）。
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/xcontrol-app" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

echo "==> AppDir 自检（AppRun 可执行 / .desktop 就位 / 二进制在位）"
test -x "$APPDIR/AppRun"
test -f "$APPDIR/com.aixhc.XControl.desktop"
test -x "$APPDIR/usr/bin/xcontrol-app"
test -x "$APPDIR/usr/bin/xcontrol-cli"
echo "OK"

OUT_BASE="target/pkg/linux/XControl-$VERSION-$ARCH"
run_appimagetool() {
    # $1 = 工具调用方式（命令或已下载的 AppImage 路径）
    echo "==> appimagetool 打包（$1）"
    rm -f "$OUT_BASE.AppImage" "$OUT_BASE.AppImage.dig" 2>/dev/null || true
    if [ -f "$1" ]; then
        # AppImage 形态的工具：--appimage-extract-and-run 免 FUSE 挂载。
        ARCH="$ARCH" "$1" --appimage-extract-and-run "$APPDIR" "$OUT_BASE.AppImage"
    else
        ARCH="$ARCH" "$1" "$APPDIR" "$OUT_BASE.AppImage"
    fi
}

TOOL=""
if command -v appimagetool >/dev/null 2>&1; then
    TOOL="$(command -v appimagetool)"
elif [ -n "${APPIMAGETOOL:-}" ] && [ -f "$APPIMAGETOOL" ]; then
    TOOL="$APPIMAGETOOL"
else
    TOOL_DL="target/pkg/linux/appimagetool-$ARCH.AppImage"
    echo "==> 未找到 appimagetool，尝试官方工具下载（可设 APPIMAGETOOL 跳过）"
    if command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1; then
        URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
        mkdir -p "$(dirname "$TOOL_DL")"
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL -o "$TOOL_DL" "$URL" || true
        else
            wget -qO "$TOOL_DL" "$URL" || true
        fi
        if [ -f "$TOOL_DL" ]; then
            chmod 755 "$TOOL_DL"
            TOOL="$TOOL_DL"
        fi
    fi
fi

if [ -n "$TOOL" ]; then
    run_appimagetool "$TOOL"
    test -f "$OUT_BASE.AppImage"
    chmod 755 "$OUT_BASE.AppImage"
    echo "==> 产物自检（file 类型 + 大小）"
    file "$OUT_BASE.AppImage"
    ls -lh "$OUT_BASE.AppImage"
    echo ""
    echo "完成：$OUT_BASE.AppImage"
    echo "运行：chmod +x 后直接执行（./$(basename "$OUT_BASE.AppImage")）；"
    echo "权限（docs/05 §4/§5）：uinput 注入需 /dev/uinput 写权限；Wayland portal 首次使用弹窗授权。"
else
    echo ""
    echo "appimagetool 不可用（无网络/工具缺失）——交付已校验 AppDir：$APPDIR"
    echo "后续出包步骤（记录为 handover）："
    echo "  1) 下载 https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    echo "  2) APPIMAGETOOL=<路径> scripts/linux/appimage.sh"
    echo "  3) 或直接：appimagetool $APPDIR <输出>.AppImage"
    exit 0
fi
