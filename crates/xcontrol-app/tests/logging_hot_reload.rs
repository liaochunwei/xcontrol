//! 日志级别热改集成测试（11-18-m4-ui，docs/06 §4.1 生效时点表第一行）。
//!
//! 独立进程内装配真实 subscriber（`logging::init` + reload 层）→ `set_level`
//! 热改 → 断言 EnvFilter 立即放行/拦截对应级别事件——设置页「日志级别立即生效」
//! 的端到端证据（UI 侧提交链由 ui::tests_ui 覆盖到 RuntimeConfigUpdate 下发，
//! 本测试补上命令消费点之后的过滤行为）。

use xcontrol_app::logging::{self, LogBuffer, LogSettings};
use xcontrol_config::LogLevel;

#[test]
fn set_level_hot_reloads_env_filter() {
    let buffer = LogBuffer::new();
    let guard = logging::init(
        buffer.clone(),
        &LogSettings {
            level: LogLevel::Info,
            file_dir: None,
        },
    )
    .expect("subscriber 装配（独立测试进程，全局唯一）");

    // info 基线：debug 事件被过滤。
    tracing::debug!(target: "app::test", "debug-before-hot");
    assert!(
        !buffer
            .snapshot()
            .iter()
            .any(|line| line.contains("debug-before-hot")),
        "info 级别下 debug 事件必须被过滤"
    );

    // 热改 debug：立即生效（无需重建 subscriber）。
    assert!(logging::set_level(LogLevel::Debug), "热改返回已应用");
    tracing::debug!(target: "app::test", "debug-after-hot");
    assert!(
        buffer
            .snapshot()
            .iter()
            .any(|line| line.contains("debug-after-hot")),
        "热改后 debug 事件立即放行（EnvFilter 重建）"
    );

    // 热改回 info：debug 再次被拦截。
    assert!(logging::set_level(LogLevel::Info));
    tracing::debug!(target: "app::test", "debug-after-revert");
    assert!(
        !buffer
            .snapshot()
            .iter()
            .any(|line| line.contains("debug-after-revert")),
        "热改回 info 后 debug 事件立即被拦截"
    );

    drop(guard);
}
