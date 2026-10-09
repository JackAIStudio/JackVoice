pub mod asr;
mod audio;
mod credentials;
mod delivery;
mod dock_menu;
mod history;
mod hotwords;
mod main_window;
mod memos;
mod normalize;
mod onboarding;
mod output_mute;
mod overlay;
mod session;
mod settings;
mod shortcut;
mod storage;
mod tray;
mod volc_hotword_api;

use audio::InputDeviceInfo;
use delivery::DeliveryResult;
use hotwords::ReplacementRule;
use session::{AppState, SaveHotwordsResult, UiState};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{
    Builder as ShortcutBuilder, GlobalShortcutExt, Shortcut, ShortcutState,
};
use tauri_plugin_opener::OpenerExt;

/// 当前已注册的「存入待办」快捷键。全局快捷键插件只有一个 handler，
/// 靠这里区分一次按下是开始听写，还是把剪贴板收入待办。
#[derive(Default)]
struct CaptureShortcutState(parking_lot::Mutex<Option<Shortcut>>);

/// 摘掉「存入待办」快捷键，返回是否本来注册着。
fn release_capture_shortcut(app: &AppHandle) -> bool {
    let Some(state) = app.try_state::<CaptureShortcutState>() else {
        return false;
    };
    let previous = state.0.lock().take();
    match previous {
        Some(shortcut) => {
            let _ = app.global_shortcut().unregister(shortcut);
            true
        }
        None => false,
    }
}

/// 按当前设置注册「存入待办」快捷键。留空表示关闭，只保留托盘菜单入口。
/// 不接受 Fn 组合：Fn 走原生事件监听，不属于全局快捷键插件的管辖范围。
fn register_capture_shortcut(app: &AppHandle) -> Result<(), String> {
    release_capture_shortcut(app);

    let configured = app.state::<AppState>().capture_shortcut();
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if shortcut::uses_fn_modifier(trimmed) {
        return Err("「存入待办」不支持 Fn 组合，请换一个带 ⌘ / ⌥ / ⌃ / ⇧ 的组合。".into());
    }
    let parsed = trimmed
        .parse::<Shortcut>()
        .map_err(|_| "快捷键格式无效，可参考 CommandOrControl+Shift+M 形式。".to_string())?;
    app.global_shortcut()
        .register(parsed)
        .map_err(|error| format!("无法注册该快捷键（可能与其他应用冲突）：{error}"))?;
    if let Some(state) = app.try_state::<CaptureShortcutState>() {
        *state.0.lock() = Some(parsed);
    }
    Ok(())
}

fn is_capture_shortcut(app: &AppHandle, shortcut: &Shortcut) -> bool {
    app.try_state::<CaptureShortcutState>()
        .and_then(|state| *state.0.lock())
        .is_some_and(|registered| registered == *shortcut)
}

