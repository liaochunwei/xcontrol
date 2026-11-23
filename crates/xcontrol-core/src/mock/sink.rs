//! 记录型注入 sink（[`InputSink`](crate::sink::InputSink) 的 mock 实现）。

use xcontrol_proto::input::{InputEvent, ScreenId};

use crate::sink::{InputSink, SinkError};

/// 记录全部注入动作供链路断言（T3 tests/chain.rs 简版 RecordingSink 迁移至此）。
///
/// 恒成功——注入失败路径（error-handling.md §4 计数不打断）由 route 层单测的
/// 失败 sink 覆盖；本实现只服务"事件到达"断言。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordingInputSink {
    events: Vec<InputEvent>,
    warps: Vec<(ScreenId, f32, f32)>,
    releases: u32,
}

impl RecordingInputSink {
    /// 空记录。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 到达的注入事件（按到达顺序；docs/02 §4.2 因果顺序即注入顺序）。
    #[must_use]
    pub fn events(&self) -> &[InputEvent] {
        &self.events
    }

    /// warp 记录（`EnterScreen` 到达边位置，docs/03 §5）。
    #[must_use]
    pub fn warps(&self) -> &[(ScreenId, f32, f32)] {
        &self.warps
    }

    /// release_all 次数（切出/暂停/断开防卡键，docs/03 §3）。
    #[must_use]
    pub fn release_count(&self) -> u32 {
        self.releases
    }

    /// 取走全部事件（清空记录；分阶段断言用）。
    pub fn take_events(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.events)
    }

    /// 全部 `Text` 事件按序拼接（超长分段重组断言，docs/03 §4.5）。
    #[must_use]
    pub fn joined_text(&self) -> String {
        let mut joined = String::new();
        for event in &self.events {
            if let InputEvent::Text { utf8 } = event {
                joined.push_str(utf8);
            }
        }
        joined
    }
}

impl InputSink for RecordingInputSink {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        self.events.push(event.clone());
        Ok(())
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        self.warps.push((screen, x, y));
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        self.releases = self.releases.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 事件/warp/释放全记录；joined_text 拼接；take_events 清空。
    #[test]
    fn records_events_warps_and_releases() {
        let mut sink = RecordingInputSink::new();
        sink.inject(&InputEvent::Text { utf8: "he".into() })
            .unwrap();
        sink.inject(&InputEvent::Text { utf8: "llo".into() })
            .unwrap();
        sink.warp(1, 1.0, 0.5).unwrap();
        sink.release_all().unwrap();
        sink.release_all().unwrap();

        assert_eq!(sink.events().len(), 2);
        assert_eq!(sink.joined_text(), "hello", "Text 按到达顺序拼接");
        assert_eq!(sink.warps(), &[(1, 1.0, 0.5)]);
        assert_eq!(sink.release_count(), 2);

        let taken = sink.take_events();
        assert_eq!(taken.len(), 2);
        assert!(sink.events().is_empty(), "take 后清空");
        assert_eq!(sink.joined_text(), "");
    }

    /// 非事件输入事件原样记录（保序，不做任何改写）。
    #[test]
    fn preserves_arrival_order() {
        use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_META};

        let mut sink = RecordingInputSink::new();
        for event in [
            InputEvent::Key {
                code: KEY_LEFT_META,
                state: xcontrol_proto::input::KeyState::Down,
            },
            InputEvent::Key {
                code: KEY_A,
                state: xcontrol_proto::input::KeyState::Down,
            },
            InputEvent::Key {
                code: KEY_A,
                state: xcontrol_proto::input::KeyState::Up,
            },
            InputEvent::Key {
                code: KEY_LEFT_META,
                state: xcontrol_proto::input::KeyState::Up,
            },
        ] {
            sink.inject(&event).unwrap();
        }
        let codes: Vec<(u16, xcontrol_proto::input::KeyState)> = sink
            .events()
            .iter()
            .map(|event| match event {
                InputEvent::Key { code, state } => (code.raw(), *state),
                other => panic!("应为 Key 事件：{other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            vec![
                (KEY_LEFT_META.raw(), xcontrol_proto::input::KeyState::Down),
                (KEY_A.raw(), xcontrol_proto::input::KeyState::Down),
                (KEY_A.raw(), xcontrol_proto::input::KeyState::Up),
                (KEY_LEFT_META.raw(), xcontrol_proto::input::KeyState::Up),
            ],
            "到达顺序严格保持（因果保序断言基准）"
        );
    }
}
