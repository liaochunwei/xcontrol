# macOS 输入层要点（xcontrol-input/platform/macos.rs）

来源：docs/05 §1、M2 输入任务实测（11-17-m2-input-macos）。CG API 用法不确定时查 docs.rs + Apple 头文件（phracker/MacOSX-SDKs 镜像可下载核验），禁止凭记忆。

## 事件语义坑（M2 实测教训）

1. **指针定位三路（rel/abs/warp）统一走 `CGWarpMouseCursorPosition`，不要走 HID-posted `MouseMoved`**（11-19-m5-layout 实测，docs/05 §1.1）：HID-posted MouseMoved 对光标位置的施加不可靠（多轮探针 0/3 精确命中、相对路径净移动大量丢失），Session tap 2/3；warp 3/3 亚像素命中。语义代价已被接受：warp 不生成事件 → hover 高亮不随受控光标刷新（视觉提示损失，控制语义无影响）；按键/滚轮仍 CGEventPost。相对移动的"读改写"由注入层承担（读 `CGEventGetLocation` → warp 目标点）。证据测试：`inject_mouse_move_abs_centers_cursor_on_main_display`（#[ignore]，需辅助功能授权）。
2. **FlagsChanged 方向判定必须用设备相关位**（`NX_DEVICEL{SHIFT,CTL,CMD,ALT}KEYMASK` / `NX_DEVICER{...}`，IOLLEvent.h；core-graphics bitflags 未定义需本地声明）——用家族位判定会在左右同族修饰键同按时把"松开一颗"误报成 Down（卡位）。回归测试 `flags_changed_same_family_release_reports_up` 钉死。
2. **CapsLock 经 FlagsChanged 到达但无 mods bit**：用 `NX_ALPHASHIFTMASK`(0x00010000) 锁存位判定方向产出 Key 事件，否则 mac 主控永远发不出 CapsLock（docs/03 §4.1 映射示例需要它）。
3. **合成 CG 事件做测试必须用具体事件构造器**（`CGEvent::new_keyboard_event` 等）——`CGEvent::new`（Null 探针）字段不驻留，字段读取全失败。
4. **F 键 kVK 值非连续**（F1=0x7A、F2=0x78、F3=0x63、F5=0x60、F12=0x6F）——查表来源 HIToolbox/Events.h；keymap 全表 112 项已对 macOS 11.3 SDK 头文件全量核验。

## 结构与纪律

- kVK→HID 表在 `keymap.rs`（非门控纯数据区，CI 任意平台可测）；平台 API 全部在 `platform/macos.rs`（feature 门控 + 文件级 `#![allow(unsafe_code)]`，workspace lints 仍全量继承——unsafe 放行落在实际位置，docs/08 §2 定稿）。
- 权限检测独立于 tap 创建：`AXIsProcessTrustedWithOptions` 显式做；tap 创建成功 ≠ 有权限（无权限时系统静默忽略回调，不报错）。
- 回环测试手法：注入 `CGEventPost` → 会话 tap 捕获 → kVK→XKey → mpsc，可端到端断言（需辅助功能授权；`#[ignore]` 标注防 CI 误跑侵入式注入）。

## 遗留（M3 前必办）

- docs/02 §4.2 Wheel delta 符号约定未钉死（mac 原生：axis1 正=上、axis2 正=左）——M3 映射引擎动工前先改 docs。
- docs/03 §6 的"转换表 golden fixture 管线（xtask 生成 CSV）"未兑现——M3 三平台表任务一并落地或修订该条。
- 媒体键捕获走 NSSystemDefined 而非 KeyDown（注入路径 keymap 已覆盖，捕获 M3+）。