/// 两个快捷键是否指向同一个组合。写法可能不同——录制产出 `Meta+Shift+M`，
/// 默认值是 `CommandOrControl+Shift+M`，在 macOS 上按的是同一组键——所以比解析结果。
/// Fn 组合解析不了，而「存入待办」也不接受 Fn，返回 false 即可。
fn same_accelerator(left: &str, right: &str) -> bool {
    match (
        left.trim().parse::<Shortcut>(),
        right.trim().parse::<Shortcut>(),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// 快捷键与托盘菜单共用的收入动作。失败只写日志：这是一次后台动作，
/// 弹窗打断用户比静默失败更糟，待办窗口开着时会自己刷新。
fn file_clipboard_into_memos(app: &AppHandle) {
    let text = match app.clipboard().read_text() {
        Ok(text) => text,
        Err(error) => {
            eprintln!("[memos] 读取剪贴板失败：{error}");
            return;
        }
    };
    match crate::memos::add_memo(app, &text, crate::memos::SOURCE_CLIPBOARD) {
        Ok(_) => eprintln!("[memos] 已从剪贴板收入 1 条待办"),
        Err(error) => eprintln!("[memos] 从剪贴板收入待办失败：{error}"),
    }
}

#[tauri::command]
fn get_state(state: tauri::State<'_, AppState>) -> UiState {
    state.snapshot()
}

#[tauri::command]
fn get_history(state: tauri::State<'_, AppState>) -> history::HistoryData {
    history::load(&state.data_dir())
}

#[tauri::command]
fn delete_history_record(
    record_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<history::HistoryData, String> {
    history::delete_record(&state.data_dir(), record_id.trim())?;
    Ok(history::load(&state.data_dir()))
}

#[tauri::command]
async fn retry_history_recognition(
    record_id: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<history::HistoryData, String> {
    state.retry_history_recognition(app, record_id).await
}

/// 以二进制 IPC 返回 WAV，前端只在用户点击播放时按需读取。
#[tauri::command]
fn get_history_audio(
    record_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let bytes = history::read_audio(&state.data_dir(), record_id.trim())?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
fn reveal_history_audio(
    record_id: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let path = history::audio_path_for_record(&state.data_dir(), record_id.trim())?;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| format!("无法在文件夹中显示音频：{e}"))
}

#[tauri::command]
fn get_hotwords(state: tauri::State<'_, AppState>) -> Vec<String> {
    hotwords::load(&state.data_dir())
}

#[tauri::command]
async fn save_hotwords(
    words: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<SaveHotwordsResult, String> {
    state.save_hotwords_and_maybe_sync(words).await
}

#[tauri::command]
fn get_replacements(state: tauri::State<'_, AppState>) -> Vec<ReplacementRule> {
    hotwords::sanitize_replacements(&hotwords::load_replacements(&state.data_dir()))
}

#[tauri::command]
async fn save_replacements(
    rules: Vec<ReplacementRule>,
    state: tauri::State<'_, AppState>,
) -> Result<SaveHotwordsResult, String> {
    state.save_replacements_and_maybe_sync(rules).await
}

/// 把本地词典同步到当前配置的火山热词表（整表覆盖，权重统一 10）。
#[tauri::command]
async fn sync_volc_hotword_table(state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.sync_volc_hotword_table().await
}

#[tauri::command]
fn update_shortcut(
    shortcut: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    let trimmed = shortcut.trim().to_string();
    // 两个快捷键撞成同一个组合时，注册失败的原因是自己而不是别人，先说清楚。
    if same_accelerator(&trimmed, &state.capture_shortcut()) {
        return Err("这个组合已经用于「存入待办」，请换一个。".into());
    }
    let uses_fn = shortcut::uses_fn_modifier(&trimmed);
    let new_shortcut =
        if uses_fn {
            shortcut::validate_fn_shortcut(&trimmed)?;
            if onboarding::accessibility_is_trusted() {
                shortcut::install_fn_shortcut_monitor(app.clone());
            }
            None
        } else {
            Some(trimmed.parse::<Shortcut>().map_err(|_| {
                "快捷键格式无效，可参考 CommandOrControl+Shift+K 形式。".to_string()
            })?)
        };
    let old_shortcut = state.snapshot().shortcut;
    let global_shortcut = app.global_shortcut();
    let _ = global_shortcut.unregister_all();
    if let Some(new_shortcut) = new_shortcut {
        if let Err(err) = global_shortcut.register(new_shortcut) {
            if !shortcut::uses_fn_modifier(&old_shortcut) {
                if let Ok(old) = old_shortcut.parse::<Shortcut>() {
                    let _ = global_shortcut.register(old);
                }
            }
            return Err(format!("无法注册该快捷键（可能与其他应用冲突）：{err}"));
        }
    }
    match state.set_shortcut(trimmed) {
        Ok(ui) => {
            // unregister_all 会把「存入待办」快捷键一并摘掉，这里补回来。
            if let Err(error) = register_capture_shortcut(&app) {
                eprintln!("[shortcut] {error}");
            }
            Ok(ui)
        }
        Err(error) => {
            let _ = global_shortcut.unregister_all();
            if !shortcut::uses_fn_modifier(&old_shortcut) {
                if let Ok(old) = old_shortcut.parse::<Shortcut>() {
                    let _ = global_shortcut.register(old);
                }
            }
            Err(error)
        }
    }
}

/// 设置「存入待办」的全局快捷键。留空表示关闭，功能仍可从托盘菜单使用。
#[tauri::command]
fn update_capture_shortcut(
    shortcut: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    let trimmed = shortcut.trim().to_string();
    // 留空表示关闭，否则不能和听写快捷键撞车。
    if !trimmed.is_empty() && same_accelerator(&trimmed, &state.shortcut()) {
        return Err("这个组合已经用于免按模式听写，请换一个。".into());
    }
    // 注册函数从 AppState 读取当前值，所以先落盘再注册。
    let ui = state.set_capture_shortcut(trimmed)?;
    if let Err(error) = register_capture_shortcut(&app) {
        // 注册失败就退回关闭状态，界面不会显示一个其实没生效的快捷键。
        let _ = state.set_capture_shortcut(String::new());
        let _ = register_capture_shortcut(&app);
        return Err(error);
    }
    Ok(ui)
}

#[tauri::command]
fn start_shortcut_recording(
    app: AppHandle,
    state: tauri::State<'_, shortcut::ShortcutCaptureState>,
) {
    // 录制期间先摘掉「存入待办」快捷键：用户按下的组合应当被录进去，
    // 而不是顺手把剪贴板存成一条待办。
    release_capture_shortcut(&app);
    // The native event tap is only useful for capturing Fn combinations.
    // Never create it before Accessibility has already been granted: doing so
    // makes macOS show a permission dialog before the onboarding UI is ready.
    if onboarding::accessibility_is_trusted() {
        shortcut::install_fn_shortcut_monitor(app);
    }
    state.start();
}

#[tauri::command]
fn cancel_shortcut_recording(
    app: AppHandle,
    state: tauri::State<'_, shortcut::ShortcutCaptureState>,
) {
    state.stop();
    if let Err(error) = register_capture_shortcut(&app) {
        eprintln!("[shortcut] {error}");
    }
}

#[tauri::command]
fn set_launch_at_login(
    enabled: bool,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch
            .enable()
            .map_err(|e| format!("开启开机启动失败：{e}"))?;
    } else {
        autolaunch
            .disable()
            .map_err(|e| format!("关闭开机启动失败：{e}"))?;
    }
    state.set_launch_at_login_flag(enabled)
}

#[tauri::command]
fn set_mute_system_audio_during_dictation(
    enabled: bool,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.set_mute_system_audio_during_dictation(enabled)
}

#[tauri::command]
fn list_input_devices(state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.refresh_input_devices()
}

#[tauri::command]
fn set_input_device(
    device_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.set_input_device(device_id)
}

#[tauri::command]
fn start_mic_test(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.start_mic_test(app)
}

#[tauri::command]
fn stop_mic_test(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.stop_mic_test(app)
}

#[tauri::command]
async fn save_volc_settings(
    api_key: String,
    resource_id: String,
    boosting_table_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state
        .save_volc_settings(api_key, resource_id, boosting_table_id)
        .await
}

#[tauri::command]
fn remove_volc_api_key(state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.remove_volc_api_key()
}

#[tauri::command]
async fn test_volc_connection(
    api_key: String,
    resource_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    state.test_volc_connection(api_key, resource_id).await
}

#[tauri::command]
fn set_input_gain(gain_db: f32, state: tauri::State<'_, AppState>) -> Result<UiState, String> {
    state.set_input_gain(gain_db)
}

#[tauri::command]
fn set_history_text_size(
    size: String,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.set_history_text_size(size)
}

#[tauri::command]
fn get_permissions() -> onboarding::PermissionStatus {
    onboarding::current_status()
}

#[cfg(target_os = "macos")]
fn open_macos_permission_settings(app: &AppHandle, permission: &str) -> Result<(), String> {
    let (url, label) = match permission {
        "microphone" => (
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
            "麦克风",
        ),
        "accessibility" => (
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            "辅助功能",
        ),
        _ => return Err("不支持的权限设置类型。".into()),
    };
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|error| format!("无法打开系统设置的「{label}」页面：{error}"))
}

#[cfg(not(target_os = "macos"))]
fn open_macos_permission_settings(_app: &AppHandle, _permission: &str) -> Result<(), String> {
    Err("当前平台不支持直接打开 macOS 权限设置。".into())
}

/// Open a narrowly allowlisted macOS privacy pane. This recovery path is
/// essential after a denial because macOS normally does not show the original
/// consent dialog again on later attempts.
#[tauri::command]
fn open_permission_settings(permission: String, app: AppHandle) -> Result<(), String> {
    open_macos_permission_settings(&app, permission.trim())
}

/// Ask macOS to show the Accessibility trust prompt and open System Settings.
/// Returns the (still likely unchanged) Accessibility status right after.
#[tauri::command]
fn request_accessibility_permission(app: AppHandle) -> Result<bool, String> {
    let trusted = onboarding::request_accessibility_prompt();
    if trusted {
        shortcut::install_fn_shortcut_monitor(app);
    } else {
        open_macos_permission_settings(&app, "accessibility")?;
    }
    Ok(trusted)
}

/// Re-read only Accessibility status after the user toggled the switch in
/// System Settings. This must not query microphone authorization: on some
/// macOS versions that query itself can start the microphone consent flow.
#[tauri::command]
fn check_accessibility_permission(app: AppHandle) -> bool {
    let trusted = onboarding::accessibility_is_trusted();
    if trusted {
        shortcut::install_fn_shortcut_monitor(app);
    }
    trusted
}

#[tauri::command]
fn complete_onboarding(
    privacy_confirmed: bool,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    let permissions = onboarding::current_status();
    onboarding::validate_completion(
        permissions.microphone,
        permissions.accessibility,
        privacy_confirmed,
    )?;
    let ui = state.complete_onboarding()?;
    if permissions.accessibility {
        shortcut::install_fn_shortcut_monitor(app);
    }
    Ok(ui)
}

#[tauri::command]
fn update_recognition_options(
    semantic_punctuation_enabled: bool,
    semantic_smoothing_enabled: bool,
    max_sentence_silence_ms: u32,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.update_recognition_options(
        semantic_punctuation_enabled,
        semantic_smoothing_enabled,
        max_sentence_silence_ms,
    )
}

#[tauri::command]
async fn toggle_dictation(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.toggle(app).await
}

#[tauri::command]
async fn cancel_dictation(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UiState, String> {
    state.cancel(app).await
}

#[tauri::command]
fn save_overlay_position(app: AppHandle, x: f64, y: f64) -> Result<(), String> {
    crate::overlay::save_overlay_position(app, x, y)
}

#[tauri::command]
fn reset_overlay_position(app: AppHandle) -> Result<(), String> {
    crate::overlay::reset_overlay_position(app)
}

#[tauri::command]
fn start_overlay_drag(app: AppHandle) -> Result<(), String> {
    crate::overlay::start_overlay_drag(app)
}

/// Re-copy the last dictation result to the clipboard. Used by the manual
/// "copy" button that appears when auto-insertion was not detected.
#[tauri::command]
fn copy_last_transcript(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<DeliveryResult, String> {
    let text = state.snapshot().transcript.trim().to_string();
    if text.is_empty() {
        return Err("没有可复制的文本。".into());
    }
    app.clipboard()
        .write_text(text)
        .map_err(|e| format!("复制到剪贴板失败：{e}"))?;
    Ok(DeliveryResult {
        pasted: false,
        copied: true,
        message: "已复制到剪贴板。".into(),
    })
}

/// Retry delivery without sacrificing whatever is currently on the clipboard.
/// The last transcript lives in JackVoice history/state, so retry can use the
/// same transparent clipboard transaction as first delivery.
#[tauri::command]
async fn retry_last_transcript(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<DeliveryResult, String> {
    let text = state.snapshot().transcript.trim().to_string();
    if text.is_empty() {
        return Err("没有可重试的听写文字。".into());
    }

    let target = crate::overlay::remembered_frontmost_app();
    if let Some(target_app) = target.as_ref() {
        crate::overlay::activate_running_process(target_app.pid);
    }
    crate::overlay::hide_overlay(&app);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The user explicitly requested retry from the capsule; trust their intent
    // and deliver via best-effort paste rather than gating behind AX probes.
    let result = delivery::deliver_text(&app, &text, delivery::InsertionProbe::Unknown).await;
    let ui = state.apply_delivery_result(&result);
    let _ = app.emit("jackvoice://state", ui);
    let _ = app.emit("jackvoice://delivery", result.clone());
    if !result.pasted {
        crate::overlay::show_overlay(&app);
    }
    Ok(result)
}

/// Hide the capsule from the frontend (e.g. after manual copy or timeout).
#[tauri::command]
fn dismiss_overlay(app: AppHandle) -> Result<(), String> {
    crate::overlay::hide_overlay(&app);
    Ok(())
}

#[tauri::command]
fn toggle_session_pin(state: tauri::State<'_, AppState>, app: AppHandle) -> bool {
    let pinned = state.toggle_session_pin();
    let _ = app.emit("jackvoice://state", state.snapshot());
    pinned
}

#[tauri::command]
fn set_session_pin(state: tauri::State<'_, AppState>, app: AppHandle, pinned: bool) -> bool {
    state.set_session_pin(pinned);
    let _ = app.emit("jackvoice://state", state.snapshot());
    pinned
}

#[tauri::command]
fn get_memos(app: AppHandle) -> Vec<crate::memos::MemoItem> {
    crate::memos::list_memos(&app)
}

#[tauri::command]
fn add_memo(
    app: AppHandle,
    text: String,
    source: Option<String>,
) -> Result<crate::memos::MemoItem, String> {
    let source = source.unwrap_or_else(|| crate::memos::SOURCE_MANUAL.to_string());
    crate::memos::add_memo(&app, &text, &source)
}

/// 把剪贴板文字整段收进待办，作为一条。
#[tauri::command]
fn capture_clipboard_to_memo(app: AppHandle) -> Result<crate::memos::MemoItem, String> {
    let text = app
        .clipboard()
        .read_text()
        .map_err(|error| format!("读取剪贴板失败：{error}"))?;
    crate::memos::add_memo(&app, &text, crate::memos::SOURCE_CLIPBOARD)
}

#[tauri::command]
fn toggle_memo(app: AppHandle, id: String) -> Result<Vec<crate::memos::MemoItem>, String> {
    crate::memos::toggle_memo(&app, &id)
}

#[tauri::command]
fn delete_memo(app: AppHandle, id: String) -> Result<Vec<crate::memos::MemoItem>, String> {
    crate::memos::delete_memo(&app, &id)
}

#[tauri::command]
fn clear_completed_memos(app: AppHandle) -> Result<Vec<crate::memos::MemoItem>, String> {
    crate::memos::clear_completed_memos(&app)
}

#[tauri::command]
fn open_settings_window(app: AppHandle) -> Result<(), String> {
    main_window::show_main_window(&app)
}

#[tauri::command]
fn open_external_url(url: String, app: AppHandle) -> Result<(), String> {
    // Only open the official help destinations rendered by the app. Keeping
    // this allowlist here prevents the command from becoming an arbitrary URL
    // launcher if untrusted text ever reaches the webview.
    const VOLC_API_KEY_CONSOLE: &str = "https://console.volcengine.com/speech/new/";
    const VOLC_HOTWORD_GUIDE: &str = "https://www.volcengine.com/docs/6561/155739?lang=zh";
    if !matches!(url.as_str(), VOLC_API_KEY_CONSOLE | VOLC_HOTWORD_GUIDE) {
        return Err("该链接不在 JackVoice 的官方帮助链接列表中。".into());
    }
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|error| format!("无法使用系统浏览器打开链接：{error}"))
}

fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 同一客户端身份重复启动时，唤起已经运行的窗口。正式版和开发版
        // 使用不同 Bundle ID，但共享业务数据，跨版本并行运行由共享锁阻止。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Debug-only diagnostic: ask the already running development
            // instance to validate its in-memory credential without ever
            // printing or forwarding the secret through a second process.
            #[cfg(debug_assertions)]
            if _argv.iter().any(|arg| arg == "--test-volc-connection") {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app.try_state::<AppState>() {
                        match state
                            .test_volc_connection(String::new(), String::new())
                            .await
                        {
                            Ok(message) => eprintln!("[volc-connection-test] {message}"),
                            Err(message) => eprintln!("[volc-connection-test] {message}"),
                        }
                    }
                });
                return;
            }

            // 正在听写时不要抢焦点：否则自动粘贴的目标应用会被设置窗口顶掉。
            let dictating = app
                .try_state::<AppState>()
                .map(|state| state.snapshot().is_live_dictation())
                .unwrap_or(false);
            if dictating {
                eprintln!("[single-instance] 重复实例已忽略（正在听写中，不抢占焦点）");
                return;
            }
            eprintln!("[single-instance] 检测到重复实例，唤起已有 JackVoice 窗口");
            if let Err(error) = main_window::show_main_window(app) {
                eprintln!("[single-instance] {error}");
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
                .build(),
        )
        .plugin(
            ShortcutBuilder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    // 「存入待办」与听写共用一个 handler，先认这次按下的是谁。
                    if is_capture_shortcut(app, shortcut) {
                        let app_handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            file_clipboard_into_memos(&app_handle);
                        });
                        return;
                    }
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(state) = app_handle.try_state::<AppState>() {
                            let _ = state.toggle(app_handle.clone()).await;
                        }
                    });
                })
                .build(),
        )
        .on_window_event(main_window::handle_window_event)
        .setup(|app| {
            let directories = storage::prepare_directories(app.handle())
                .map_err(Box::<dyn std::error::Error>::from)?;
            let instance_guard = match storage::acquire_shared_instance_lock(&directories.shared) {
                Ok(guard) => guard,
                Err(error) if error == storage::INSTANCE_CONFLICT_MESSAGE => {
                    eprintln!("[single-instance] {error}");
                    app.cleanup_before_exit();
                    std::process::exit(0);
                }
                Err(error) => return Err(Box::<dyn std::error::Error>::from(error)),
            };
            let state = AppState::new(
                directories.shared,
                directories.variant,
                directories.production_variant,
                credentials::CredentialMode::from_identifier(&app.config().identifier),
            )
            .map_err(Box::<dyn std::error::Error>::from)?;
            app.manage(instance_guard);
            app.manage(state);
            app.manage(shortcut::ShortcutCaptureState::default());
            app.manage(CaptureShortcutState::default());
            main_window::ensure_main_window(app.handle())
                .map_err(Box::<dyn std::error::Error>::from)?;
            let state = app.state::<AppState>();
            let mut initial_state = state.snapshot();
            let accessibility_trusted = onboarding::accessibility_is_trusted();
            if initial_state.onboarding_completed && !accessibility_trusted {
                initial_state = state
                    .require_onboarding("辅助功能权限已关闭，请重新开启后再继续使用 JackVoice。")
                    .map_err(Box::<dyn std::error::Error>::from)?;
            }
            if shortcut::should_install_fn_monitor_at_startup(
                initial_state.onboarding_completed,
                accessibility_trusted,
            ) {
                shortcut::install_fn_shortcut_monitor(app.handle().clone());
            }
            let _ = crate::overlay::ensure_overlay(app.handle());
            let _ = crate::tray::setup_tray(app.handle());
            let _ = crate::tray::ensure_memos_window(app.handle());
            session::start_pending_recognition_retries(app.handle().clone());

            // First run (onboarding not finished yet): show the main window so
            // the user can walk through the permission setup. The window is
            // otherwise hidden by design (background dictation app).
            if !initial_state.onboarding_completed {
                eprintln!("[onboarding] showing main window for first-run setup");
                match main_window::show_main_window(app.handle()) {
                    Ok(()) => {
                        if let Some(main) = app.get_webview_window(main_window::MAIN_LABEL) {
                            eprintln!("[onboarding] window shown, visible={:?}", main.is_visible());
                            let _ = main.set_position(tauri::LogicalPosition::new(80.0, 80.0));
                        }
                    }
                    Err(error) => eprintln!("[onboarding] show failed: {error}"),
                }
            }

            // Register the user-configured hands-free shortcut (fallback: Alt+Space).
            let saved_shortcut = app.state::<AppState>().snapshot().shortcut;
            let registered = if shortcut::uses_fn_modifier(&saved_shortcut) {
                shortcut::validate_fn_shortcut(&saved_shortcut).is_ok()
            } else {
                saved_shortcut
                    .parse::<Shortcut>()
                    .ok()
                    .and_then(|sc| app.global_shortcut().register(sc).ok())
                    .is_some()
            };
            if !registered {
                app.global_shortcut()
                    .register("Alt+Space")
                    .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            }

            // 「存入待办」快捷键是附加能力：注册失败只记日志，听写不受影响，
            // 托盘菜单里始终保留同一个入口。
            if let Err(error) = register_capture_shortcut(app.handle()) {
                eprintln!("[shortcut] {error}");
            }

            // Keep the OS launch-at-login state in sync with saved settings.
            let want_autostart = app.state::<AppState>().snapshot().launch_at_login;
            let autolaunch = app.autolaunch();
            if let Ok(is_enabled) = autolaunch.is_enabled() {
                if want_autostart && !is_enabled {
                    let _ = autolaunch.enable();
                } else if !want_autostart && is_enabled {
                    let _ = autolaunch.disable();
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_history,
            delete_history_record,
            retry_history_recognition,
            get_history_audio,
            reveal_history_audio,
            get_hotwords,
            save_hotwords,
            get_replacements,
            save_replacements,
            sync_volc_hotword_table,
            update_shortcut,
            update_capture_shortcut,
            start_shortcut_recording,
            cancel_shortcut_recording,
            set_launch_at_login,
            set_mute_system_audio_during_dictation,
            list_input_devices,
            set_input_device,
            start_mic_test,
            stop_mic_test,
            save_volc_settings,
            remove_volc_api_key,
            test_volc_connection,
            set_input_gain,
            set_history_text_size,
            get_permissions,
            open_permission_settings,
            request_accessibility_permission,
            check_accessibility_permission,
            complete_onboarding,
            update_recognition_options,
            toggle_dictation,
            cancel_dictation,
            save_overlay_position,
            reset_overlay_position,
            start_overlay_drag,
            copy_last_transcript,
            retry_last_transcript,
            dismiss_overlay,
            toggle_session_pin,
            set_session_pin,
            get_memos,
            add_memo,
            capture_clipboard_to_memo,
            toggle_memo,
            delete_memo,
            clear_completed_memos,
            open_settings_window,
            open_external_url
        ])
        .build(app_context())
        .expect("error while building JackVoice")
        .run(|app, event| match event {
            // 应用起来、NSApplicationDelegate 就位之后再装程序坞右键菜单。
            tauri::RunEvent::Ready => {
                dock_menu::setup(app);
            }
            // Dock icon click opens settings, except when the live capsule
            // itself caused this activation.
            tauri::RunEvent::Reopen { .. } => {
                if overlay::suppress_reopen_for_overlay(app)
                    || app
                        .try_state::<AppState>()
                        .is_some_and(|state| state.snapshot().is_live_dictation())
                {
                    eprintln!("[main-window] 忽略 Reopen：听写胶囊仍在前台，不打开设置窗口");
                    return;
                }
                if let Err(error) = main_window::show_main_window(app) {
                    eprintln!("[main-window] Dock 唤起失败：{error}");
                }
            }
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => {
                if let Some(state) = app.try_state::<AppState>() {
                    state.restore_output_audio();
                }
            }
            _ => {}
        });
}

// Silence unused import warning if compiler tree-shakes differently.
#[allow(dead_code)]
type _InputDeviceInfo = InputDeviceInfo;

#[cfg(test)]
mod shortcut_identity_tests {
    use super::same_accelerator;

    #[test]
    fn identical_spellings_match() {
        assert!(same_accelerator("Alt+Space", "Alt+Space"));
        assert!(same_accelerator(" Alt+Space ", "Alt+Space"));
    }

    #[test]
    fn different_combinations_do_not_match() {
        assert!(!same_accelerator("Alt+Space", "Alt+Enter"));
        assert!(!same_accelerator(
            "CommandOrControl+Shift+M",
            "CommandOrControl+Shift+K"
        ));
        // 「存入待办」不接受 Fn，只要保证它不会和普通组合误判相等。
        assert!(!same_accelerator("Fn+Space", "Alt+Space"));
    }

    /// 录制产出 `Super`，默认值写 `CommandOrControl`：两者只在 macOS 上指同一个键。
    #[test]
    fn super_and_command_or_control_follow_the_platform() {
        #[cfg(target_os = "macos")]
        assert!(same_accelerator(
            "CommandOrControl+Shift+M",
            "Super+Shift+M"
        ));
        #[cfg(not(target_os = "macos"))]
        assert!(!same_accelerator(
            "CommandOrControl+Shift+M",
            "Super+Shift+M"
        ));
    }

    #[test]
    fn empty_never_matches() {
        assert!(!same_accelerator("", ""));
        assert!(!same_accelerator("Alt+Space", ""));
    }
}
