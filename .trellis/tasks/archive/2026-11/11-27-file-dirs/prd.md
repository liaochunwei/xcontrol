# 目录复制：FileExpand 接线（源端枚举+接收端续拉）

## Goal

切片 d 遗留 R1：源端 PullReq 命中目录时有界枚举发 FileExpand，接收端下标空间增长并自动续拉文件子条目——补 US-FT-4。

## Requirements

- TBD

## Acceptance Criteria

- [ ] TBD

## Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
- Lightweight tasks can remain PRD-only.
- For complex tasks, add `design.md` for technical design and `implement.md` for execution planning before `task.py start`.

## 追加（2026-11-27）：wayland 面补齐（24b8b03）

- platform/wayland.rs 文件引用读写实装（text/uri-list mime 直通 + url crate
  percent-encoding；xtask 白名单过审）——linux 目标编译级验证 PASS；
  app 检测/挂板 cfg 门控扩至 wayland。行为实测待 Wayland 实机（docs/05
  §0.2 状态：mac 实测 / wayland 实装待实测 / x11·win 待实装）。
