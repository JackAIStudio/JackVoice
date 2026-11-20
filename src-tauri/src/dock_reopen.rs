//! macOS 程序坞（Dock）左键单击与双击分流。
//!
//! - 单击程序坞图标：打开 JackVoice 主设置窗口。
//! - 双击程序坞图标：取消打开设置窗口，直接将实时预览胶囊移到当前屏幕中央并简短闪现。
//!
//! macOS 不会在 `applicationShouldHandleReopen:` 里区分单双击，快速连点两下会连续
//! 派发两次 `RunEvent::Reopen`。这里采用短窗口延迟判定（方案 A）：
//! 第 1 次到达后暂缓一小段窗口；若窗口内收到第 2 次，即判定为双击并取消主窗口弹出；
//! 否则窗口到期后正常弹出主窗口。双击结束后附带极短冷却期，防止三连击尾点误弹设置窗。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::AppHandle;

/// 两次 `Reopen` 在此间隔内视为双击 Dock 图标。
pub const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(320);

/// 双击触发后的极短冷却期：吸收三连击或手抖尾点，避免意外弹出主窗口。
pub const DOUBLE_CLICK_COOLDOWN: Duration = Duration::from_millis(320);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReopenAction {
    /// 首次点击：等待窗口结束后若 token 未变化则打开主窗口。
    ScheduleSingleClick { token: u64 },
    /// 命中双击：取消待定单击，把预览胶囊移到当前屏幕中央并闪现。
    TriggerDoubleClick,
    /// 双击后的极短冷却期内（例如三连击尾点），忽略不弹设置窗。
    Ignore,
}

#[derive(Debug, Clone, Copy)]
struct PendingClick {
    token: u64,
    at: Instant,
}

#[derive(Debug, Default)]
struct ReopenDetector {
    next_token: u64,
    pending: Option<PendingClick>,
    last_double_click_at: Option<Instant>,
}

impl ReopenDetector {
    fn on_reopen(&mut self, now: Instant) -> ReopenAction {
        if let Some(pending) = self.pending {
            if now.saturating_duration_since(pending.at) <= DOUBLE_CLICK_WINDOW {
                self.pending = None;
                self.last_double_click_at = Some(now);
                return ReopenAction::TriggerDoubleClick;
            }
        }

        if let Some(last_dc) = self.last_double_click_at {
            if now.saturating_duration_since(last_dc) <= DOUBLE_CLICK_COOLDOWN {
                self.pending = None;
                return ReopenAction::Ignore;
            }
        }

        self.next_token = self.next_token.wrapping_add(1).max(1);
        let token = self.next_token;
        self.pending = Some(PendingClick { token, at: now });
        ReopenAction::ScheduleSingleClick { token }
    }

    fn consume_pending(&mut self, token: u64) -> bool {
        if self.pending.is_some_and(|pending| pending.token == token) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    fn cancel_pending(&mut self) {
        self.pending = None;
    }
}

static DETECTOR: OnceLock<Mutex<ReopenDetector>> = OnceLock::new();

fn detector() -> &'static Mutex<ReopenDetector> {
    DETECTOR.get_or_init(|| Mutex::new(ReopenDetector::default()))
}

/// 取消尚未到期的程序坞单击待定动作（例如用户已通过其他入口打开设置窗或触发居中）。
pub fn cancel_pending() {
    detector().lock().cancel_pending();
}

