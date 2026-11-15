//! 输入信号状态只依赖本地音频和设备通知，不受云端识别是否成功影响。
use serde::Serialize;
use std::time::{Duration, Instant};

/// 低于正常讲话阈值，目的是确认输入有声音，而非判断是否有人说话。
const SOUND_THRESHOLD: f32 = 0.01;
const SOUND_HOLD: Duration = Duration::from_millis(350);
const SILENT_AFTER: Duration = Duration::from_secs(5);
const NO_DATA_AFTER: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InputSignal {
    #[default]
    Quiet,
    Sound,
    Silent,
    Disconnected,
    NoData,
}

#[derive(Debug, Clone, Default)]
pub struct InputActivity {
    started_at: Option<Instant>,
    last_frame: Option<Instant>,
    last_sound: Option<Instant>,
    disconnected: bool,
}

impl InputActivity {
    pub fn connected(&mut self, now: Instant) {
        *self = Self {
            started_at: Some(now),
            ..Self::default()
        };
    }

    pub fn disconnected(&mut self) {
        self.disconnected = true;
    }

    pub fn frame(&mut self, level: f32, now: Instant) {
        // 断连后的队列尾帧不能把设备重新标为可用；只能由设备通知恢复。
        if self.disconnected {
            return;
        }
        self.started_at.get_or_insert(now);
        self.last_frame = Some(now);
        if level.is_finite() && level >= SOUND_THRESHOLD {
            self.last_sound = Some(now);
        }
    }

    pub fn signal(&self, now: Instant) -> InputSignal {
        if self.disconnected {
            return InputSignal::Disconnected;
        }
        let Some(started) = self.started_at else {
            return InputSignal::Quiet;
        };
        if now.saturating_duration_since(self.last_frame.unwrap_or(started)) >= NO_DATA_AFTER {
            return InputSignal::NoData;
        }
        if self
            .last_sound
            .is_some_and(|sound| now.saturating_duration_since(sound) <= SOUND_HOLD)
        {
            return InputSignal::Sound;
        }
        if now.saturating_duration_since(self.last_sound.unwrap_or(started)) >= SILENT_AFTER {
            return InputSignal::Silent;
        }
        InputSignal::Quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_stop_motion_before_the_five_second_notice() {
        let now = Instant::now();
        let mut input = InputActivity::default();
        input.connected(now);
        input.frame(0.02, now);
        assert_eq!(input.signal(now), InputSignal::Sound);
        input.frame(0.0, now + Duration::from_millis(351));
        assert_eq!(
            input.signal(now + Duration::from_millis(351)),
            InputSignal::Quiet
        );
        input.frame(0.0, now + Duration::from_millis(4999));
        assert_eq!(
            input.signal(now + Duration::from_millis(4999)),
            InputSignal::Quiet
        );
        input.frame(0.0, now + SILENT_AFTER);
        assert_eq!(input.signal(now + SILENT_AFTER), InputSignal::Silent);
        input.frame(0.02, now + Duration::from_secs(6));
        assert_eq!(
            input.signal(now + Duration::from_secs(6)),
            InputSignal::Sound
        );
    }

    #[test]
    fn zero_samples_and_missing_samples_are_different() {
        let now = Instant::now();
        let mut input = InputActivity::default();
        input.connected(now);
        assert_eq!(input.signal(now + NO_DATA_AFTER), InputSignal::NoData);
        input.frame(0.0, now + SILENT_AFTER);
        assert_eq!(input.signal(now + SILENT_AFTER), InputSignal::Silent);
    }

    #[test]
    fn disconnect_takes_priority_over_buffered_audio_and_reconnect_resets_warning() {
        let now = Instant::now();
        let mut input = InputActivity::default();
        input.connected(now);
        input.frame(0.7, now);
        input.disconnected();
        input.frame(0.7, now + Duration::from_secs(1));
        assert_eq!(
            input.signal(now + Duration::from_secs(1)),
            InputSignal::Disconnected
        );
        input.connected(now + Duration::from_secs(10));
        assert_eq!(
            input.signal(now + Duration::from_secs(10)),
            InputSignal::Quiet
        );
        input.frame(0.02, now + Duration::from_secs(10));
        assert_eq!(
            input.signal(now + Duration::from_secs(10)),
            InputSignal::Sound
        );
    }

    #[test]
    fn noise_and_nonfinite_levels_do_not_claim_sound() {
        let now = Instant::now();
        for level in [0.001, -1.0, f32::NAN, f32::INFINITY] {
            let mut input = InputActivity::default();
            input.connected(now);
            input.frame(level, now + SILENT_AFTER);
            assert_eq!(input.signal(now + SILENT_AFTER), InputSignal::Silent);
        }
    }
}
