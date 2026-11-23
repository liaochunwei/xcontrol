# xcontrol-proto fuzz（cargo-fuzz 脚手架）

协议层恶意流 fuzz（docs/02 §8、`.trellis/spec/rust-core/testing.md` fuzz 层）。
**只建不进 CI**：cargo-fuzz 依赖 nightly 工具链，本地按需运行。

## 目标

| target | 覆盖 |
| --- | --- |
| `frame_decode` | 帧解码全路径：超长 len（越界）、截断帧头、垃圾 kind、未知 kind 跳过；三类消息 payload 的强校验路径（值域/布局/postcard 严格消费） |
| `clip_announce` | 剪贴板五消息 + 接收端限额（`ClipAnnounce.total_len` ≤ 16MiB、`ClipChunk.data` ≤ 64KiB），伪造 clip_id / total_len / seq 不 panic（Deskflow CVE-2026-65976 教训） |

断言基线：违规输入**报错或跳过，绝不 panic**（`#![forbid(unsafe_code)]` crate，无 UB 面）。

## 运行（需要 nightly + cargo-fuzz）

```bash
# 安装（一次性）
cargo install cargo-fuzz
rustup toolchain install nightly

# 从本目录（crates/xcontrol-proto/fuzz）运行：
cargo +nightly fuzz run frame_decode -- -max_total_time=300
cargo +nightly fuzz run clip_announce -- -max_total_time=300

# 用仓库内 golden fixtures 做种子（可加快覆盖）：
cargo +nightly fuzz run frame_decode ../tests/fixtures/xcp
```

注意：

- 本目录是**独立 workspace**（`fuzz/Cargo.toml` 末尾 `[workspace]`），主 workspace
  members（`crates/*` glob）不含它，`check-deps` 也不扫描它——libFuzzer/nightly
  依赖不进入主依赖图；
- 首次运行会生成 `Cargo.lock` 与 `artifacts/`（已 gitignore，不入库）；
- 发现 crash 时：把 crash 文件最小化（`cargo fuzz tmin`），修复后在
  `tests/stream.rs` 追加确定性回归用例（fuzz 产物不直接入库）。
