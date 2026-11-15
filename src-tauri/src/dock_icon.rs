//! 有声时大幅波动，无声时静止，断连时显示独立图标；结束后恢复原 logo。
//! 只读取录音状态，不持有麦克风，也不依赖设置窗口或胶囊是否可见。

#[cfg(any(target_os = "macos", test))]
use crate::input_signal::InputSignal;
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(any(target_os = "macos", test))]
const FRAME_COUNT: usize = 8;

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconChange {
    Original,
    Recording(usize),
    Quiet,
    Silent,
    Disconnected,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct RecordingIconState {
    current: Option<IconChange>,
    started_at: Option<Instant>,
    updated_at: Option<Instant>,
}

#[cfg(any(target_os = "macos", test))]
impl RecordingIconState {
    fn update(&mut self, phase: &str, signal: InputSignal, now: Instant) -> Option<IconChange> {
        // 只认本地录音；ASR 连接失败仍可能在录音，整理文字和麦克风测试则不是。
        // 离开录音态立即恢复，不受波形刷新限速影响。
        if phase != "recording" {
            let was_recording = self.current.take().is_some();
            self.started_at = None;
            self.updated_at = None;
            return was_recording.then_some(IconChange::Original);
        }
        let started_at = *self.started_at.get_or_insert(now);
        let next = match signal {
            InputSignal::Sound => {
                let tick = now.saturating_duration_since(started_at).as_millis()
                    / FRAME_INTERVAL.as_millis();
                IconChange::Recording((tick % FRAME_COUNT as u128) as usize)
            }
            InputSignal::Quiet => IconChange::Quiet,
            InputSignal::Silent => IconChange::Silent,
            InputSignal::Disconnected | InputSignal::NoData => IconChange::Disconnected,
        };
        if self.current == Some(next) {
            return None;
        }
        // 只限制连续动画；声音停止、断连、恢复和退出录音都立即响应。
        if matches!(self.current, Some(IconChange::Recording(_)))
            && matches!(next, IconChange::Recording(_))
            && self
                .updated_at
                .is_some_and(|previous| now.duration_since(previous) < FRAME_INTERVAL)
        {
            return None;
        }
        self.updated_at = Some(now);
        self.current = Some(next);
        Some(next)
    }
}

pub fn setup(_app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    imp::setup(_app);
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use objc2::rc::Retained;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSCopying, NSData};
    use tauri::{AppHandle, Listener, Manager};
    use tokio::sync::Notify;

    use super::{IconChange, InputSignal, Instant, RecordingIconState, FRAME_COUNT};
    use crate::session::AppState;

    macro_rules! frames {
        ($variant:literal) => {
            [
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-0.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-1.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-2.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-3.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-4.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-5.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-6.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/recording-7.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/quiet.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/silent.png")).as_slice(),
                include_bytes!(concat!("../dock-icons/", $variant, "/disconnected.png")).as_slice(),
            ]
        };
    }

    const PRODUCTION: [&[u8]; FRAME_COUNT + 3] = frames!("production");
    const DEVELOPMENT: [&[u8]; FRAME_COUNT + 3] = frames!("development");

    struct DockIcon {
        original: Option<Retained<NSImage>>,
        frames: Vec<Retained<NSImage>>,
        state: RecordingIconState,
        trace: bool,
    }

    thread_local! {
        // NSImage 的生命周期和所有 AppKit 操作都留在主线程。
        static ICON: RefCell<Option<DockIcon>> = const { RefCell::new(None) };
    }

    impl DockIcon {
        fn new(app: &AppHandle, mtm: MainThreadMarker) -> Option<Self> {
            let images = if app.config().identifier.ends_with(".dev") {
                &DEVELOPMENT
            } else {
                &PRODUCTION
            };
            let frames = images
                .iter()
                .map(|bytes| NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes)))
                .collect::<Option<Vec<_>>>()?;
            Some(Self {
                // AppKit 更新图标时会复用 applicationIconImage 返回的对象。
                // 只 retain 会让「原图」跟着变成录音帧，必须保存独立副本。
                original: NSApplication::sharedApplication(mtm)
                    .applicationIconImage()
                    .map(|image| image.copy()),
                frames,
                state: RecordingIconState::default(),
                trace: std::env::var_os("JACKVOICE_DOCK_ICON_TRACE").is_some(),
            })
        }

        fn update(&mut self, phase: &str, signal: InputSignal, mtm: MainThreadMarker) {
            let Some(change) = self.state.update(phase, signal, Instant::now()) else {
                return;
            };
            if self.trace {
                eprintln!("[dock-icon] phase={phase} signal={signal:?} change={change:?}");
            }
            let image = match change {
                IconChange::Original => self.original.as_deref(),
                IconChange::Recording(frame) => Some(&*self.frames[frame]),
                IconChange::Quiet => Some(&*self.frames[FRAME_COUNT]),
                IconChange::Silent => Some(&*self.frames[FRAME_COUNT + 1]),
                IconChange::Disconnected => Some(&*self.frames[FRAME_COUNT + 2]),
            };
            // Apple 明确支持 nil 恢复 bundle 原图；有启动图标时直接恢复它，
            // 同时兼容未打包的开发进程。不会修改磁盘上的应用图标。
            unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(image) };
        }
    }

    pub(super) fn setup(app: &AppHandle) {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("[dock-icon] 录音图标需要在主线程初始化");
            return;
        };
        let installed = ICON.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return false;
            }
            *slot = DockIcon::new(app, mtm);
            slot.is_some()
        });
        if !installed {
            return;
        }

        let wake = Arc::new(Notify::new());
        let wake_for_events = wake.clone();
        app.listen("jackvoice://state", move |_| wake_for_events.notify_one());

        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let pending = Arc::new(AtomicBool::new(false));
            loop {
                wake.notified().await;
                if pending.swap(true, Ordering::AcqRel) {
                    continue;
                }
                let app_for_main = app.clone();
                let pending_for_main = pending.clone();
                // state 事件常在 session 的 UI 锁内发出。先经异步任务再切主线程，
                // 不在事件回调内读状态，避免同线程重入死锁。
                if let Err(error) = app.run_on_main_thread(move || {
                    pending_for_main.store(false, Ordering::Release);
                    let Some(mtm) = MainThreadMarker::new() else {
                        return;
                    };
                    let Some(state) = app_for_main.try_state::<AppState>() else {
                        return;
                    };
                    // 执行时读取最新状态：排队中的旧录音事件不能盖掉停止/新会话。
                    let ui = state.snapshot();
                    ICON.with(|slot| {
                        if let Some(icon) = slot.borrow_mut().as_mut() {
                            icon.update(&ui.phase, ui.input_signal, mtm);
                        }
                    });
                }) {
                    eprintln!("[dock-icon] 更新程序坞图标失败：{error}");
                    break;
                }
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bundled_frames_keep_macos_safe_area_and_development_badge() {
            for images in [&PRODUCTION, &DEVELOPMENT] {
                let mut first = None;
                for bytes in images {
                    let image = tauri::image::Image::from_bytes(bytes).unwrap();
                    assert_eq!((image.width(), image.height()), (512, 512));
                    assert_eq!(image.rgba()[3], 0, "透明边角不能变成白底");
                    if let Some(first) = &first {
                        let first: &Vec<u8> = first;
                        assert_ne!(image.rgba(), first);
                        // 声柱之外保持一致，尤其是底部 DEV 标记和 macOS 安全区。
                        assert_eq!(&image.rgba()[384 * 512 * 4..], &first[384 * 512 * 4..]);
                    } else {
                        first = Some(image.rgba().to_vec());
                    }
                }
            }
            assert_ne!(PRODUCTION[0], DEVELOPMENT[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_recording_changes_the_icon() {
        let mut state = RecordingIconState::default();
        let now = Instant::now();
        for phase in ["idle", "starting", "connecting", "finalizing", "error"] {
            assert_eq!(state.update(phase, InputSignal::Sound, now), None);
        }
        assert_eq!(
            state.update("recording", InputSignal::Quiet, now),
            Some(IconChange::Quiet)
        );
    }

    #[test]
    fn sound_uses_large_animation_and_quiet_stays_still() {
        let mut state = RecordingIconState::default();
        let now = Instant::now();
        assert_eq!(
            state.update("recording", InputSignal::Sound, now),
            Some(IconChange::Recording(0))
        );
        assert_eq!(
            state.update(
                "recording",
                InputSignal::Sound,
                now + Duration::from_millis(99)
            ),
            None
        );
        assert_eq!(
            state.update("recording", InputSignal::Sound, now + FRAME_INTERVAL),
            Some(IconChange::Recording(1))
        );
        assert_eq!(
            state.update(
                "recording",
                InputSignal::Quiet,
                now + FRAME_INTERVAL + Duration::from_millis(1)
            ),
            Some(IconChange::Quiet)
        );
        assert_eq!(
            state.update(
                "recording",
                InputSignal::Quiet,
                now + Duration::from_secs(10)
            ),
            None
        );
    }

    #[test]
    fn faults_and_recovery_are_immediate_and_stop_restores_every_state() {
        let now = Instant::now();
        for (signal, icon) in [
            (InputSignal::Quiet, IconChange::Quiet),
            (InputSignal::Silent, IconChange::Silent),
            (InputSignal::Disconnected, IconChange::Disconnected),
            (InputSignal::NoData, IconChange::Disconnected),
        ] {
            let mut state = RecordingIconState::default();
            state.update("recording", InputSignal::Sound, now);
            assert_eq!(
                state.update("recording", signal, now + Duration::from_millis(1)),
                Some(icon)
            );
            assert_eq!(
                state.update(
                    "recording",
                    InputSignal::Sound,
                    now + Duration::from_millis(2)
                ),
                Some(IconChange::Recording(0))
            );
            state.update("recording", signal, now + Duration::from_millis(3));
            assert_eq!(
                state.update("finalizing", signal, now + Duration::from_millis(4)),
                Some(IconChange::Original)
            );
            assert_eq!(
                state.update("idle", signal, now + Duration::from_millis(5)),
                None
            );
        }
    }
}
