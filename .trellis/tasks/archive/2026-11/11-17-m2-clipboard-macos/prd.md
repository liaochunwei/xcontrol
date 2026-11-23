# M2: macOS 文本剪贴板

## Goal

`xcontrol-clipboard/macos`：文本剪贴板监听与读写（FR-4.1）——复制后 <200ms 可粘贴的跨机同步的 mac 端基石。本任务只做文本（图片/HTML 是 M5 FR-4.2/4.3）。

## Requirements

1. **监听**（docs/04 §4）：`NSPasteboard.changeCount` 轮询 250ms（macOS 无系统剪贴板通知 API，rustdesk 同款 333ms 量级）——后台线程独立运行，检测到变化回调 `ClipboardWatch` 语义（core trait 接线）。
2. **读取**：文本（UTF-8）读取；空剪贴板/非文本内容返回"无文本"而非错误。
3. **写入**：文本写入 + **防回环标记**（docs/04 §5 三防线：写入内容带本机标记 → 自己的 changeCount 轮询识别"刚写入的"不回传）。
4. **上限**：读取时检查协商上限（默认 16MiB，FR-4.5）——超限返回超限信号（托盘提示由 UI 层做，本层只报信号）。
5. **trait 实现**：core 的 `ClipboardWatch` 语义接线（M1 mock 的接口形状为准，若 mock 接口需微调属授权范围）。
6. **feature 门控**：`#[cfg(feature = "macos")]`，`platform/macos.rs` 模块；依赖仅 macos feature 生效。

## Acceptance Criteria

- [ ] 写入→轮询检测→读取 round-trip 测试（本机冒烟，#[ignore] 标注 + 默认可跑的纯逻辑部分分开）
- [ ] 防回环：写入后自己的监听器不触发回传（测试断言）
- [ ] 超限信号路径测试（模拟超长内容）
- [ ] 轮询间隔 250ms 可配；后台线程生命周期可启停（无泄漏）
- [ ] clippy/fmt/test 全绿（macos feature 组合）；check-deps 绿
- [ ] unsafe 块全部 `// SAFETY:` 注释

## Notes

- docs/05 §1 指定 `arboard`（text/image）+ `objc2-app-kit` 补 HTML——本任务只文本：优先 arboard（其内部就是 NSPasteboard）；changeCount 轮询需要原生 API（arboard 不暴露）→ 用 `objc2-app-kit` 或 `objc2` 直接调 NSPasteboard generalPasteboard().changeCount。实现时以 docs.rs 实际 API 为准，不凭记忆。
- 空剪贴板/非文本是常态不是错误（FR-4.1 AC"空剪贴板/超时处理正确"）。
- 轮询线程与 mpsc 桥接同 input crate 模式（回调线程禁 async）。
