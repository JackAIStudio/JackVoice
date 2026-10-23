use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::main_window::MAIN_LABEL;

const OVERLAY_LABEL: &str = "overlay";
const OVERLAY_WIDTH: f64 = 376.0;
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
        // `show()` 在 macOS 上是 makeKeyAndOrderFront：胶囊会变成 key window，
        // JackVoice 被激活，用户正在打字的窗口丢掉焦点。收尾时虽然会再激活
        // 目标应用，但中间整段听写期间光标都不在原处，粘贴时机稍有偏差就会
        // 落空。这里把焦点还回去。
        restore_focus_after_overlay_show();
    }
}

/// 胶囊只是提示层，不该抢走用户正在编辑的窗口焦点。
fn restore_focus_after_overlay_show() {
    let snapshot = focus_snapshot().lock();
    let remembered = snapshot.app.clone();
    let main_was_visible = snapshot.main_was_visible;
    drop(snapshot);

    // 用户本来就在 JackVoice 设置窗口里时不要把他踢走。
    if main_was_visible {
        return;
    }
    let Some(target) = remembered else {
        return;
    };
    if frontmost_pid() != Some(std::process::id() as i32) {
        return;
    }
    activate_running_process(target.pid);
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

#[derive(Default)]
struct FocusSnapshot {
    app: Option<FrontmostApp>,
    main_was_visible: bool,
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

/// 当前前台进程的 PID（包含 JackVoice 自己）。
///
/// 只走 NSWorkspace：这是同步的进程内调用。之前前台应用是 JackVoice 时会
/// 回退到 `osascript`，而那条路径没有任何超时；System Events 忙或弹出
/// 自动化授权框时，调用线程会被无限期挂住。
#[cfg(target_os = "macos")]
pub fn frontmost_pid() -> Option<i32> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let front = workspace.frontmostApplication()?;
    let pid = front.processIdentifier();
    (pid > 0).then_some(pid)
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_pid() -> Option<i32> {
    None
}

#[cfg(target_os = "macos")]
fn read_frontmost_app() -> Option<FrontmostApp> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let front = workspace.frontmostApplication()?;
    let pid = front.processIdentifier();
    if pid <= 0 {
        return None;
    }
    let name = front
        .localizedName()
        .map(|n| n.to_string())
        .unwrap_or_default();
    let app = FrontmostApp { pid, name };
    if is_self_app(&app, std::process::id()) {
        None
    } else {
        Some(app)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn activate_running_process(pid: i32) {
    if pid <= 0 {
        return;
    }
    // Primary activation via native NSRunningApplication: macOS resolves
    // directly by PID at the kernel level. This avoids a critical System Events
    // bug where multiple processes sharing a binary name (e.g. WorkBuddy and
    // WorkBuddy AI both having Mach-O name "Electron") cause
    // `whose unix id is {pid}` to incorrectly resolve to the first matching name.
    //
    // 不要带 ActivateAllWindows：它会把目标应用的每个窗口都提到最前，多窗口
    // 应用（Chrome / Finder / 编辑器）里当前那个带光标的窗口会被顶掉，随后
    // 的 Cmd+V 就粘进了另一个窗口。只把应用带回前台即可。
    use objc2_app_kit::NSRunningApplication;
    if let Some(target) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        #[allow(deprecated)]
        let options = objc2_app_kit::NSApplicationActivationOptions::ActivateIgnoringOtherApps;
        if target.activateWithOptions(options) {
            return;
        }
    }

    use std::process::Command;
    // Block until System Events flips `frontmost` so a subsequent Cmd+V lands
    // in this exact process, not a newly launched bundle of the same name.
    // Bounded: a hung System Events must not freeze dictation forever.
    let mut child = match Command::new("osascript")
        .arg("-e")
        .arg(restore_process_script(pid))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("[overlay] 激活目标应用超时，已放弃 osascript 兜底");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn activate_running_process(_pid: i32) {}

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

/// 胶囊自己刚被点过的时间。点击胶囊会让 JackVoice 变成前台应用，macOS
/// 随即补发一个 `Reopen`（无边框 skip-taskbar 面板不算“标准窗口”）。
/// 只有紧跟着胶囊交互的那个 Reopen 才该被吞掉。
static LAST_OVERLAY_INTERACTION: std::sync::OnceLock<
    parking_lot::Mutex<Option<std::time::Instant>>,
> = std::sync::OnceLock::new();

fn overlay_interaction_slot() -> &'static parking_lot::Mutex<Option<std::time::Instant>> {
    LAST_OVERLAY_INTERACTION.get_or_init(|| parking_lot::Mutex::new(None))
}

/// 前端在胶囊上按下鼠标 / 点击按钮时调用，用来标记「这次 Reopen 是自己人」。
pub fn note_overlay_interaction() {
    *overlay_interaction_slot().lock() = Some(std::time::Instant::now());
}

/// 胶囊交互后的抑制窗口。真实 Dock 点击一般发生在胶囊出现后很久，
/// 不会被这个窗口误伤。
const OVERLAY_REOPEN_SUPPRESS_WINDOW: std::time::Duration = std::time::Duration::from_millis(700);

/// Capsule clicks activate JackVoice. macOS then sends `Reopen` as if the
/// Dock icon was clicked, because a borderless skip-taskbar panel is not a
/// standard visible window. Suppress only that immediate follow-up.
///
/// 之前的实现是「胶囊可见且聚焦就一律忽略 Reopen」，于是只要结果胶囊还挂在
/// 屏幕上（未插入时的复制提示会停留 12 秒），用户点程序坞图标就永远没反应。
pub fn suppress_reopen_for_overlay(_app: &AppHandle) -> bool {
    let last = *overlay_interaction_slot().lock();
    should_suppress_reopen(last, std::time::Instant::now())
}

/// 纯判定：只有「距离胶囊交互不超过窗口」的那次 Reopen 才算自己人。
fn should_suppress_reopen(
    last_interaction: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    last_interaction
        .is_some_and(|last| now.saturating_duration_since(last) < OVERLAY_REOPEN_SUPPRESS_WINDOW)
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
    release_windowless_foreground(app);
}

/// 最后一个可见窗口关掉后，macOS 会让应用继续「前台但无窗口」，此时所有
/// 按键都投给一个没有 key window 的应用 —— 表现就是空格键、打字全都没反应，
/// 直到用户手动点一下别的应用。JackVoice 是后台常驻工具，遇到这种情况
/// 必须主动把前台还给上一个应用。
pub fn release_windowless_foreground<R: tauri::Runtime>(app: &AppHandle<R>) {
    #[cfg(target_os = "macos")]
    {
        if !has_visible_windows(app) {
            // 自己不是前台就什么都不用做。
            if frontmost_pid() != Some(std::process::id() as i32) {
                return;
            }
            if let Some(target) = remembered_frontmost_app() {
                activate_running_process(target.pid);
                if frontmost_pid() != Some(std::process::id() as i32) {
                    return;
                }
            }
            // 没有可恢复的目标时，只能把整个应用隐藏起来：macOS 会把激活权
            // 交还给上一个应用。`deactivate()` 在 macOS 14+ 不会归还激活，
            // 实测前台仍停在一个没有窗口的应用上，按键全部丢失。
            use objc2::MainThreadMarker;
            use objc2_app_kit::NSApplication;
            if let Some(mtm) = MainThreadMarker::new() {
                let app = NSApplication::sharedApplication(mtm);
                app.hide(None);
                // 隐藏之后要能被下次 Dock 点击重新唤起；unhide 由
                // show_main_window 负责，这里不做额外处理。
            }
        }
    }
}

/// 是否还有「能让用户继续打字」的窗口。
///
/// 只看 Tauri 自己的窗口列表：AppKit 的 `windows()` 会把已经 orderOut 的
/// 窗口以及各种内部面板都算进来，用它判断几乎永远是“有窗口”。
#[cfg(target_os = "macos")]
fn has_visible_windows<R: tauri::Runtime>(app: &AppHandle<R>) -> bool {
    for (label, window) in app.webview_windows() {
        if label == OVERLAY_LABEL {
            // 胶囊是提示层，不能替用户接收键盘。
            continue;
        }
        if window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false) {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "macos"))]
fn has_visible_windows<R: tauri::Runtime>(_app: &AppHandle<R>) -> bool {
    true
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

    /// Reopen 抑制只认「刚刚发生的胶囊交互」。胶囊挂在屏幕上、或者听写
    /// 还没结束，都不该让程序坞点击失效 —— 那正是「点图标没反应」的成因。
    #[test]
    fn reopen_is_only_suppressed_right_after_a_capsule_interaction() {
        let now = std::time::Instant::now();
        assert!(!should_suppress_reopen(None, now));
        assert!(should_suppress_reopen(Some(now), now));
        assert!(should_suppress_reopen(
            Some(now - OVERLAY_REOPEN_SUPPRESS_WINDOW / 2),
            now
        ));
        assert!(!should_suppress_reopen(
            Some(now - OVERLAY_REOPEN_SUPPRESS_WINDOW),
            now
        ));
        assert!(!should_suppress_reopen(
            Some(now - OVERLAY_REOPEN_SUPPRESS_WINDOW - std::time::Duration::from_secs(5)),
            now
        ));
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

    #[test]
    fn read_frontmost_app_returns_non_self_process_or_none() {
        #[cfg(target_os = "macos")]
        {
            if let Some(app) = read_frontmost_app() {
                assert!(app.pid > 0);
                assert!(!is_self_app(&app, std::process::id()));
            }
        }
    }

    #[test]
    fn activate_running_process_tolerates_invalid_or_nonexistent_pid() {
        activate_running_process(-1);
        activate_running_process(0);
        activate_running_process(999999);
    }
}