/// 处理来自程序坞的 `RunEvent::Reopen`（已排除胶囊自身激活补发的 Reopen）。
pub fn handle_reopen(app: &AppHandle) {
    let now = Instant::now();
    let action = detector().lock().on_reopen(now);

    match action {
        ReopenAction::ScheduleSingleClick { token } => {
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(DOUBLE_CLICK_WINDOW).await;
                if !detector().lock().consume_pending(token) {
                    return;
                }
                let app_for_main = app_handle.clone();
                if let Err(error) = app_handle.run_on_main_thread(move || {
                    // 程序坞点击时 AppKit 已经把应用激活了；这里只负责把窗口
                    // 显示出来并设成 key window。再调 activate() 会把用户刚点
                    // 进来的激活顺序打乱，反而可能落到别的窗口。
                    if let Err(error) = crate::main_window::show_main_window(&app_for_main) {
                        eprintln!("[main-window] Dock 唤起失败：{error}");
                    }
                }) {
                    eprintln!("[main-window] 派发 Dock 单击主线程任务失败：{error}");
                }
            });
        }
        ReopenAction::TriggerDoubleClick => {
            eprintln!("[dock-reopen] 检测到双击程序坞图标，将预览胶囊移到屏幕中央");
            if let Err(error) = crate::overlay::recenter_and_flash_overlay(app.clone()) {
                eprintln!("[dock-reopen] 双击居中预览胶囊失败：{error}");
            }
        }
        ReopenAction::Ignore => {
            eprintln!("[dock-reopen] 忽略双击冷却期内的多余 Reopen");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_click_schedules_and_consumes_when_timer_expires() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        let action = detector.on_reopen(t0);
        let ReopenAction::ScheduleSingleClick { token } = action else {
            panic!("首次点击应排入单击等待，实际：{action:?}");
        };

        assert!(detector.consume_pending(token));
        assert!(!detector.consume_pending(token));
    }

    #[test]
    fn second_click_within_window_triggers_double_click_and_cancels_single() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        let ReopenAction::ScheduleSingleClick { token } = detector.on_reopen(t0) else {
            panic!("首次点击应排入单击等待");
        };

        let second = detector.on_reopen(t0 + Duration::from_millis(180));
        assert_eq!(second, ReopenAction::TriggerDoubleClick);
        // 第 1 次的定时器到期时必须被作废，不能弹出设置窗口。
        assert!(!detector.consume_pending(token));
    }

    #[test]
    fn second_click_at_exact_boundary_still_counts_as_double_click() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        assert!(matches!(
            detector.on_reopen(t0),
            ReopenAction::ScheduleSingleClick { .. }
        ));
        assert_eq!(
            detector.on_reopen(t0 + DOUBLE_CLICK_WINDOW),
            ReopenAction::TriggerDoubleClick
        );
    }

    #[test]
    fn third_click_right_after_double_click_is_ignored_to_prevent_accidental_settings_popup() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        assert!(matches!(
            detector.on_reopen(t0),
            ReopenAction::ScheduleSingleClick { .. }
        ));
        assert_eq!(
            detector.on_reopen(t0 + Duration::from_millis(160)),
            ReopenAction::TriggerDoubleClick
        );
        // 紧跟着的第 3 下在冷却期内，应被吸收而不是打开主设置窗。
        assert_eq!(
            detector.on_reopen(t0 + Duration::from_millis(320)),
            ReopenAction::Ignore
        );
        // 冷却期结束后恢复正常单击识别。
        assert!(matches!(
            detector.on_reopen(t0 + Duration::from_millis(160) + DOUBLE_CLICK_COOLDOWN + Duration::from_millis(10)),
            ReopenAction::ScheduleSingleClick { .. }
        ));
    }

    #[test]
    fn slow_second_click_after_window_starts_a_fresh_single_click() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        let ReopenAction::ScheduleSingleClick { token: first_token } = detector.on_reopen(t0)
        else {
            panic!("首次点击应排入单击等待");
        };
        let ReopenAction::ScheduleSingleClick {
            token: second_token,
        } = detector.on_reopen(t0 + DOUBLE_CLICK_WINDOW + Duration::from_millis(50))
        else {
            panic!("超时后的点击应作为新的单击");
        };

        assert_ne!(first_token, second_token);
        assert!(!detector.consume_pending(first_token));
        assert!(detector.consume_pending(second_token));
    }

    #[test]
    fn cancel_pending_aborts_scheduled_single_click() {
        let mut detector = ReopenDetector::default();
        let t0 = Instant::now();

        let ReopenAction::ScheduleSingleClick { token } = detector.on_reopen(t0) else {
            panic!("首次点击应排入单击等待");
        };
        detector.cancel_pending();
        assert!(!detector.consume_pending(token));
    }
}
