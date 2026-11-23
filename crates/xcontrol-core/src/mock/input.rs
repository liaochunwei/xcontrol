//! 脚本化输入源（[`InputSource`](crate::sink::InputSource) 的 mock 实现）。

use std::collections::VecDeque;

use xcontrol_proto::input::{InputEvent, MAX_TEXT_LEN, Mods};

use crate::sink::InputSource;

/// 预设序列输入源：`poll` 非阻塞按序产出全部剩余脚本事件（无事件时 `out` 不变）。
///
/// 用途（docs/08 §4 回环集成测试）：
/// - 常规脚本：按键/修饰键/鼠标/滚轮/文本序列（产出顺序 = 脚本顺序 = 因果顺序）；
/// - 边界注入：[`ScriptedInputSource::long_text`] 把超长文本按 512B 上限在
///   UTF-8 字符边界分段为多条 `Text` 事件（docs/03 §4.5「超长由主控分段」）。
///
/// 键盘与按钮事件严禁合并或重排（docs/02 §4.2）——本源逐条产出，不做任何
/// 合帧；`MouseMoveRel` 的合帧由发送侧按需使用 proto [`RelMerger`]
/// （mock 源保持"脚本即真相"）。
///
/// [`RelMerger`]: xcontrol_proto::input::RelMerger
#[derive(Debug, Clone, Default)]
pub struct ScriptedInputSource {
    script: VecDeque<InputEvent>,
    mods: Mods,
}

impl ScriptedInputSource {
    /// 以事件序列构造（产出顺序 = 迭代顺序）。
    #[must_use]
    pub fn new(script: impl IntoIterator<Item = InputEvent>) -> Self {
        Self {
            script: script.into_iter().collect(),
            mods: Mods::default(),
        }
    }

    /// 空脚本（后经 [`ScriptedInputSource::push`] 追加）。
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// 指定修饰键快照（`EnterScreen`/`LeaveScreen` 载荷来源，docs/03 §3）。
    #[must_use]
    pub fn with_mods(script: impl IntoIterator<Item = InputEvent>, mods: Mods) -> Self {
        Self {
            script: script.into_iter().collect(),
            mods,
        }
    }

    /// 超长文本边界构造器：按 [`MAX_TEXT_LEN`]（512B）上限在 UTF-8 字符边界
    /// 分段为多条 `Text` 事件（docs/02 §4.2 kind 0x07、docs/03 §4.5）。
    #[must_use]
    pub fn long_text(text: &str) -> Self {
        Self::new(
            Self::segment_text(text)
                .into_iter()
                .map(|utf8| InputEvent::Text { utf8 }),
        )
    }

    /// UTF-8 字符边界分段（每段 ≤ [`MAX_TEXT_LEN`] 字节）。
    ///
    /// 不丢字符、不切坏多字节序列：各段拼接严格还原原文；空文本 → 空分段。
    #[must_use]
    pub fn segment_text(text: &str) -> Vec<String> {
        let mut segments = Vec::new();
        let mut start = 0usize;
        let mut len = 0usize;
        for (idx, ch) in text.char_indices() {
            if len + ch.len_utf8() > MAX_TEXT_LEN {
                segments.push(text[start..idx].to_owned());
                start = idx;
                len = ch.len_utf8();
            } else {
                len += ch.len_utf8();
            }
        }
        if start < text.len() {
            segments.push(text[start..].to_owned());
        }
        segments
    }

    /// 追加事件（分批驱动捕获路径用）。
    pub fn push(&mut self, event: InputEvent) {
        self.script.push_back(event);
    }

    /// 剩余未产出事件数。
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.script.len()
    }
}

impl InputSource for ScriptedInputSource {
    fn poll(&mut self, out: &mut Vec<InputEvent>) {
        out.extend(self.script.drain(..));
    }

    fn mods(&self) -> Mods {
        self.mods
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::input::{Axis, MAX_TEXT_LEN as TEXT_LIMIT};

    use super::*;

    /// 分段：ASCII 512B 单段、513B 两段；多字节序列不切坏；拼接严格还原。
    #[test]
    fn segment_text_respects_byte_limit_and_char_boundaries() {
        let ascii = "a".repeat(TEXT_LIMIT);
        let segments = ScriptedInputSource::segment_text(&ascii);
        assert_eq!(segments.len(), 1, "恰好 512B 单段");

        let segments = ScriptedInputSource::segment_text(&"a".repeat(TEXT_LIMIT + 1));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].len(), TEXT_LIMIT);
        assert_eq!(segments[1], "a");

        // 171 个三字节汉字 = 513B：512B 边界处必须回退到字符边界（170 字 = 510B）
        let cjk = "中".repeat(171);
        let segments = ScriptedInputSource::segment_text(&cjk);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], "中".repeat(170));
        assert_eq!(segments[1], "中");

        // 混合内容：全部段落 ≤ 上限且拼接还原（含 4 字节 emoji）
        let mixed = format!("{}字里行间✂️", "a".repeat(TEXT_LIMIT - 3));
        let segments = ScriptedInputSource::segment_text(&mixed);
        assert!(segments.iter().all(|s| s.len() <= TEXT_LIMIT));
        assert_eq!(segments.concat(), mixed);

        assert!(
            ScriptedInputSource::segment_text("").is_empty(),
            "空文本无分段"
        );
    }

    /// long_text 源：分段数与剩余计数一致；poll 一次清空；拼接还原原文。
    #[test]
    fn long_text_source_polls_segments() {
        let text = "x".repeat(1200);
        let mut source = ScriptedInputSource::long_text(&text);
        assert_eq!(source.remaining(), 3, "1200B → 512+512+176 三段");

        let mut batch = Vec::new();
        source.poll(&mut batch);
        assert_eq!(batch.len(), 3);
        assert_eq!(source.remaining(), 0);
        // 无事件时 out 不变（InputSource 契约）
        source.poll(&mut batch);
        assert_eq!(batch.len(), 3);

        assert!(
            batch
                .iter()
                .all(|event| matches!(event, InputEvent::Text { .. }))
        );
        let joined: String = batch
            .iter()
            .map(|event| match event {
                InputEvent::Text { utf8 } => utf8.clone(),
                other => panic!("应为 Text 事件：{other:?}"),
            })
            .collect();
        assert_eq!(joined, text);
    }

    /// push 追加 / with_mods 快照 / new 迭代顺序 = 产出顺序。
    #[test]
    fn push_mods_and_ordering() {
        let mut source = ScriptedInputSource::empty();
        source.push(InputEvent::Wheel {
            axis: Axis::Vertical,
            delta: -1.5,
        });
        source.push(InputEvent::KeyRepeat {
            code: xcontrol_proto::xkey::KEY_C,
        });
        assert_eq!(source.remaining(), 2);

        let mut batch = Vec::new();
        source.poll(&mut batch);
        assert_eq!(batch.len(), 2);
        assert_eq!(source.remaining(), 0);

        let mut scripted = ScriptedInputSource::with_mods(
            [InputEvent::KeyRepeat {
                code: xcontrol_proto::xkey::KEY_A,
            }],
            Mods::LSHIFT.union(Mods::LCTRL),
        );
        assert_eq!(scripted.mods(), Mods::LSHIFT.union(Mods::LCTRL));
        let mut batch = Vec::new();
        scripted.poll(&mut batch);
        assert_eq!(
            batch,
            vec![InputEvent::KeyRepeat {
                code: xcontrol_proto::xkey::KEY_A
            }],
            "产出顺序 = 脚本顺序"
        );
    }
}
