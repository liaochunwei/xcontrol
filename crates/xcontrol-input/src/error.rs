//! 输入层错误类型（error-handling.md §1：thiserror + 可编程区分语义，中文错误文案）。
//!
//! 与 [`crate::SinkError`](xcontrol_core::sink::SinkError) 的分工：
//! - `InputError`——捕获源构建 / 权限检测 / 屏幕几何查询等**本 crate 独有失败**；
//! - `SinkError`——core 定义的注入 trait 错误（route 层按 error-handling.md §4 计数不打断）。

/// 输入层错误（可编程区分；文案不含敏感信息）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// 平台 API 调用失败（CGEventTap 创建失败、CFRunLoopSource 创建失败等）。
    #[error("输入平台 API 调用失败（{action}）：{reason}")]
    Platform {
        /// 失败的动作（tap-create/runloop-source/capture-stop/display-query 等）。
        action: &'static str,
        /// 平台侧原因（诊断用，无安全语义）。
        reason: String,
    },
    /// 辅助功能权限未授予（mac 系统设置 → 隐私与安全性 → 辅助功能；docs/05 §6）。
    ///
    /// 捕获与注入在未授权时被系统**静默忽略**（tap 创建成功但事件不到达、
    /// CGEventPost 不生效）——本变体只由显式权限检测（platform 层
    /// `check_accessibility`）产生，供会话层走 FR-6.3 权限引导。
    #[error("辅助功能权限未授予（系统设置 → 隐私与安全性 → 辅助功能）")]
    AccessibilityDenied,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误文案可读且不含敏感信息（error-handling.md §6）。
    #[test]
    fn display_is_descriptive() {
        let e = InputError::Platform {
            action: "tap-create",
            reason: "null port".into(),
        };
        assert_eq!(
            e.to_string(),
            "输入平台 API 调用失败（tap-create）：null port"
        );
        assert_eq!(
            InputError::AccessibilityDenied.to_string(),
            "辅助功能权限未授予（系统设置 → 隐私与安全性 → 辅助功能）"
        );
    }
}
