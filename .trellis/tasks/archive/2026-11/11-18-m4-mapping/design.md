# design: 按键映射引擎

```
crates/xcontrol-core/src/mapping.rs   # MappingEngine：规则编译 + 两级状态机 + 预设表
```

## 结构

```rust
pub struct MappingEngine { rules: CompiledRules, passthrough: bool }
pub enum MappingOutput { Inject(Vec<SessionAction-ish InjectEvent>), Drop, Pass }
impl MappingEngine {
    pub fn new(preset: &str, custom: &[KeymapRuleConfig], passthrough: bool) -> Result<Self, MappingError>
    pub fn step(&mut self, ev: &InputEvent) -> Vec<Injection>   // 状态机主入口；Key/KeyRepeat/Modifier 路径分流
    pub fn flush_pending(&mut self) -> Vec<Injection>           // 断开/LeaveScreen 收尾
    pub fn reset(&mut self)                                     // EnterScreen/对齐协同：清 pending + origin 对照表
}
pub struct Injection { mods_down: Vec<XKey>, key: Option<(XKey, KeyState)>, mods_up: Vec<XKey> }
```

- **规则编译**：加载时把 config 规则编译为三类查找结构——`key_remap: Map<XKey, XKey>`（单键）、`mod_map: Map<XKey, XKey>`（事件级修饰）、`combo: Vec<(ComboTrigger, ComboAction)>`（组合级，保序 first-match）；drop 规则并入 combo/key 分支标记。
- **预设**：`PRESETS: &[(&str, &[RuleSpec])]` 常量表（docs/03 §4.4 表逐行）——编译期生成规则列表，`new()` 按 preset 名查表 + 自定义追加（自定义在预设后判定）。
- **两级状态机**：
  - 事件级：`origin_to_mapped: Map<XKey, XKey>`（按下时登记 origin→mapped，释放按 origin 查 mapped 注入 ↑——错序安全）。
  - 组合级：`pending: Option<PendingCombo>`（触发器修饰键按下且存在组合规则时进入）——非修饰键到达查 combo 表；未命中 → flush（pending 修饰键按事件级语义注入）+ 当前键；缓存 Vec 上限 16。
- **热键前置**：引擎入口不变——热键识别在 app 捕获层（主控侧）与注入层之前（从机侧）由调用方完成（M4-hotkeys 任务）；引擎提供 `passthrough()` 查询即可。文档化"热键在映射管线之外"。
- **EnterScreen 协同**：`reset()` 由调用方在收到 EnterScreen 对齐时调用（合成对齐 Key 事件**不过映射**——对端已映射语义，双重映射会错；裁定入模块文档）。
- **Text 分流**：`step(Text)` 原样 Pass（字符路径不过键码映射），文档化。

## 测试矩阵（docs/03 §4 全部 AC）

FR-3.1 序列、错序释放、组合改写、flush 三路径（未命中/全释放/超 16）、8 预设逐规则、自定义叠加与覆盖（preset="none"）、drop、直通、reset 协同、Text 分流。全表驱动（输入序列 → 期望注入序列）。
