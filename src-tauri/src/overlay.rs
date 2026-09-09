use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::main_window::MAIN_LABEL;

const OVERLAY_LABEL: &str = "overlay";
const OVERLAY_WIDTH: f64 = 340.0;
const OVERLAY_HEIGHT: f64 = 46.0;
const DOCK_GAP: f64 = 14.0;
/// Fallback when we cannot read dock height from work area.
const DEFAULT_DOCK_RESERVE: f64 = 78.0;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct OverlayPosition {
    x: f64,
    y: f64,
}

fn position_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|dir| dir.join("overlay-position.json"))
}

fn load_saved_position(app: &AppHandle) -> Option<OverlayPosition> {
    let path = position_path(app)?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_position(app: &AppHandle, pos: &OverlayPosition) -> Result<(), String> {
    let path = position_path(app).ok_or_else(|| "无法定位配置目录。".to_string())?;
    let raw = serde_json::to_vec_pretty(pos).map_err(|e| e.to_string())?;
    crate::storage::write_atomic(&path, &raw, true)
}

fn default_bottom_center(app: &AppHandle) -> Option<(f64, f64)> {
    let window = app.get_webview_window(OVERLAY_LABEL)?;
    let monitor = window.current_monitor().ok().flatten()?;
    let screen = monitor.size();
    let work = monitor.work_area();
    let scale = monitor.scale_factor();

    let screen_w = screen.width as f64 / scale;
    let screen_h = screen.height as f64 / scale;
    let work_h = work.size.height as f64 / scale;
    let work_y = work.position.y as f64 / scale;

    // Prefer visible work area so we sit just above the Dock, not under it.
    let dock_reserve = (screen_h - (work_y + work_h)).max(0.0);
    let bottom_gap = if dock_reserve > 1.0 {
        dock_reserve + DOCK_GAP
    } else {
        DEFAULT_DOCK_RESERVE
    };

    let x = (screen_w - OVERLAY_WIDTH) / 2.0;
    let y = (screen_h - OVERLAY_HEIGHT - bottom_gap).max(8.0);
    Some((x, y))
}

fn center_on_settings_screen(app: &AppHandle) -> Option<(f64, f64)> {
    let anchor = app
        .get_webview_window(MAIN_LABEL)
        .or_else(|| app.get_webview_window(OVERLAY_LABEL))?;
    let monitor = anchor.current_monitor().ok().flatten()?;
    let work = monitor.work_area();
    let scale = monitor.scale_factor();

    let work_x = work.position.x as f64 / scale;
    let work_y = work.position.y as f64 / scale;
    let work_w = work.size.width as f64 / scale;
    let work_h = work.size.height as f64 / scale;

    Some((
        work_x + (work_w - OVERLAY_WIDTH).max(0.0) / 2.0,
        work_y + (work_h - OVERLAY_HEIGHT).max(0.0) / 2.0,
    ))
}

fn apply_position(app: &AppHandle, x: f64, y: f64) -> Result<(), String> {
    let window = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "实时预览胶囊尚未创建。".to_string())?;
    window
        .set_position(tauri::LogicalPosition::new(x, y))
        .map_err(|e| format!("移动实时预览胶囊失败：{e}"))
}

pub fn ensure_overlay(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(OVERLAY_LABEL).is_some() {
        return Ok(());
    }

    // `App` URLs automatically resolve against Tauri's configured `devUrl`
    // in development and against bundled assets in production. Keeping the
    // overlay on that shared base URL prevents it from silently loading a
    // stale port when the Vite dev server port changes.
    let url = WebviewUrl::App("overlay.html".into());

    let window = WebviewWindowBuilder::new(app, OVERLAY_LABEL, url)
        .title("JackVoice Overlay")
        .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        // Critical on macOS: window shadow creates the weird rectangular corners
        // around a transparent rounded capsule.
        .shadow(false)
        .build()
        .map_err(|e| format!("创建悬浮窗失败：{e}"))?;

    if let Some(saved) = load_saved_position(app) {
        let _ = window.set_position(tauri::LogicalPosition::new(saved.x, saved.y));
    } else if let Some((x, y)) = default_bottom_center(app) {
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }

    Ok(())
}

pub fn show_overlay(app: &AppHandle) {
    if ensure_overlay(app).is_err() {
        return;
    }
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        // If user never dragged it, re-snap above dock each show.
        if load_saved_position(app).is_none() {
            if let Some((x, y)) = default_bottom_center(app) {
                let _ = apply_position(app, x, y);
            }
        }
        // Remember the user's working app before the capsule takes any attention.
        remember_frontmost_app(app);
        let _ = window.show();
        let _ = window.set_always_on_top(true);
    }
}

pub fn hide_overlay(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.hide();
    }
    ensure_main_stays_in_background(app);
}

