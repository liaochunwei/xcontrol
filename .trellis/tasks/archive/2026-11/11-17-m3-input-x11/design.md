# design: X11 输入层

```
src/platform/x11.rs   # x11rb 连接/XRecord/XTEST（#[cfg(feature = "x11")]）
src/x11keymap.rs      # keycode-8=HID 校验 + 异常表（非门控）
```

- keycode↔HID：`keycode = usage + 8` 全量断言测试（3..=288 有效域）；异常表覆盖 docs/03 §2 提到的偏离键（以 linux/input-event-codes.h 公开值核对来源）。
- XRecord：`RecordContext` + `XRecordRange`（KeyPress/KeyRelease/ButtonPress/ButtonRelease/MotionNotify）+ 事件解析线程；连接复用策略（record 专用连接 vs 共享——x11rb 惯例专用连接回调，实现时按 docs.rs 定）。
- XTEST：`fake_input` KeyPress/KeyRelease（keycode=hid+8）、ButtonPress/Release（button 1-5，wheel=4/5 纵 6/7 横）、MotionNotify（relative=True 相对；绝对用 warp_pointer）。
- 文本注入：XTEST 无 Unicode 直发——走 keysym 合成（XChangeKeyboardMapping 动态 keysym 有侵入性）→ **M3 策略**：Text 事件在 x11 从机侧用 XTEST 按键序列不可行时降级为"每字符 keysym 经 keycode 映射表"或记录限制移交 M5（ashpd/reis Wayland 路径有 unicode 处理）。实现时调研 xtest + per-character keysym 的社区做法（x11- keysym 2..=255 latin-1 直发可行，超范围记录限制）。
- 验证：`rustup target add x86_64-unknown-linux-gnu` + target check（x11rb 纯 Rust，无需系统库）。
