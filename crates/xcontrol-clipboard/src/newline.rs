//! 换行规范化（docs/04 §2 线格式行 + §7 单元测试要求）：`text/plain` 线格式
//! **换行统一 LF**，平台本地格式转换在平台层完成——Windows 读写时 LF↔CRLF
//! （写时 `\n`→`\r\n`，读时去 `\r`）；macOS/Linux 原生 LF 直通（不经过本模块）。
//!
//! # 为什么是独立模块（而非内嵌 platform/windows.rs）
//!
//! 纯 std 字符串逻辑、无平台依赖——与 input 的 `winkeymap`/`x11keymap` 先例同款
//! 布局：平台消费的数据变换放在顶层模块，使单测在**所有开发机/CI 平台**可跑
//! （platform/windows.rs 只在 Windows target 编译，其内嵌测试在 mac 开发机上是
//! 编译死区；docs/04 §7 把换行规范化列为单元测试要求，不能只在 Windows CI 覆盖）。
//!
//! # 回环指纹一致性（.trellis/spec rust-platform "Windows 指纹字节教训"）
//!
//! Windows 平台层的写入路径是「先 LF 归一化 → 再 CRLF 落板 → 指纹取**归一化后
//! 的 LF 文本**」：监听回声读回时经同一 [`normalize_to_lf`] 归一化，与写入时的
//! 指纹逐字节一致（round-trip 不动点，见 [`normalize_lf_of_crlf_is_fixed_point`]
//! 测试）——防线 1（内容指纹）在 Windows 上因此保持可用，不依赖防线 2 origin
//! 兜底。若指纹直接按 CRLF 落板字节计算，读回（已归一化 LF）的指纹必然不同，
//! 自写回声会被误判为外部变化（回环风暴起点）。

/// 写侧转换：LF → CRLF（docs/04 §2 Windows 行"写时 `\n`→`\r\n`"）。
///
/// 输入约定为线格式文本（换行已统一 LF）；孤 `\r` 不在写侧契约内、原样保留
/// （写侧不做 `\r` 处理——归一化在读侧/写前的 [`normalize_to_lf`]）。
#[must_use]
pub fn lf_to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' {
            out.push('\r');
        }
        out.push(c);
    }
    out
}

/// 读侧/写前归一化：CRLF 与孤 CR 统一为 LF（docs/04 §2 Windows 行"读时去 `\r`"；
/// "换行统一 LF" 的完整语义含旧 Mac 风格孤 CR，docs/04 §7 测试矩阵 "CRLF/LF/CR
/// 混合"）。
///
/// 读侧：CF_UNICODETEXT 落板字节（Windows 惯例 CRLF）→ 线格式 LF。
/// 写前：防御性归一化（线格式文本本应已是 LF；孤 CR 输入同样收敛，保证写读
/// round-trip 指纹不动点对**任意**输入成立，见模块文档）。
#[must_use]
pub fn normalize_to_lf(text: &str) -> String {
    // 两段替换：先消 CRLF（避免被第二步二次膨胀），再收敛孤 CR。
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::{lf_to_crlf, normalize_to_lf};
    use crate::fingerprint;

    /// LF 直通：写侧转换对纯 LF 文本逐字符原样（每处 \n 前补 \r）。
    #[test]
    fn lf_to_crlf_converts_every_lf() {
        assert_eq!(lf_to_crlf("a\nb"), "a\r\nb");
        assert_eq!(lf_to_crlf("a\nb\nc"), "a\r\nb\r\nc");
        assert_eq!(lf_to_crlf("\n"), "\r\n");
        assert_eq!(lf_to_crlf(""), "", "空串不变");
        assert_eq!(lf_to_crlf("no newline"), "no newline", "无换行不变");
    }

    /// 孤 \r 在写侧原样保留（写侧契约：输入已是线格式 LF 文本）。
    #[test]
    fn lf_to_crlf_keeps_lone_cr() {
        assert_eq!(lf_to_crlf("a\rb"), "a\rb");
    }

    /// 读侧归一化：CRLF → LF（docs/04 §2 "读时去 \r"）。
    #[test]
    fn normalize_to_lf_unifies_crlf() {
        assert_eq!(normalize_to_lf("a\r\nb"), "a\nb");
        assert_eq!(normalize_to_lf("a\r\nb\r\nc"), "a\nb\nc");
    }

    /// 孤 CR 同样收敛为 LF（docs/04 §7 "CRLF/LF/CR 混合"矩阵；旧 Mac 风格换行
    /// 不能静默丢弃——丢弃会把两行并成一行，语义有损）。
    #[test]
    fn normalize_to_lf_unifies_lone_cr() {
        assert_eq!(normalize_to_lf("a\rb"), "a\nb");
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc", "混合行尾全收敛");
        assert_eq!(normalize_to_lf("\r\r"), "\n\n");
        assert_eq!(normalize_to_lf(""), "");
    }

    /// 连续 CRLF（空行）不合并、不重复膨胀：先消 CRLF 再收孤 CR 的两段顺序
    /// 保证 \r\r\n 不会被误判成两个 \r + 一个 \n。
    #[test]
    fn normalize_to_lf_handles_blank_lines() {
        assert_eq!(normalize_to_lf("a\r\n\r\nb"), "a\n\nb");
        assert_eq!(normalize_to_lf("a\r\r\nb"), "a\n\nb");
    }

    /// 回环指纹不动点（模块文档核心断言）：写路径「归一化 → CRLF 落板」与读
    /// 路径「读回 → 归一化」对任意输入收敛到同一文本——防线 1 指纹判据在
    /// Windows 上可用（.trellis/spec "Windows 指纹字节教训"）。
    #[test]
    fn normalize_lf_of_crlf_is_fixed_point() {
        for text in [
            "plain",
            "a\nb",
            "a\r\nb",
            "a\rb",
            "a\r\nb\rc\r\n\rd\n\r",
            "剪贴板 αβγ\n第二行\r\n第三行\rd",
            "",
        ] {
            let normalized = normalize_to_lf(text);
            let stored = lf_to_crlf(&normalized); // 落板字节
            let read_back = normalize_to_lf(&stored); // 读回归一化
            assert_eq!(read_back, normalized, "round-trip 不动点：{text:?}");
            assert_eq!(
                fingerprint(&read_back),
                fingerprint(&normalized),
                "回声指纹一致：{text:?}"
            );
        }
    }
}