/// Hide the capsule before delivery. When the chosen target is already the
/// current app, do not activate it again: activating can select a different
/// window in multi-window apps and discard the user's original caret.
pub fn hide_overlay_for_delivery(app: &AppHandle, reactivate_target: bool) {
    if reactivate_target {
        hide_overlay(app);
        return;
    }
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.hide();
    }
    conceal_main_if_unwanted(app);
}

/// The user's working app when dictation started. Identity is the process
/// PID, not the product name: two installs of the same app (dev vs
/// production JackAICut, for example) share a name but never a PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmostApp {
    pub pid: i32,
    pub name: String,
}

struct FocusSnapshot {
    app: Option<FrontmostApp>,
    main_was_visible: bool,
}

impl Default for FocusSnapshot {
    fn default() -> Self {
        Self {
            app: None,
            main_was_visible: false,
        }
    }
}

/// Settings/main window is manual-only. Never auto-present it because overlay closed.
static FOCUS_SNAPSHOT: std::sync::OnceLock<parking_lot::Mutex<FocusSnapshot>> =
    std::sync::OnceLock::new();

fn focus_snapshot() -> &'static parking_lot::Mutex<FocusSnapshot> {
    FOCUS_SNAPSHOT.get_or_init(|| parking_lot::Mutex::new(FocusSnapshot::default()))
}

pub(crate) fn is_jackvoice_process_name(name: &str) -> bool {
    name.to_lowercase().starts_with("jackvoice")
}

pub(crate) fn is_self_app(app: &FrontmostApp, self_pid: u32) -> bool {
    app.pid == self_pid as i32 || is_jackvoice_process_name(&app.name)
}

/// System Events output: `{unix id}\t{process name}`.
pub(crate) fn parse_frontmost_process_line(raw: &str, self_pid: u32) -> Option<FrontmostApp> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (pid_str, name) = raw.split_once('\t')?;
    let pid = pid_str.trim().parse::<i32>().ok()?;
    if pid <= 0 {
        return None;
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let app = FrontmostApp { pid, name };
    if is_self_app(&app, self_pid) {
        None
    } else {
        Some(app)
    }
}

/// Restore the remembered process only when JackVoice currently owns the
/// foreground. `tell application "Name" to activate` is forbidden: Launch
/// Services resolves by product name and can launch a different install of
/// the same app (production JackAICut + DaVinci Resolve while the user was
/// in the development build).
pub(crate) fn should_restore_remembered_app(
    remembered: Option<&FrontmostApp>,
    current: Option<&FrontmostApp>,
    self_pid: u32,
) -> bool {
    let Some(remembered) = remembered else {
        return false;
    };
    if is_self_app(remembered, self_pid) {
        return false;
    }
    match current {
        Some(current) if is_self_app(current, self_pid) => true,
        Some(current) if current.pid == remembered.pid => false,
        Some(_) => false,
        None => true,
    }
}

pub(crate) fn restore_process_script(pid: i32) -> String {
    format!(
        concat!(
            "tell application \"System Events\"\n",
            "try\n",
            "set p to first application process whose unix id is {pid}\n",
            "set frontmost of p to true\n",
            "end try\n",
            "end tell"
        ),
        pid = pid
    )
}

#[cfg(target_os = "macos")]
fn read_frontmost_app() -> Option<FrontmostApp> {
    use std::process::Command;
    let out = Command::new("osascript")
        .arg("-e")
        .arg(concat!(
            "tell application \"System Events\"\n",
            "set p to first application process whose frontmost is true\n",
            "return (unix id of p as string) & tab & (name of p as string)\n",
            "end tell"
        ))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_frontmost_process_line(&String::from_utf8_lossy(&out.stdout), std::process::id())
}

#[cfg(target_os = "macos")]
fn activate_running_process(pid: i32) {
    use std::process::Command;
    // Block until System Events flips `frontmost` so a subsequent Cmd+V lands
    // in this exact process, not a newly launched bundle of the same name.
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(restore_process_script(pid))
        .status();
}

fn main_is_visible(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN_LABEL)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

fn conceal_main_if_unwanted(app: &AppHandle) {
    if focus_snapshot().lock().main_was_visible {
        return;
    }
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        if window.is_visible().ok() == Some(true) {
            let _ = window.hide();
        }
    }
}

/// Capsule clicks activate JackVoice. macOS then sends `Reopen` as if the
/// Dock icon was clicked, because a borderless skip-taskbar panel is not a
/// standard visible window. Suppress that so cancel/confirm never open
/// settings. A genuine Dock click while the capsule is merely on-screen
/// (not focused) still presents settings.
pub fn suppress_reopen_for_overlay(app: &AppHandle) -> bool {
    app.get_webview_window(OVERLAY_LABEL)
        .and_then(|window| {
            let visible = window.is_visible().ok().unwrap_or(false);
            let focused = window.is_focused().ok().unwrap_or(false);
            Some(visible && focused)
        })
        .unwrap_or(false)
}

