//! 剪贴板层错误类型（error-handling.md §1：thiserror + 可编程区分语义，中文错误文案）。
//!
//! 与"常态非错误"的分工（FR-4.1/FR-4.5）：空剪贴板/无该格式内容是
//! [`crate::FormatRead::NoContent`]、超限是 [`crate::FormatRead::Oversize`] 信号
//! （上层托盘提示）——都不是错误；[`ClipboardError`] 只承载平台 API 真实失败。

/// 剪贴板层错误（可编程区分；文案不含剪贴板内容——内容不写日志/错误，docs/04 §6）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardError {
    /// 平台 API 调用失败（arboard 初始化/读写失败、轮询线程创建失败等）。
    #[error("剪贴板平台 API 调用失败（{action}）：{reason}")]
    Platform {
        /// 失败的动作（clipboard-init/clipboard-write/watcher-spawn/watcher-stop 等）。
        action: &'static str,
        /// 平台侧原因（诊断用，无安全语义；不含剪贴板内容）。
        reason: String,
    },
    /// 图片编解码失败（11-19-m5-clipboard-full，FR-4.2：PNG 解析失败/色型不支持/
    /// 像素长度与宽高不符——platform/png_codec 与平台层 RGBA↔PNG 转换路径）。
    #[error("剪贴板图片编解码失败：{reason}")]
    ImageFormat {
        /// 失败原因（诊断用；不含图片内容本身）。
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误文案可读且不含剪贴板内容（error-handling.md §6；docs/04 §6 不写日志）。
    #[test]
    fn display_is_descriptive() {
        let e = ClipboardError::Platform {
            action: "clipboard-init",
            reason: "NSPasteboard 不可用".into(),
        };
        assert_eq!(
            e.to_string(),
            "剪贴板平台 API 调用失败（clipboard-init）：NSPasteboard 不可用"
        );
        let e = ClipboardError::ImageFormat {
            reason: "PNG 头解析失败".into(),
        };
        assert_eq!(e.to_string(), "剪贴板图片编解码失败：PNG 头解析失败");
    }
}
