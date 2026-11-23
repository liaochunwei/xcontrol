# M6: i18n 与发布工程

## Goal

FR-6.5 i18n + NFR 基准报告 + 打包脚本 + 文档收口 + 0.1.0 定稿——发布 milestone。

## Requirements

1. **i18n**（FR-6.5）：`tr()` 文案表框架（zh-CN 完整/en 全量翻译）——托盘菜单/设置页/向导/错误信息全部 key 化（audit：grep 中文硬编码字符串）；docs/06 §7 约定（`tr("menu.pause")`）。
2. **基准报告**（NFR-1/2/7）：criterion 数据汇总（M1 编解码/回环 + M4 映射开销）→ `docs/11-benchmarks.md`（基线数字 + 方法论）；性能回归门槛（回环 P50 < 5ms 复测）。
3. **打包脚本**：dmg（hdiutil + .app bundle 构建脚本——codesign 集成）/deb（目录结构 + dpkg-deb）/AppImage（linuxdeploy 或手写 AppDir）——scripts/ 下各平台构建脚本（本机 mac dmg 实测；deb/AppImage SSH 远端实测构建）。
4. **文档收口**：README 状态/安装快速开始/docs/10 校对/各 docs 修订记录完整性 sweep/CHANGELOG.md（0.1.0）。
5. **0.1.0 定稿**：workspace version bump 0.1.0（已是）→ 最终全量验证（build/test/clippy/fmt/llvm-cov 门槛/check-deps/loopback）→ `git tag v0.1.0`（本地 tag，release.yml 触发面就绪——push 后生效）。

## Acceptance Criteria

- [ ] tr() 框架 + zh-CN/en 全量（audit 报告：硬编码清零）
- [ ] docs/11-benchmarks.md（数据 + 方法论 + 达标结论）
- [ ] dmg 脚本本机实测产出 .dmg；deb/AppImage 远端实测产出
- [ ] CHANGELOG.md + README 校对 + 最终全量验证清单全绿
- [ ] v0.1.0 本地 tag 落地；M6 出口条件对照（三平台安装包——dmg 本地 + deb/AppImage 远端 = 两平台实证 + msi 脚本交付）

## Notes

- tr() 框架轻量实现（static table + t(key, lang)），不引重依赖（fluent 等——裁定）。
- msi 打包脚本（WiX）win 无本机环境——脚本交付 + CI release.yml 扩展（移交项）。
