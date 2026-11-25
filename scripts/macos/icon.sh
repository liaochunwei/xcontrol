#!/usr/bin/env bash
# scripts/macos/icon.sh — XControl macOS 应用图标（.icns）生成
#
# 依据：docs/06 §5.1（托盘图标图形）、docs/10 §1.1（.app 打包，图标经
#       CFBundleIconFile 引用）。应用图标此前从未进 bundle（Finder / Dock 显示
#       系统默认空白图标，2026-11-24 用户反馈补齐）。
#
# 做什么：
#   1. swift scripts/macos/icon.swift 绘制 iconset（10 个尺寸 PNG）；
#   2. iconutil 合成 .icns，写回产物路径（默认入库 scripts/macos/AppIcon.icns，
#      dmg.sh 打包时拷进 Contents/Resources）。
#
# 何时重跑：改动 icon.swift 的绘制（图形比例与 xcontrol-tray icons.rs 同源，
# 托盘图标改了也要同步这里），然后提交新的 .icns。构建打包不需要 swift——
# dmg.sh 只读取已生成的 .icns。
#
# 用法：scripts/macos/icon.sh [输出.icns 路径]   （默认 scripts/macos/AppIcon.icns）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUTPUT="${1:-$REPO_ROOT/scripts/macos/AppIcon.icns}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/xcontrol-icon.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "==> 绘制 iconset"
swift "$REPO_ROOT/scripts/macos/icon.swift" "$WORK/AppIcon.iconset"

echo "==> iconutil 合成：$OUTPUT"
iconutil -c icns "$WORK/AppIcon.iconset" -o "$OUTPUT"
ls -lh "$OUTPUT"
