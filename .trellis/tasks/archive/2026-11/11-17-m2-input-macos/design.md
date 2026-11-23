# design: macOS 输入捕获与注入

## 模块结构（xcontrol-input）

```
src/
├── lib.rs          # trait re-export（core InputSource/InputSink）+ feature 门控声明
├── platform/
│   └── macos.rs    # 全部 macos 实现（#[cfg(feature = "macos")]）
├── keymap.rs       # kVK→HID 静态映射表（跨平台纯数据，无 unsafe，默认编译——便于 CI 测试）
└── error.rs        # InputError
```

- `keymap.rs` 放非门控区（纯 u16 表 + 测试），`platform/macos.rs` 持有全部 CG API 调用——红线 2 的"平台模块命名固定 platform/macos.rs"。
- docs/03 §2 提到 X11/Wayland 转换表也在此 crate；本任务只做 macos 表，其余 platform 模块留 M3。

## 关键技术点（以 docs/05 §1 为准，实现时对照 docs.rs 核实 API 签名）

- **捕获线程模型**：`CGEventTap::new` → `CFRunLoop::run_current` 在专用 `std::thread`；tap callback 是 C ABI（`extern "C" fn`）——unsafe 块仅限 FFI 边界，回调内部安全化（收 `*mut` context → 引用 mpsc Sender）。
- **权限**：`AXIsProcessTrustedWithOptions` 经 `core-foundation` 的 CFString 字典构造；prompt 选项参数化（首次运行向导 M4 用 prompt=true，后台检测用 false）。
- **注入 f64 累加器**：`Injector` 结构持 `accum_dx/accum_dy: f64`，`MouseMoveRel` 累加后取整注入并保留余量（docs/02 §4.2 语义）。
- **修饰键注入**：`CGEventSetFlags` 合成（比逐个 post 辅助事件简单可靠，docs/05 允许"flags 合成"路径）。
- **屏幕几何**：优先 `core-graphics` 的 `CGGetActiveDisplayList` + `CGDisplayBounds`（纯 CG，避开 AppKit 主线程要求）。

## 测试策略

- keymap 表：纯函数全量锚点测试（CI 可跑，无需权限/macOS 亦可跑——表是纯数据）。
- 权限/tap/注入冒烟：`#[ignore]` 标注的"本机冒烟"测试（需 macOS 运行环境；CI 不跑），权限 Denied 路径默认可跑。

## 依赖

`core-graphics`、`core-foundation`（仅 macos feature）——版本以 docs/01 §6 调研基线为准；双登记 + input crate 白名单（macos feature 条件依赖进 GATED_CRATES）。input 非 GATED？——net/config 先例：input 也进 GATED_CRATES（直接依赖白名单），全树黑名单扫描经 core（若 core 依赖 input…core 不依赖 input！）→ **注意**：docs/08 §1 依赖图 app/cli → input；input 不被 core 依赖，故全树扫描不覆盖 input 的传递树——与 net 同待遇 GATED_CRATES + 白名单，check 注释说明。