/// Remember which app the user was working in, before the capsule shows.
pub fn remember_frontmost_app(app: &AppHandle) {
    let mut snapshot = focus_snapshot().lock();
    snapshot.main_was_visible = main_is_visible(app);
    #[cfg(target_os = "macos")]
    if let Some(front) = read_frontmost_app() {
        snapshot.app = Some(front);
    }
}

/// Read the current non-JackVoice frontmost app without changing the target
/// captured when dictation started.
pub fn current_frontmost_app() -> Option<FrontmostApp> {
    #[cfg(target_os = "macos")]
    {
        read_frontmost_app()
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Replace the remembered target only after delivery logic has determined
/// that a deliberate app switch occurred.
pub fn set_remembered_frontmost_app(target: Option<FrontmostApp>) {
    focus_snapshot().lock().app = target;
}

/// Peek the app that was frontmost when the capsule showed — this is the
/// app that will receive the paste.
pub fn remembered_frontmost_app() -> Option<FrontmostApp> {
    focus_snapshot().lock().app.clone()
}

/// Settings window is manual-only; after the capsule hides, hand focus back
/// to the same running process the user was actually working in.
pub fn ensure_main_stays_in_background(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let remembered = remembered_frontmost_app();
        let current = current_frontmost_app();
        if should_restore_remembered_app(remembered.as_ref(), current.as_ref(), std::process::id())
        {
            if let Some(target) = remembered {
                activate_running_process(target.pid);
            }
        }
    }
    conceal_main_if_unwanted(app);
}

/// Called from frontend drag end to persist capsule position.
pub fn save_overlay_position(app: AppHandle, x: f64, y: f64) -> Result<(), String> {
    save_position(&app, &OverlayPosition { x, y })
}

pub fn reset_overlay_position(app: AppHandle) -> Result<(), String> {
    ensure_overlay(&app)?;
    let (x, y) = center_on_settings_screen(&app)
        .ok_or_else(|| "无法获取当前屏幕，请稍后重试。".to_string())?;
    save_position(&app, &OverlayPosition { x, y })?;
    apply_position(&app, x, y)?;
    Ok(())
}

pub fn start_overlay_drag(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        window
            .start_dragging()
            .map_err(|e| format!("开始拖拽失败：{e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(pid: i32, name: &str) -> FrontmostApp {
        FrontmostApp {
            pid,
            name: name.into(),
        }
    }

    #[test]
    fn ignores_jackvoice_and_this_process() {
        assert!(is_jackvoice_process_name("JackVoice"));
        assert!(is_jackvoice_process_name("jackvoice"));
        assert!(is_self_app(&app(42, "Cursor"), 42));
        assert!(is_self_app(&app(99, "JackVoice"), 1));
        assert!(!is_self_app(&app(99, "Cursor"), 1));
    }

    #[test]
    fn parses_pid_and_keeps_apps_with_no_reported_windows() {
        let parsed = parse_frontmost_process_line("12345\tJackAICut for DaVinci Resolve Studio", 1)
            .expect("target app");
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.name, "JackAICut for DaVinci Resolve Studio");
        assert!(parse_frontmost_process_line("1\tJackVoice", 99).is_none());
        assert!(parse_frontmost_process_line("99\tCursor", 99).is_none());
        assert!(parse_frontmost_process_line("", 1).is_none());
    }

    #[test]
    fn cancel_restores_only_when_jackvoice_stole_focus() {
        let jackaicut = app(100, "JackAICut for DaVinci Resolve Studio");
        let chrome = app(200, "Google Chrome");
        let self_pid = 1;

        assert!(should_restore_remembered_app(
            Some(&jackaicut),
            None,
            self_pid
        ));
        assert!(!should_restore_remembered_app(
            Some(&jackaicut),
            Some(&jackaicut),
            self_pid
        ));
        assert!(!should_restore_remembered_app(
            Some(&jackaicut),
            Some(&chrome),
            self_pid
        ));
        assert!(should_restore_remembered_app(
            Some(&jackaicut),
            Some(&app(self_pid as i32, "JackVoice")),
            self_pid
        ));
        assert!(!should_restore_remembered_app(None, None, self_pid));
    }

    #[test]
    fn restore_script_targets_unix_id_and_never_launches_by_name() {
        let script = restore_process_script(12345);
        assert!(script.contains("unix id is 12345"));
        assert!(script.contains("set frontmost of p to true"));
        assert!(!script.contains("to activate"));
        assert!(!script.contains("tell application \"JackAICut"));
        assert!(!script.contains("tell application \"DaVinci"));
    }
}
