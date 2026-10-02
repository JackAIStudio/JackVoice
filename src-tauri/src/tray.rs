use tauri::{
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

pub const MEMOS_WINDOW_LABEL: &str = "memos";
pub const MEMOS_WIN_WIDTH: f64 = 320.0;
pub const MEMOS_WIN_HEIGHT: f64 = 420.0;

// 1位数字 (12x16 点阵)
const GLYPHS_1DIGIT: [[u16; 16]; 10] = [
    [
        0x000, 0x000, 0x1f8, 0x39c, 0x39c, 0x31c, 0x31c, 0x31c, 0x31c, 0x31c, 0x39c, 0x39c, 0x1f8,
        0x0f0, 0x000, 0x000,
    ], // 0
    [
        0x000, 0x000, 0x070, 0x1f0, 0x3f0, 0x3b0, 0x230, 0x030, 0x030, 0x030, 0x030, 0x030, 0x030,
        0x030, 0x000, 0x000,
    ], // 1
    [
        0x000, 0x000, 0x3f8, 0x39c, 0x30c, 0x01c, 0x01c, 0x038, 0x078, 0x0f0, 0x1e0, 0x3c0, 0x3fc,
        0x7fc, 0x000, 0x000,
    ], // 2
    [
        0x000, 0x000, 0x1f8, 0x39c, 0x31c, 0x018, 0x078, 0x078, 0x01c, 0x00c, 0x30c, 0x39c, 0x1f8,
        0x0f0, 0x000, 0x000,
    ], // 3
    [
        0x000, 0x000, 0x038, 0x078, 0x0f8, 0x0f8, 0x198, 0x398, 0x318, 0x7fe, 0x7fe, 0x018, 0x018,
        0x018, 0x000, 0x000,
    ], // 4
    [
        0x000, 0x000, 0x1fc, 0x180, 0x180, 0x3f0, 0x3fc, 0x39c, 0x00c, 0x00c, 0x00c, 0x39c, 0x3f8,
        0x0f0, 0x000, 0x000,
    ], // 5
    [
        0x000, 0x000, 0x1fc, 0x39c, 0x380, 0x3f0, 0x3fc, 0x39c, 0x38c, 0x38c, 0x38c, 0x39c, 0x1f8,
        0x0f0, 0x000, 0x000,
    ], // 6
    [
        0x000, 0x000, 0x3fc, 0x01c, 0x038, 0x030, 0x070, 0x060, 0x0e0, 0x0e0, 0x0c0, 0x0c0, 0x0c0,
        0x1c0, 0x000, 0x000,
    ], // 7
    [
        0x000, 0x000, 0x1f8, 0x39c, 0x31c, 0x39c, 0x1f8, 0x1f8, 0x39c, 0x30c, 0x30c, 0x39c, 0x3f8,
        0x0f0, 0x000, 0x000,
    ], // 8
    [
        0x000, 0x000, 0x1f8, 0x39c, 0x31c, 0x31c, 0x31c, 0x39c, 0x3fc, 0x1fc, 0x01c, 0x398, 0x3f8,
        0x1f0, 0x000, 0x000,
    ], // 9
];

// 2位数字 (7x13 点阵)
const GLYPHS_2DIGIT: [[u8; 13]; 10] = [
    [
        0x00, 0x00, 0x36, 0x32, 0x33, 0x63, 0x63, 0x33, 0x32, 0x36, 0x1c, 0x00, 0x00,
    ], // 0
    [
        0x00, 0x00, 0x1c, 0x3c, 0x2c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x00, 0x00,
    ], // 1
    [
        0x00, 0x00, 0x36, 0x03, 0x07, 0x06, 0x0e, 0x1c, 0x38, 0x30, 0x7f, 0x00, 0x00,
    ], // 2
    [
        0x00, 0x00, 0x36, 0x06, 0x06, 0x0c, 0x06, 0x03, 0x03, 0x36, 0x3c, 0x00, 0x00,
    ], // 3
    [
        0x00, 0x00, 0x0e, 0x0e, 0x1e, 0x16, 0x36, 0x66, 0x7f, 0x06, 0x06, 0x00, 0x00,
    ], // 4
    [
        0x00, 0x00, 0x30, 0x30, 0x3e, 0x36, 0x03, 0x03, 0x03, 0x36, 0x1c, 0x00, 0x00,
    ], // 5
    [
        0x00, 0x00, 0x32, 0x30, 0x30, 0x3e, 0x76, 0x33, 0x33, 0x32, 0x1e, 0x00, 0x00,
    ], // 6
    [
        0x00, 0x00, 0x06, 0x06, 0x0c, 0x0c, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00,
    ], // 7
    [
        0x00, 0x00, 0x36, 0x32, 0x36, 0x1e, 0x36, 0x23, 0x23, 0x36, 0x1c, 0x00, 0x00,
    ], // 8
    [
        0x00, 0x00, 0x36, 0x66, 0x63, 0x37, 0x1f, 0x03, 0x06, 0x36, 0x3c, 0x00, 0x00,
    ], // 9
];

/// 渲染带待办数量镂空数字的菜单栏白板图标 (32x32 RGBA, template 模式下白色底板+黑色镂空数字)
pub fn render_badge_icon(count: usize) -> Image<'static> {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    let mut rgba = vec![0u8; WIDTH * HEIGHT * 4];

    let min_x = 2;
    let max_x = 29;
    let min_y = 2;
    let max_y = 29;
    let radius: f32 = 6.0;

    // 1. 绘制白底圆角便签底板 (完全不透明白色)
    for y in 0..HEIGHT as i32 {
        for x in 0..WIDTH as i32 {
            if x < min_x || x > max_x || y < min_y || y > max_y {
                continue;
            }
            let cx = if (x as f32) < min_x as f32 + radius {
                min_x as f32 + radius
            } else if (x as f32) > max_x as f32 - radius {
                max_x as f32 - radius
            } else {
                x as f32
            };

            let cy = if (y as f32) < min_y as f32 + radius {
                min_y as f32 + radius
            } else if (y as f32) > max_y as f32 - radius {
                max_y as f32 - radius
            } else {
                y as f32
            };

            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= radius * radius {
                let idx = (y as usize * WIDTH + x as usize) * 4;
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
    }

    // 2. 将数字镂空（挖成透明，在 template 模式下自然透出背景底色）
    let val = count.min(99);
    if val <= 9 {
        let glyph = &GLYPHS_1DIGIT[val];
        let ox = (32 - 12) / 2;
        let oy = (32 - 16) / 2;
        for (r, row) in glyph.iter().enumerate() {
            for c in 0..12 {
                if ((row >> (11 - c)) & 1) == 1 {
                    let px = ox + c;
                    let py = oy + r;
                    let idx = (py * WIDTH + px) * 4;
                    rgba[idx] = 0;
                    rgba[idx + 1] = 0;
                    rgba[idx + 2] = 0;
                    rgba[idx + 3] = 0;
                }
            }
        }
    } else {
        let d1 = val / 10;
        let d2 = val % 10;
        let g1 = &GLYPHS_2DIGIT[d1];
        let g2 = &GLYPHS_2DIGIT[d2];
        let ox = (32 - 15) / 2;
        let oy = (32 - 13) / 2;
        for r in 0..13 {
            for c in 0..7 {
                if ((g1[r] >> (6 - c)) & 1) == 1 {
                    let px = ox + c;
                    let py = oy + r;
                    let idx = (py * WIDTH + px) * 4;
                    rgba[idx] = 0;
                    rgba[idx + 1] = 0;
                    rgba[idx + 2] = 0;
                    rgba[idx + 3] = 0;
                }
            }
            for c in 0..7 {
                if ((g2[r] >> (6 - c)) & 1) == 1 {
                    let px = ox + 8 + c;
                    let py = oy + r;
                    let idx = (py * WIDTH + px) * 4;
                    rgba[idx] = 0;
                    rgba[idx + 1] = 0;
                    rgba[idx + 2] = 0;
                    rgba[idx + 3] = 0;
                }
            }
        }
    }

    Image::new_owned(rgba, WIDTH as u32, HEIGHT as u32)
}

/// 更新菜单栏未完成待办数字徽标
pub fn update_tray_badge(app: &AppHandle, uncompleted_count: usize) {
    if let Some(tray) = app.tray_by_id("jackvoice-tray") {
        let icon = render_badge_icon(uncompleted_count);
        let _ = tray.set_icon(Some(icon));
        let _ = tray.set_icon_as_template(true);
        let _ = tray.set_tooltip(Some(format!(
            "JackVoice 待办备忘 ({uncompleted_count} 个待办)"
        )));
    }
}

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

pub fn toggle_memos_window(app: &AppHandle, rect: tauri::Rect) -> Result<(), String> {
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
    let initial_count = crate::memos::uncompleted_count(app);
    let icon = render_badge_icon(initial_count);

    let _tray = TrayIconBuilder::with_id("jackvoice-tray")
        .icon(icon)
        .icon_as_template(true)
        .tooltip(format!("JackVoice 待办备忘 ({initial_count} 个待办)"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_badge_icon_dimensions_and_alpha() {
        for count in [0, 1, 9, 10, 12, 99, 100] {
            let icon = render_badge_icon(count);
            assert_eq!(icon.width(), 32);
            assert_eq!(icon.height(), 32);
            assert_eq!(icon.rgba().len(), 32 * 32 * 4);

            // 验证既有白色底板像素 (alpha = 255)，也有镂空的数字/背景像素 (alpha = 0)
            let has_opaque = icon.rgba().chunks_exact(4).any(|p| p[3] == 255);
            let has_transparent = icon.rgba().chunks_exact(4).any(|p| p[3] == 0);
            assert!(has_opaque, "count {count} 应该包含白底像素");
            assert!(has_transparent, "count {count} 应该包含镂空/透明像素");
        }
    }
}
