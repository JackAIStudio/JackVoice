use tauri::{
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

pub const MEMOS_WINDOW_LABEL: &str = "memos";
pub const MEMOS_WIN_WIDTH: f64 = 320.0;
pub const MEMOS_WIN_HEIGHT: f64 = 420.0;

pub fn ensure_memos_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(MEMOS_WINDOW_LABEL).is_some() {
        return Ok(());
    }

    let url = WebviewUrl::App("memos.html".into());
    let window = WebviewWindowBuilder::new(app, MEMOS_WINDOW_LABEL, url)
        .title("JackVoice 待办备忘")
        .inner_size(MEMOS_WIN_WIDTH, MEMOS_WIN_HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .shadow(false)
        .build()
        .map_err(|e| format!("创建待办备忘窗口失败：{e}"))?;

    // 失去焦点时自动隐藏窗口
    let win_clone = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            let _ = win_clone.hide();
        }
    });

    Ok(())
}

pub fn toggle_memos_window(
    app: &AppHandle,
    rect: tauri::Rect,
) -> Result<(), String> {
    ensure_memos_window(app)?;

    let window = app
        .get_webview_window(MEMOS_WINDOW_LABEL)
        .ok_or_else(|| "待办备忘窗口未创建".to_string())?;

    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return Ok(());
    }

    // 根据托盘图标位置计算浮窗位置（居中对齐在托盘图标正下方）
    let scale = window.scale_factor().unwrap_or(1.0);
    let icon_x = rect.position.to_logical::<f64>(scale).x;
    let icon_y = rect.position.to_logical::<f64>(scale).y;
    let icon_w = rect.size.to_logical::<f64>(scale).width;
    let icon_h = rect.size.to_logical::<f64>(scale).height;

    let target_x = icon_x + (icon_w / 2.0) - (MEMOS_WIN_WIDTH / 2.0);
    let target_y = icon_y + icon_h + 4.0;

    // 防止超出屏幕边界
    let final_x = target_x.max(8.0);
    let _ = window.set_position(tauri::LogicalPosition::new(final_x, target_y));
    let _ = window.show();
    let _ = window.set_focus();

    Ok(())
}

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let icon_bytes = include_bytes!("../icons/32x32.png");
    let icon = Image::from_bytes(icon_bytes)?;

    let _tray = TrayIconBuilder::with_id("jackvoice-tray")
        .icon(icon)
        .icon_as_template(true)
        .tooltip("JackVoice 待办备忘")
        .on_tray_icon_event(|tray: &tauri::tray::TrayIcon, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                let _ = toggle_memos_window(tray.app_handle(), rect);
            }
        })
        .build(app)?;

    Ok(())
}
