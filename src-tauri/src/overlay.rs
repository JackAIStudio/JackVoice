use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const OVERLAY_LABEL: &str = "overlay";
const MAIN_LABEL: &str = "main";
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

fn apply_position(app: &AppHandle, x: f64, y: f64) {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
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
                apply_position(app, x, y);
            }
        }
        // Remember the user's working app before the capsule takes any attention.
        remember_frontmost_app();
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
/// current app, do not activate it again: `activate` can select a different
/// window in multi-window apps and discard the user's original caret.
pub fn hide_overlay_for_delivery(app: &AppHandle, reactivate_target: bool) {
    if reactivate_target {
        hide_overlay(app);
        return;
    }
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.hide();
    }
    previous_frontmost().lock().take();
}

/// Settings/main window is manual-only. Never auto-present it because overlay closed.
static PREVIOUS_FRONTMOST: std::sync::OnceLock<parking_lot::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

fn previous_frontmost() -> &'static parking_lot::Mutex<Option<String>> {
    PREVIOUS_FRONTMOST.get_or_init(|| parking_lot::Mutex::new(None))
}

#[cfg(target_os = "macos")]
fn frontmost_app_name() -> Option<String> {
    use std::process::Command;
    let out = Command::new("osascript")
        .arg("-e")
        .arg(concat!(
            "tell application \"System Events\"\n",
            "set p to first application process whose frontmost is true\n",
            "return (name of p as string) & tab & (count of windows of p as string)\n",
            "end tell"
        ))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let (name, window_count) = raw.trim().rsplit_once('\t')?;
    let window_count = window_count.parse::<usize>().ok()?;
    if name.is_empty() || name.starts_with("JackVoice") || window_count == 0 {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(target_os = "macos")]
fn activate_app(name: &str) {
    use std::process::Command;
    let script = format!("tell application \"{}\" to activate", name.replace('"', ""));
    // Block until activation completes so a subsequent Cmd+V is guaranteed to
    // target this app instead of whatever was frontmost mid-transition.
    let _ = Command::new("osascript").arg("-e").arg(script).status();
}

/// Remember which app the user was working in, before the capsule shows.
pub fn remember_frontmost_app() {
    #[cfg(target_os = "macos")]
    {
        if let Some(name) = current_frontmost_app() {
            *previous_frontmost().lock() = Some(name);
        }
    }
}

/// Read the current non-JackVoice frontmost app without changing the target
/// captured when dictation started.
pub fn current_frontmost_app() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        frontmost_app_name()
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Replace the remembered target only after delivery logic has determined
/// that a deliberate app switch occurred.
pub fn set_remembered_frontmost_app(target: Option<String>) {
    *previous_frontmost().lock() = target;
}

/// Peek (without consuming) the app that was frontmost when the capsule
/// showed — this is the app that will receive the paste.
pub fn remembered_frontmost_app() -> Option<String> {
    previous_frontmost().lock().clone()
}

/// Settings window is manual-only; after the capsule hides, hand focus back
/// to the app the user was actually working in.
pub fn ensure_main_stays_in_background(app: &AppHandle) {
    let _ = app.get_webview_window(MAIN_LABEL);
    #[cfg(target_os = "macos")]
    {
        if let Some(name) = previous_frontmost().lock().take() {
            activate_app(&name);
        }
    }
}

/// Called from frontend drag end to persist capsule position.
pub fn save_overlay_position(app: AppHandle, x: f64, y: f64) -> Result<(), String> {
    save_position(&app, &OverlayPosition { x, y })
}

pub fn reset_overlay_position(app: AppHandle) -> Result<(), String> {
    if let Some(path) = position_path(&app) {
        let _ = fs::remove_file(path);
    }
    if let Some((x, y)) = default_bottom_center(&app) {
        apply_position(&app, x, y);
    }
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
