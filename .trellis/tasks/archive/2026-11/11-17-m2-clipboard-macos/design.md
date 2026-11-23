# design: macOS 文本剪贴板

## 模块结构（xcontrol-clipboard）

```
src/
├── lib.rs          # ClipboardWatch 语义接线 + feature 声明
├── platform/
│   └── macos.rs    # NSPasteboard changeCount 轮询 + arboard 读写
└── error.rs
```

## 关键决策

- **读取/写入用 arboard**（高层封装稳定），**changeCount 轮询用 objc2-app-kit 原生**（arboard 不暴露 changeCount）——两条 API 线并存，注释注明分工。
- **防回环（docs/04 §5）**：写入时记住"本机最后写入的内容指纹 + changeCount 值"；轮询时 changeCount 变化但内容指纹匹配 → 忽略（是自己的回声）。docs/04 §5 的三防线中防线 2/3（origin 协议字段）在协议层（M1 clip_rx 已有 origin 字段承接），本层防线 1 是本机回声抑制。
- 轮询线程：`std::thread` + `Duration::from_millis(250)` sleep（可配）；`WatchHandle` 支持 stop（AtomicBool flag + join）。
- 超限：读出超 16MiB 文本 → 截断信号返回（不读全量到内存两次——先查长度再决定）。

## 测试策略

- 纯逻辑（指纹/回声判定/超限判定）默认可跑；NSPasteboard 实际读写/轮询为本机冒烟（#[ignore]）。
- trait 接线测试用 core 的 mock 语义对拍。

## 依赖

`arboard`（默认 feature 已含 mac 文本路径）、`objc2-app-kit`（仅 macos feature）——检查传递树（objc2 系无 libc 风险？objc2 纯 bindings crate，verify）；双登记 + clipboard 白名单条目（GATED_CRATES 同 input 待遇）。
