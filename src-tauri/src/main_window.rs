use tauri::{
    AppHandle, Manager, Runtime, WebviewWindow, WebviewWindowBuilder, Window, WindowEvent,
};

pub const MAIN_LABEL: &str = "main";

fn manages_window(label: &str) -> bool {
    label == MAIN_LABEL
}

/// Create the settings window from the checked-in Tauri configuration when it
/// does not already exist. The normal close path keeps the window alive, but
/// rebuilding here makes every explicit open action recover from an unexpected
/// native/webview destruction as well.
pub fn ensure_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<WebviewWindow<R>, String> {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        return Ok(window);
    }

    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|config| config.label == MAIN_LABEL)
        .ok_or_else(|| "JackVoice 主窗口配置不存在。".to_string())?;

    match WebviewWindowBuilder::from_config(app, config).and_then(|builder| builder.build()) {
        Ok(window) => Ok(window),
        // Two presentation requests can race after an unexpected destruction.
        // If the other request rebuilt the shared label first, use that window.
        Err(build_error) => app
            .get_webview_window(MAIN_LABEL)
            .ok_or_else(|| format!("无法重新创建 JackVoice 主窗口：{build_error}")),
    }
}

/// Present the settings window consistently from Dock reopen, single-instance
/// activation, permission recovery, and frontend commands.
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let window = ensure_main_window(app)?;

    #[cfg(target_os = "macos")]
    app.show()
        .map_err(|error| format!("无法显示 JackVoice 应用：{error}"))?;

    if window
        .is_minimized()
        .map_err(|error| format!("无法读取 JackVoice 主窗口状态：{error}"))?
    {
        window
            .unminimize()
            .map_err(|error| format!("无法恢复 JackVoice 主窗口：{error}"))?;
    }
    window
        .show()
        .map_err(|error| format!("无法显示 JackVoice 主窗口：{error}"))?;
    window
        .set_focus()
        .map_err(|error| format!("无法聚焦 JackVoice 主窗口：{error}"))
}

/// JackVoice is a background dictation app, so the native close button should
/// dismiss settings without destroying the only reusable settings webview.
/// Application-level Quit remains unaffected.
pub fn handle_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if !manages_window(window.label()) {
        return;
    }

    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        if let Err(error) = window.hide() {
            eprintln!("[main-window] 隐藏主窗口失败：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_main_window_uses_hide_on_close_policy() {
        assert!(manages_window(MAIN_LABEL));
        assert!(!manages_window("overlay"));
        assert!(!manages_window("other"));
    }

    #[test]
    fn explicit_open_rebuilds_main_from_config_and_is_idempotent() {
        let app = tauri::test::mock_builder()
            .build(crate::app_context())
            .expect("mock app should build");

        let config = app
            .config()
            .app
            .windows
            .iter()
            .find(|config| config.label == MAIN_LABEL)
            .expect("main config should exist");
        assert!(!config.create, "main must be managed explicitly");
        assert!(app.get_webview_window(MAIN_LABEL).is_none());

        let first = ensure_main_window(app.handle()).expect("main should be created");
        assert_eq!(first.label(), MAIN_LABEL);
        show_main_window(app.handle()).expect("created main should be presentable");

        let second = ensure_main_window(app.handle()).expect("main should be reused");
        assert_eq!(first.label(), second.label());
        assert_eq!(app.webview_windows().len(), 1);
    }

    #[test]
    fn missing_main_config_returns_a_clear_error() {
        let app = tauri::test::mock_app();
        let error = ensure_main_window(app.handle()).expect_err("missing config must fail");
        assert_eq!(error, "JackVoice 主窗口配置不存在。");
    }
}
