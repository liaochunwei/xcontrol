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

    fn resync_cursor(&mut self) -> Result<(), SinkError> {
        (**self).resync_cursor()
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
    /// **端点之外的取值（11-24 回切行程补偿，docs/02 §4.2「反向回报取值范围
    /// 扩展到端点之外」）**：光标被推出屏幕边界期间，度量行程的实现回报端点之外
    /// 的值——`x < 0` / `y < 0` = 推出左/上边界（绝对值 = 自到达该边以来的累计
    /// 行程），`x > 1` / `y > 1` = 推出右/下边界（超出 1 的部分 = 行程）；单位是
    /// 控制方转发的 `MouseMoveRel` delta 单位（即主控的点），光标严格回到屏内
    /// 即清零。控制方仍按 `≤ 0` / `≥ 1` 判极值，行程随回切通知带出用于落点补偿。
    /// 不度量行程的实现维持端点值（越界期间恒报 0.0/1.0，控制方算出的行程为 0）
    /// ——两者对控制方兼容。
    ///
    /// 默认 `None`（无光标语义的实现——mock/无查询 API 的平台）恒不回报，
    /// 调用方（app 会话循环受控光标回报分支）按 `None` 跳过本拍，零额外
    /// 状态；实现方为纯查询（非阻塞快速返回，同 [`InputSink::inject`] 契约）。
    ///
    /// 平台矩阵（11-22；行程度量 11-24）：`UinputInjector`（uinput ABS 虚拟光标
    /// 域换算，**度量行程**）/ `X11Injector`（XQueryPointer root 几何换算）/
    /// `MacInjector`（CGEventGetLocation 布局换算）；win 后端暂无（默认 None，
    /// 回报分支零动作——与被控采样面的平台矩阵一致）。
    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        None
    }

    /// 把真实光标校准到实现的位置模型（保持原位切入后调用一次，11-24）。
    ///
    /// 背景：`EnterScreen{preserve_cursor=1}`（热键/托盘切换）不做落点 warp，
    /// 被控端光标"留在切出时的位置"（docs/02 §4.2）。位置模型为**虚拟**的后端
    /// （uinput——回报取自注入器自己维护的虚拟位置，无"查询真实光标"入口）
    /// 此时虚拟位置与真实光标可能不一致（真实光标被本机物理鼠标挪过等），
    /// 回切判定会基于一个虚构位置——实现覆写本方法，把真实光标定位到虚拟
    /// 位置（虚拟位置不动），使二者重新对齐。
    ///
    /// 默认无操作（`Ok`）：位置模型即真实光标的实现（mac / X11 / win / mock）
    /// 无需校准——调用方对任意 sink 直接调用即可，无需按后端分支。
    /// 契约同 [`InputSink::inject`]：非阻塞快速返回；**不产出输入事件流**
    /// （不触碰已注入按键/按钮的记账）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]（平台调用失败）——实时路径的可忽略错误，调用方
    /// 告警不中断会话（error-handling.md 规则 4）。
    fn resync_cursor(&mut self) -> Result<(), SinkError> {
        Ok(())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// 只实现三个必需方法的 sink——`resync_cursor` 取 trait 默认实现。
    struct RequiredOnly;

    impl InputSink for RequiredOnly {
        fn inject(&mut self, _event: &InputEvent) -> Result<(), SinkError> {
            Ok(())
        }

        fn warp(&mut self, _screen: ScreenId, _x: f32, _y: f32) -> Result<(), SinkError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    /// 覆写 `resync_cursor` 的计数 sink（共享计数——装箱成 trait 对象后仍可读）。
    #[derive(Default)]
    struct CountingSink {
        resyncs: Arc<AtomicU32>,
    }

    impl InputSink for CountingSink {
        fn inject(&mut self, _event: &InputEvent) -> Result<(), SinkError> {
            Ok(())
        }

        fn warp(&mut self, _screen: ScreenId, _x: f32, _y: f32) -> Result<(), SinkError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), SinkError> {
            Ok(())
        }

        fn resync_cursor(&mut self) -> Result<(), SinkError> {
            self.resyncs.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// `resync_cursor` 失败的 sink（错误透传断言用）。
    struct FailingResync;

    impl InputSink for FailingResync {
        fn inject(&mut self, _event: &InputEvent) -> Result<(), SinkError> {
            Ok(())
        }

        fn warp(&mut self, _screen: ScreenId, _x: f32, _y: f32) -> Result<(), SinkError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), SinkError> {
            Ok(())
        }

        fn resync_cursor(&mut self) -> Result<(), SinkError> {
            Err(SinkError::Failed {
                action: "resync",
                reason: "mock".into(),
            })
        }
    }

    /// 默认实现无操作（11-24 位置模型校准钩子）：未覆写的实现调用成功——
    /// 位置模型即真实光标的后端（mac / X11 / win / mock）无需校准。
    #[test]
    fn resync_cursor_default_is_noop() {
        let mut sink = RequiredOnly;
        sink.resync_cursor().expect("默认实现恒成功");
        sink.resync_cursor()
            .expect("重复调用同样成功（幂等无副作用）");
        assert_eq!(sink.cursor_normalized(), None, "既有默认方法不受影响");
    }

    /// Box 透传：`Box<dyn InputSink + Send>`（生产接线形态）的 `resync_cursor`
    /// 到达内层覆写，错误原样上抛（调用方据此告警，error-handling.md 规则 4）。
    #[test]
    fn boxed_sink_forwards_resync_cursor() {
        let inner = CountingSink::default();
        let resyncs = Arc::clone(&inner.resyncs);
        let mut sink: Box<dyn InputSink + Send> = Box::new(inner);
        sink.resync_cursor().expect("透传成功");
        sink.resync_cursor().expect("透传成功");
        assert_eq!(resyncs.load(Ordering::Relaxed), 2, "内层覆写被调两次");

        let mut failing: Box<dyn InputSink + Send> = Box::new(FailingResync);
        assert!(failing.resync_cursor().is_err(), "内层错误原样上抛");
    }
}
