//! 输入注入/捕获抽象 trait（docs/08 §2 命名：`InputSource` 捕获 / `InputSink` 注入）。
//!
//! core 只定义契约（零平台依赖）；实现由平台 crate（macos/windows/x11/wayland，
//! feature 门控）与 `mock-platform`（回环集成测试）提供，`xcontrol-input` 后续
//! re-export 本模块类型（任务 11-16-m1-core prd Notes）。
//!
//! 错误语义（error-handling.md §4）：单次注入失败**不致命**——调用方
//! （[`crate::route::apply_actions`]）计数并继续，不得中断事件流。

use xcontrol_proto::input::{InputEvent, Mods, ScreenId};

/// trait 对象转发（11-20 生产接线修复：`MappingSink<Box<dyn InputSink>>` 需要
/// `Box<dyn InputSink + Send>: InputSink` 才能实例化泛型包装（会话任务 spawn 需 Send）。
impl InputSink for Box<dyn InputSink + Send> {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        (**self).inject(event)
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        (**self).warp(screen, x, y)
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        (**self).release_all()
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        (**self).cursor_normalized()
    }
}

/// 注入错误（可编程区分；不含敏感信息）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SinkError {
    /// 平台注入 API 调用失败（一次性的、可继续的错误）。
    #[error("注入失败（{action}）：{reason}")]
    Failed {
        /// 失败的动作类别（inject/warp/release_all）。
        action: &'static str,
        /// 平台侧原因（诊断用，无安全语义）。
        reason: String,
    },
    /// 平台不支持该事件（如 Wayland 无 Text 注入入口，docs/03 §4.5）。
    #[error("平台不支持：{0}")]
    Unsupported(&'static str),
}

/// 输入注入抽象（协议事件 → 本机 OS 输入）。
///
/// 实现方保证 `inject` 非阻塞快速返回（热路径；平台回调/投递模型自管）。
pub trait InputSink {
    /// 注入一条输入事件（已按映射引擎/快照对齐处理后的最终事件）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]（平台调用失败）/ [`SinkError::Unsupported`]。
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError>;

    /// 指针瞬移（warp）到目标屏幕的归一化坐标（`EnterScreen` 到达边位置；
    /// 物理像素换算由实现按该屏几何+缩放提示完成）。
    ///
    /// # Errors
    /// 同 [`InputSink::inject`]。
    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError>;

    /// 释放本实现注入的全部按键/按钮（切出/暂停/断开防卡键，docs/03 §3）。
    ///
    /// # Errors
    /// 同 [`InputSink::inject`]。
    fn release_all(&mut self) -> Result<(), SinkError>;

    /// 当前光标归一化位置（双向边缘回切的被控方采样面，11-22 修复轮；
    /// docs/03 §5）：`(x, y)` ∈ [0.0, 1.0]²，边缘驻留收敛到端点值 0.0/1.0
    /// （控制方 `at_outer_extreme` 极值判定的回报契约）。
    ///
    /// 默认 `None`（无光标语义的实现——mock/无查询 API 的平台）恒不回报，
    /// 调用方（app 会话循环受控光标回报分支）按 `None` 跳过本拍，零额外
    /// 状态；实现方为纯查询（非阻塞快速返回，同 [`InputSink::inject`] 契约）。
    ///
    /// 平台矩阵（11-22）：`UinputInjector`（uinput ABS 虚拟光标域换算）/
    /// `X11Injector`（XQueryPointer root 几何换算）/ `MacInjector`
    /// （CGEventGetLocation 布局换算）；win 后端暂无（默认 None，回报
    /// 分支零动作——与被控采样面的平台矩阵一致）。
    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        None
    }
}

/// 输入捕获抽象（本机 OS 输入 → 协议事件）。
///
/// 平台实现经回调线程桥接（docs/08 §2：输入平台层不用 async）；主控侧
/// 捕获事件转发前须经过滤（本地热键不转发，docs/03 §4.6）。
pub trait InputSource {
    /// 拉取一批已转换为协议语义的本地输入事件（非阻塞；无事件时 `out` 不变）。
    fn poll(&mut self, out: &mut Vec<InputEvent>);

    /// 当前修饰键快照（`EnterScreen`/`LeaveScreen`/`ModifierSync` 载荷来源）。
    fn mods(&self) -> Mods;
}
