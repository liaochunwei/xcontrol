# M3: X11 输入层

## Goal

`xcontrol-input/x11`：XRecord 被动监听捕获 + XTEST 注入 + keycode-8=HID 校验表。编译级验证（`--target x86_64-unknown-linux-gnu`，x11rb 纯 Rust）；运行时移交 Linux 机器。

## Requirements

1. **捕获**（docs/05 §3）：`x11rb`（record feature）XRecord 上下文被动监听 key/mouse 事件；受控切换期 `grab_pointer/grab_keyboard` 隐藏本机输入（ControlDir 联动钩子，M3 只留接口注释+实现）；事件线程 + mpsc（同既有模式）。
2. **注入**：XTEST `fake_input`——**keycode 非 keysym**（docs/05 红线：XSendEvent 被忽略不可用）；keycode = HID usage + 8（evdev 驱动，docs/03 §2）；滚轮按键 4/5/6/7 合成；绝对移动 warp pointer / 相对 move。
3. **keycode-8 校验表**：非门控纯数据区——`hid_to_keycode = hid + 8` 线性关系 + 异常表（docs/03 §2"转换表校验 + 异常表"——异常项以公开 evdev 表核对，源写明）；锚点测试跨平台可跑。
4. **修饰键**：keycode 50/62（Shift）等 → Mods 位；`x11rb` 状态跟踪（XRecord 事件带 state）。
5. **测试**：keycode 校验表全量 + 异常表锚点；转换纯函数；`--target x86_64-unknown-linux-gnu` check 验证。
6. **XRecord 细节**：record context 需要 XRecordRange 指定事件类；_reply 序列化解析（x11rb record 扩展 API 按文档实际签名）。

## Acceptance Criteria

- [ ] `cargo check -p xcontrol-input --features x11 --target x86_64-unknown-linux-gnu` 通过
- [ ] keycode 表锚点 + 异常表测试跨平台可跑（含 +8 关系全量断言）
- [ ] XSendEvent 禁令/XTEST keycode 语义在模块文档钉死（docs/05 逐条）
- [ ] unsafe 全部 SAFETY（x11rb 大部分安全封装，raw XRecord 回调如有则注释）；check-deps/clippy/fmt 全绿
- [ ] 运行时移交清单（X11 机器：捕获/XTEST 注入/grab 隐藏本机输入/XWayland 行为记录）

## Notes

- x11rb record 扩展的回调模型（reply 解析）以 docs.rs 实际 API 为准，不凭记忆。
- grab 隐藏本机输入的完整实现（受控期防双输入）可后置到实测阶段——本任务交付接口 + 文档化行为，实测发现问题时迭代。
