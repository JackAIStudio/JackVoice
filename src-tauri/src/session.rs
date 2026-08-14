use crate::asr::{RealtimeSession, TranscriptUpdate, VolcAsrConfig, ASR_ENGINE_NAME};
use crate::audio::{self, AudioCapture, InputDeviceInfo, InputDevicePreference};
use crate::credentials::{CredentialMode, CredentialSource};
use crate::delivery::{self, DeliveryResult};
use crate::history::{AudioRecorder, HistoryAppend, RecognitionContext};
use crate::output_mute::OutputMuteGuard;
use crate::settings::{self, AppSettings};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VolcCredentialStatus {
    Missing,
    Configured,
    Verified,
    Failed,
    Unavailable,
}

/// Upper bound for the user-facing digital input gain (dB).
pub const MAX_INPUT_GAIN_DB: f32 = 24.0;
const LOCAL_AUDIO_QUEUE_CAPACITY: usize = 4_096;
const ASR_AUDIO_QUEUE_CAPACITY: usize = 8_192;
const MAX_CONNECT_BACKLOG_BYTES: usize = 2 * 1024 * 1024;
const ASR_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_DICTATION_DURATION: Duration = Duration::from_secs(30 * 60);
const MAX_DICTATION_DURATION_LABEL: &str = "30 分钟";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveHotwordsResult {
    pub words: Vec<String>,
    /// 是否已成功同步到火山云端词表。
    pub synced: bool,
    /// 给人看的状态文案（本地保存 / 同步成功 / 同步跳过 / 同步失败）。
    pub message: String,
    pub sync_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub phase: String,
    /// idle / connecting / streaming / unavailable / error / finalizing。
    /// 录音生命周期由 `phase` 表达，识别状态绝不能反向控制本地录音。
    pub recognition_phase: String,
    pub status: String,
    pub transcript: String,
    pub has_volc_api_key: bool,
    /// 凭据与真实服务连接是两回事：configured 只表示已安全读取，
    /// verified 必须来自本进程内成功的真实连接测试或听写连接。
    pub volc_credential_status: VolcCredentialStatus,
    pub masked_volc_api_key: String,
    pub volc_credential_source: String,
    pub volc_credential_warning: String,
    pub volc_resource_id: String,
    pub volc_boosting_table_id: String,
    pub semantic_punctuation_enabled: bool,
    pub semantic_smoothing_enabled: bool,
    pub max_sentence_silence_ms: u32,
    pub input_gain_db: f32,
    pub selected_input_device_id: String,
    pub selected_input_device_name: String,
    pub selected_input_device_available: bool,
    pub input_devices: Vec<InputDeviceInfo>,
    /// The device currently capturing, or the device that would be selected
    /// if capture started now after the latest device refresh.
    pub active_input_device_id: String,
    pub active_input_device_name: String,
    pub using_input_device_fallback: bool,
    pub audio_level: f32,
    pub mic_testing: bool,
    pub last_delivery_message: String,
    /// True when the last dictation ended without a detected insertion,
    /// so the overlay should offer retry and explicit-copy actions.
    pub needs_copy_prompt: bool,
    /// Transient microphone notice (fallback / disconnect / restore) shown
    /// as a toast on the capsule. `mic_notice_seq` increments on every new
    /// notice so the frontend can re-trigger its timer.
    pub mic_notice: String,
    pub mic_notice_seq: u32,
    pub shortcut: String,
    pub launch_at_login: bool,
    pub mute_system_audio_during_dictation: bool,
    pub system_audio_mute_supported: bool,
    /// Whether the first-run onboarding walkthrough has been completed.
    pub onboarding_completed: bool,
    pub history_text_size: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            recognition_phase: "idle".into(),
            status: "准备就绪。按 Option+Space 开始听写。".into(),
            transcript: String::new(),
            has_volc_api_key: false,
            volc_credential_status: VolcCredentialStatus::Missing,
            masked_volc_api_key: String::new(),
            volc_credential_source: CredentialSource::Missing.as_str().into(),
            volc_credential_warning: String::new(),
            volc_resource_id: "volc.seedasr.sauc.duration".into(),
            volc_boosting_table_id: String::new(),
            semantic_punctuation_enabled: true,
            semantic_smoothing_enabled: false,
            max_sentence_silence_ms: 0,
            input_gain_db: 0.0,
            selected_input_device_id: String::new(),
            selected_input_device_name: String::new(),
            selected_input_device_available: true,
            input_devices: Vec::new(),
            active_input_device_id: String::new(),
            active_input_device_name: String::new(),
            using_input_device_fallback: false,
            audio_level: 0.0,
            mic_testing: false,
            last_delivery_message: String::new(),
            needs_copy_prompt: false,
            mic_notice: String::new(),
            mic_notice_seq: 0,
            shortcut: "Alt+Space".into(),
            launch_at_login: false,
            mute_system_audio_during_dictation: false,
            system_audio_mute_supported: crate::output_mute::supported(),
            onboarding_completed: false,
            history_text_size: "standard".into(),
        }
    }
}

struct ActiveSession {
    id: u64,
    stop_tx: Option<mpsc::Sender<SessionControl>>,
    /// Stop the dedicated audio owner thread (Send).
    audio_stop_tx: Option<std::sync::mpsc::Sender<()>>,
    /// Owns only the mute state applied by this dictation session.
    output_mute: Option<OutputMuteGuard>,
    started_at: Option<Instant>,
    stopping: bool,
}

struct RecordingSession {
    session_id: u64,
    record_id: String,
    recorder: AudioRecorder,
    audio_rx: mpsc::Receiver<Vec<u8>>,
    control_rx: mpsc::Receiver<SessionControl>,
    audio_stop_tx: std::sync::mpsc::Sender<()>,
    settings: AppSettings,
    actual_input_device_id: String,
}

enum SessionControl {
    Stop,
    RecordingFailed(String),
}

enum AsrCommand {
    Audio(Vec<u8>),
    Finish(oneshot::Sender<Result<String, String>>),
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<RealtimeSession, String>> + Send>>;

struct MonitorSession {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
}

#[derive(Default)]
struct MicFallbackState {
    /// Set for one continuous offline episode. Repeated dictations while the
    /// same preferred device is absent must not repeat the same toast.
    unavailable_preference_id: Option<String>,
    lost_active_device_id: Option<String>,
}

pub struct AppState {
    shared_data_dir: PathBuf,
    variant_data_dir: PathBuf,
    credential_mode: CredentialMode,
    settings: Mutex<AppSettings>,
    ui: Mutex<UiState>,
    active: Mutex<Option<ActiveSession>>,
    monitor: Mutex<Option<MonitorSession>>,
    mic_fallback_state: Mutex<MicFallbackState>,
    cancel_requested: Mutex<bool>,
    /// Bumped whenever a dictation session starts, so a scheduled
    /// "hide the error capsule" task never hides a newer session's capsule.
    session_epoch: AtomicU64,
}

impl AppState {
    pub fn new(
        shared_data_dir: PathBuf,
        variant_data_dir: PathBuf,
        production_variant_data_dir: PathBuf,
        credential_mode: CredentialMode,
    ) -> Result<Self, String> {
        let mut settings = settings::load_settings(
            &shared_data_dir,
            &variant_data_dir,
            &production_variant_data_dir,
        )?;
        let legacy_api_key = settings.volc_api_key.trim().to_string();
        let loaded = crate::credentials::load_volc_api_key(credential_mode, &variant_data_dir);
        let mut credential_source = loaded.source;
        let mut credential_warning = loaded.warning;
        let mut legacy_migration_complete = legacy_api_key.is_empty();

        if !loaded.value.is_empty() {
            settings.volc_api_key = loaded.value;
            if !legacy_api_key.is_empty() {
                settings::save_settings(&shared_data_dir, &variant_data_dir, &settings)?;
                legacy_migration_complete = true;
            }
        } else if !legacy_api_key.is_empty() {
            // Very early builds wrote the key to settings.json. Move it to the
            // credential entry for the current build identity. Failure remains
            // recoverable and never aborts startup.
            match crate::credentials::save_volc_api_key(
                credential_mode,
                &variant_data_dir,
                &legacy_api_key,
            ) {
                Ok(source) => {
                    settings.volc_api_key = legacy_api_key;
                    credential_source = source;
                    settings::save_settings(&shared_data_dir, &variant_data_dir, &settings)?;
                    legacy_migration_complete = true;
                }
                Err(error) => {
                    settings.volc_api_key = legacy_api_key;
                    credential_source = CredentialSource::Session;
                    credential_warning =
                        format!("API Key 本次仍可使用，但暂时无法保存到系统凭据库：{error}");
                }
            }
        } else {
            settings.volc_api_key.clear();
        }
        if legacy_migration_complete {
            settings::remove_legacy_settings_backup(&shared_data_dir)?;
        }
        match crate::history::recover_partial_audio_files(&shared_data_dir) {
            Ok(count) if count > 0 => {
                eprintln!("[history] 已恢复 {count} 个异常退出遗留的本地录音")
            }
            Ok(_) => {}
            Err(error) => eprintln!("[history] 恢复未完成录音失败：{error}"),
        }
        match crate::output_mute::restore_stale(&shared_data_dir) {
            Ok(true) => eprintln!("[output-mute] 已恢复上次异常退出遗留的系统音频状态"),
            Ok(false) => {}
            Err(error) => eprintln!("[output-mute] 启动恢复失败：{error}"),
        }
        let mut ui = UiState::default();
        apply_settings_to_ui(&mut ui, &settings);
        ui.volc_credential_source = credential_source.as_str().into();
        ui.volc_credential_warning = credential_warning;
        ui.volc_credential_status =
            initial_volc_credential_status(&settings.volc_api_key, &ui.volc_credential_warning);
        // NOTE: intentionally do NOT enumerate audio devices at startup. On
        // macOS, touching CoreAudio input devices (even just listing them)
        // triggers the system microphone permission prompt. The user should
        // only see that prompt when they reach the mic setup step or start a
        // mic test / dictation, not the moment the app opens. Devices are
        // populated lazily: onboarding mic step, settings, or mic test.
        Ok(Self {
            shared_data_dir,
            variant_data_dir,
            credential_mode,
            settings: Mutex::new(settings),
            ui: Mutex::new(ui),
            active: Mutex::new(None),
            monitor: Mutex::new(None),
            mic_fallback_state: Mutex::new(MicFallbackState::default()),
            cancel_requested: Mutex::new(false),
            session_epoch: AtomicU64::new(0),
        })
    }

    pub fn snapshot(&self) -> UiState {
        self.ui.lock().clone()
    }

    pub fn apply_delivery_result(&self, delivery: &DeliveryResult) -> UiState {
        let mut ui = self.ui.lock();
        ui.needs_copy_prompt = !delivery.pasted;
        ui.last_delivery_message = delivery.message.clone();
        ui.clone()
    }

    pub fn shortcut(&self) -> String {
        self.settings.lock().shortcut.clone()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.shared_data_dir.clone()
    }

    fn save_settings(&self, settings: &AppSettings) -> Result<(), String> {
        settings::save_settings(&self.shared_data_dir, &self.variant_data_dir, settings)
    }

    /// Update runtime device state without mutating the saved preference.
    /// `show_overlay_notice` is false for the settings mic test, where the
    /// same information belongs in the settings status instead of the capsule.
    fn apply_audio_notice(
        &self,
        notice: &audio::AudioNotice,
        show_overlay_notice: bool,
    ) -> UiState {
        let settings = self.settings.lock().clone();
        let preference = input_device_preference(&settings);
        let mut ui = self.ui.lock();
        let mut fallback_state = self.mic_fallback_state.lock();
        let message =
            update_audio_device_state(&mut ui, &mut fallback_state, preference.as_ref(), notice);
        if show_overlay_notice {
            if let Some(message) = message {
                ui.mic_notice = message;
                ui.mic_notice_seq = ui.mic_notice_seq.wrapping_add(1);
            }
        } else if let Some(message) = message {
            ui.status = message;
        }
        ui.clone()
    }

    pub fn refresh_input_devices(&self) -> Result<UiState, String> {
        let devices = AudioCapture::list_input_devices().map_err(|e| e.to_string())?;
        let mut settings = self.settings.lock();
        let mut ui = self.ui.lock();
        ui.input_devices = devices;

        // Upgrade legacy name-based selections to a stable platform ID as
        // soon as that device is online. If it is offline, preserve the old
        // value; a temporary disconnect must never rewrite user intent.
        let selected = find_selected_input_device(&ui.input_devices, &settings);
        let mut settings_changed = false;
        if let Some(device) = selected {
            if settings.selected_input_device_id != device.id {
                settings.selected_input_device_id = device.id.clone();
                settings_changed = true;
            }
            if settings.selected_input_device_name != device.name {
                settings.selected_input_device_name = device.name.clone();
                settings_changed = true;
            }
        } else if !settings.selected_input_device_id.is_empty()
            && settings.selected_input_device_name.is_empty()
        {
            // Older settings stored only a device name. Keep it available for
            // the offline option and for a later stable-ID migration.
            settings.selected_input_device_name = settings.selected_input_device_id.clone();
            settings_changed = true;
        }
        if settings_changed {
            self.save_settings(&settings)?;
        }

        apply_settings_to_ui(&mut ui, &settings);
        sync_resolved_input_device(&mut ui, &settings);
        Ok(ui.clone())
    }

    pub fn set_input_device(&self, device_id: String) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请结束本次听写后再切换麦克风。".into());
        }
        // Switching mic should release any temporary test capture.
        self.stop_mic_test_internal();

        let mut devices = {
            let ui = self.ui.lock();
            ui.input_devices.clone()
        };
        let device_id = device_id.trim().to_string();
        if !device_id.is_empty() && !devices.iter().any(|d| d.id == device_id) {
            // Refresh once in case wireless mic just appeared.
            let refreshed = AudioCapture::list_input_devices().map_err(|e| e.to_string())?;
            if !refreshed.iter().any(|d| d.id == device_id) {
                return Err(format!("找不到麦克风：{device_id}"));
            }
            devices = refreshed;
        }

        let selected_name = devices
            .iter()
            .find(|device| device.id == device_id)
            .map(|device| device.name.clone())
            .unwrap_or_default();
        let mut settings = self.settings.lock();
        settings.selected_input_device_id = device_id;
        settings.selected_input_device_name = selected_name;
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        ui.input_devices = devices;
        apply_settings_to_ui(&mut ui, &settings);
        sync_resolved_input_device(&mut ui, &settings);
        self.mic_fallback_state.lock().unavailable_preference_id = None;
        ui.status = if settings.selected_input_device_id.is_empty() {
            "已设为自动选择，将跟随系统默认麦克风。".into()
        } else {
            format!("已优先选择麦克风：{}", settings.selected_input_device_name)
        };
        Ok(ui.clone())
    }

    pub fn start_mic_test(&self, app: AppHandle) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再测试麦克风。".into());
        }
        if self.monitor.lock().is_some() {
            return Ok(self.snapshot());
        }

        // The device list may be empty on a fresh launch (we no longer
        // enumerate at startup). This is also the user-initiated moment where
        // the TCC prompt may just have been granted, so refresh the list so
        // the real microphone shows up. Best-effort: failure to list devices
        // shouldn't block the test itself.
        let _ = self.refresh_input_devices();

        let (selected_device, mic_test_gain_db) = {
            let settings = self.settings.lock();
            (input_device_preference(&settings), settings.input_gain_db)
        };

        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let app_for_notice = app.clone();
        let on_notice = move |notice: audio::AudioNotice| {
            if let Some(state) = app_for_notice.try_state::<AppState>() {
                let ui = state.apply_audio_notice(&notice, false);
                let _ = app_for_notice.emit("jackvoice://state", ui);
            }
        };

        thread::Builder::new()
            .name("jackvoice-mic-test".into())
            .spawn(move || {
                let capture = match AudioCapture::start_with_device(
                    selected_device,
                    mic_test_gain_db,
                    move |pcm| {
                        let _ = audio_tx.try_send(pcm);
                    },
                    on_notice,
                ) {
                    Ok(capture) => {
                        let _ = ready_tx.send(Ok(()));
                        capture
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                let _ = stop_rx.recv();
                capture.stop();
            })
            .map_err(|e| format!("无法启动麦克风测试：{e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("麦克风测试启动失败。".into()),
        }

        // CoreAudio can successfully create and play an input stream even
        // after the user chose “Don't Allow”. The TCC authorization state is
        // the source of truth; never turn a successful stream start into a
        // false-positive permission result.
        let microphone_authorization = crate::onboarding::microphone_authorization();
        if !microphone_authorization.is_authorized() {
            let _ = stop_tx.send(());
            let message = crate::onboarding::microphone_permission_error(microphone_authorization);
            let ui = self.require_onboarding(&message)?;
            let _ = app.emit("jackvoice://state", ui);
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
            return Err(message);
        }

        {
            let mut monitor = self.monitor.lock();
            *monitor = Some(MonitorSession {
                stop_tx: Some(stop_tx),
            });
        }

        {
            let mut ui = self.ui.lock();
            ui.mic_testing = true;
            ui.audio_level = 0.0;
            ui.status = if ui.active_input_device_name.is_empty() {
                "麦克风测试中。对着麦说话，确认波形是否跳动。".into()
            } else if ui.using_input_device_fallback {
                format!(
                    "正在测试备用麦克风「{}」。首选设备恢复后会自动优先使用。",
                    ui.active_input_device_name
                )
            } else {
                format!(
                    "正在测试「{}」。对着麦说话确认音量条会跳动。",
                    ui.active_input_device_name
                )
            };
            let _ = app.emit("jackvoice://state", ui.clone());
            let _ = app.emit("jackvoice://level", 0.0_f32);
        }

        // Important: start_mic_test is a sync Tauri command (main thread).
        // Do not call tokio::spawn here; there is no runtime on this thread and it panics.
        let app_for_task = app.clone();
        thread::Builder::new()
            .name("jackvoice-mic-test-meter".into())
            .spawn(move || {
                let mut last_emit = Instant::now() - Duration::from_millis(100);
                while let Some(pcm) = audio_rx.blocking_recv() {
                    let level = audio::pcm16_level(&pcm);
                    if let Some(state) = app_for_task.try_state::<AppState>() {
                        if state.monitor.lock().is_none() {
                            break;
                        }
                        let mut ui = state.ui.lock();
                        ui.mic_testing = true;
                        ui.audio_level = level;
                        if last_emit.elapsed() >= Duration::from_millis(50) {
                            last_emit = Instant::now();
                            let _ = app_for_task.emit("jackvoice://level", level);
                            let _ = app_for_task.emit("jackvoice://state", ui.clone());
                        }
                    } else {
                        break;
                    }
                }

                if let Some(state) = app_for_task.try_state::<AppState>() {
                    let mut ui = state.ui.lock();
                    ui.mic_testing = false;
                    ui.audio_level = 0.0;
                    if ui.phase == "idle" {
                        ui.status = "麦克风测试已停止。".into();
                    }
                    let _ = app_for_task.emit("jackvoice://level", 0.0_f32);
                    let _ = app_for_task.emit("jackvoice://state", ui.clone());
                }
            })
            .map_err(|e| format!("无法启动麦克风电平监听：{e}"))?;

        Ok(self.snapshot())
    }

    pub fn stop_mic_test(&self, app: AppHandle) -> Result<UiState, String> {
        self.stop_mic_test_internal();
        let mut ui = self.ui.lock();
        ui.mic_testing = false;
        ui.audio_level = 0.0;
        if ui.phase == "idle" {
            ui.status = "麦克风测试已停止。".into();
        }
        let _ = app.emit("jackvoice://level", 0.0_f32);
        let _ = app.emit("jackvoice://state", ui.clone());
        Ok(ui.clone())
    }

    fn stop_mic_test_internal(&self) {
        if let Some(session) = self.monitor.lock().take() {
            if let Some(tx) = session.stop_tx {
                let _ = tx.send(());
            }
        }
    }

    /// 保存当前唯一识别服务（豆包流式录音识别）的连接配置。
    pub async fn save_volc_settings(
        &self,
        api_key: String,
        resource_id: String,
        boosting_table_id: String,
    ) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再修改豆包识别配置。".into());
        }
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err("请输入豆包语音 API Key。".into());
        }
        let resource_id = if resource_id.trim().is_empty() {
            "volc.seedasr.sauc.duration".to_string()
        } else {
            resource_id.trim().to_string()
        };

        if let Err(error) = verify_volc_service(&api_key, &resource_id).await {
            let is_saved_key = self.settings.lock().volc_api_key.trim() == api_key;
            let mut ui = self.ui.lock();
            if is_saved_key {
                ui.volc_credential_status = VolcCredentialStatus::Failed;
            }
            ui.status = error.clone();
            return Err(error);
        }

        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再修改豆包识别配置。".into());
        }
        let credential_source = crate::credentials::save_volc_api_key(
            self.credential_mode,
            &self.variant_data_dir,
            &api_key,
        )?;
        let mut settings = self.settings.lock();
        settings.volc_api_key = api_key;
        settings.volc_resource_id = resource_id;
        settings.volc_boosting_table_id = boosting_table_id.trim().to_string();
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.volc_credential_source = credential_source.as_str().into();
        ui.volc_credential_status = VolcCredentialStatus::Verified;
        ui.volc_credential_warning.clear();
        ui.status = "豆包语音 API Key 已验证并安全保存。".into();
        Ok(ui.clone())
    }

    /// 显式移除当前应用身份保存的凭据，避免空输入框被误当成删除操作。
    pub fn remove_volc_api_key(&self) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再移除 API Key。".into());
        }
        let mut settings = self.settings.lock();
        let credential_source = crate::credentials::save_volc_api_key(
            self.credential_mode,
            &self.variant_data_dir,
            "",
        )?;
        settings.volc_api_key.clear();
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.volc_credential_source = credential_source.as_str().into();
        ui.volc_credential_status = VolcCredentialStatus::Missing;
        ui.volc_credential_warning.clear();
        ui.status = "豆包语音 API Key 已移除。".into();
        Ok(ui.clone())
    }

    /// 使用真实识别服务验证 API Key 与资源 ID，但不打开麦克风或发送音频。
    /// 输入框为空时测试当前已保存的 API Key，便于用户随时重新验证。
    pub async fn test_volc_connection(
        &self,
        api_key: String,
        resource_id: String,
    ) -> Result<String, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再测试识别连接。".into());
        }

        let settings = self.settings.lock().clone();
        let supplied_api_key = api_key.trim().to_string();
        let testing_saved_key =
            supplied_api_key.is_empty() || supplied_api_key == settings.volc_api_key.trim();
        let api_key = if supplied_api_key.is_empty() {
            settings.volc_api_key.trim().to_string()
        } else {
            supplied_api_key
        };
        if api_key.is_empty() {
            return Err("请先填写或保存豆包语音 API Key。".into());
        }
        let resource_id = if resource_id.trim().is_empty() {
            settings.volc_resource_id.trim().to_string()
        } else {
            resource_id.trim().to_string()
        };
        match verify_volc_service(&api_key, &resource_id).await {
            Ok(message) => {
                let mut ui = self.ui.lock();
                if testing_saved_key {
                    ui.volc_credential_status = VolcCredentialStatus::Verified;
                }
                ui.status = message.clone();
                Ok(message)
            }
            Err(message) => {
                let mut ui = self.ui.lock();
                if testing_saved_key {
                    ui.volc_credential_status = VolcCredentialStatus::Failed;
                }
                ui.status = message.clone();
                Err(message)
            }
        }
    }

    /// 保存本地热词；若当前是火山引擎且已配置热词表 ID，则自动同步热词表。
    /// 本地保存成功后，云端同步失败不会回滚本地结果。
    pub async fn save_hotwords_and_maybe_sync(
        &self,
        words: Vec<String>,
    ) -> Result<SaveHotwordsResult, String> {
        let cleaned = crate::hotwords::sanitize(&words);
        crate::hotwords::save(&self.shared_data_dir, &cleaned)?;

        let settings = self.settings.lock().clone();
        let should_sync = !settings.volc_api_key.trim().is_empty()
            && !settings.volc_boosting_table_id.trim().is_empty();

        if !should_sync {
            let message = format!("热词已保存（{} 个）。", cleaned.len());
            let mut ui = self.ui.lock();
            ui.status = message.clone();
            return Ok(SaveHotwordsResult {
                words: cleaned,
                synced: false,
                message,
                sync_error: None,
            });
        }

        if self.active.lock().is_some() {
            let message = format!(
                "热词已保存（{} 个）；听写中，云端同步将在结束后手动执行。",
                cleaned.len()
            );
            let mut ui = self.ui.lock();
            ui.status = message.clone();
            return Ok(SaveHotwordsResult {
                words: cleaned,
                synced: false,
                message,
                sync_error: Some("正在听写中，已跳过自动同步。".into()),
            });
        }

        let boosting_file = crate::hotwords::format_volc_boosting_table_file(&cleaned);
        if boosting_file.trim().is_empty() {
            let message = format!("热词已保存（{} 个）；没有可同步的有效热词。", cleaned.len());
            let mut ui = self.ui.lock();
            ui.status = message.clone();
            return Ok(SaveHotwordsResult {
                words: cleaned,
                synced: false,
                message,
                sync_error: Some("没有可同步的火山热词。".into()),
            });
        }

        {
            let mut ui = self.ui.lock();
            ui.status = format!("热词已保存（{} 个），正在同步云端热词表…", cleaned.len());
        }

        match crate::volc_hotword_api::update_boosting_table(
            &settings.volc_api_key,
            &settings.volc_boosting_table_id,
            None,
            &boosting_file,
        )
        .await
        {
            Ok(sync) => {
                let table_label = if sync.table_name.is_empty() {
                    sync.table_id
                } else {
                    sync.table_name
                };
                let message = format!(
                    "热词已保存，并同步到云端热词表「{}」：{} 个。",
                    table_label, sync.word_count
                );
                let mut ui = self.ui.lock();
                ui.status = message.clone();
                Ok(SaveHotwordsResult {
                    words: cleaned,
                    synced: true,
                    message,
                    sync_error: None,
                })
            }
            Err(err) => {
                let message = format!(
                    "热词已保存（{} 个）；同步云端热词表失败：{err}",
                    cleaned.len()
                );
                let mut ui = self.ui.lock();
                ui.status = message.clone();
                Ok(SaveHotwordsResult {
                    words: cleaned,
                    synced: false,
                    message,
                    sync_error: Some(err),
                })
            }
        }
    }

    /// 保存用户配置的替换词。
    /// 替换是听写后的本地确定性后处理，不依赖云端替换词表。
    pub async fn save_replacements_and_maybe_sync(
        &self,
        rules: Vec<crate::hotwords::ReplacementRule>,
    ) -> Result<SaveHotwordsResult, String> {
        let cleaned = crate::hotwords::sanitize_replacements(&rules);
        crate::hotwords::save_replacements(&self.shared_data_dir, &cleaned)?;

        let message = format!("替换词已保存（{} 条）；听写时本地生效。", cleaned.len());
        let mut ui = self.ui.lock();
        ui.status = message.clone();
        Ok(SaveHotwordsResult {
            words: cleaned
                .iter()
                .map(|r| format!("{}→{}", r.from, r.to))
                .collect(),
            synced: false,
            message,
            sync_error: None,
        })
    }

    /// 手动同步云端热词表。
    /// 替换词只在本地生效，不再同步火山替换词表。
    pub async fn sync_volc_hotword_table(&self) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再同步云端。".into());
        }
        let settings = self.settings.lock().clone();
        if settings.volc_api_key.trim().is_empty() {
            return Err("请先填写豆包语音 API Key。".into());
        }

        let local_words = crate::hotwords::load(&self.shared_data_dir);
        let cleaned_words = crate::hotwords::sanitize(&local_words);
        crate::hotwords::save(&self.shared_data_dir, &cleaned_words)?;
        let local_rules = crate::hotwords::sanitize_replacements(
            &crate::hotwords::load_replacements(&self.shared_data_dir),
        );
        // 本地替换词只做规范化落盘，不推云端。
        crate::hotwords::save_replacements(&self.shared_data_dir, &local_rules)?;

        let mut parts = Vec::new();
        {
            let mut ui = self.ui.lock();
            ui.status = "正在同步热词表到火山…".into();
        }

        if settings.volc_boosting_table_id.trim().is_empty() {
            parts.push("未配置热词表 ID，跳过热词同步".into());
        } else {
            let boosting_file = crate::hotwords::format_volc_boosting_table_file(&cleaned_words);
            if boosting_file.trim().is_empty() {
                parts.push("没有可同步的热词".into());
            } else {
                match crate::volc_hotword_api::update_boosting_table(
                    &settings.volc_api_key,
                    &settings.volc_boosting_table_id,
                    None,
                    &boosting_file,
                )
                .await
                {
                    Ok(sync) => parts.push(format!(
                        "热词表「{}」{} 个",
                        if sync.table_name.is_empty() {
                            sync.table_id
                        } else {
                            sync.table_name
                        },
                        sync.word_count
                    )),
                    Err(err) => parts.push(format!("热词表同步失败：{err}")),
                }
            }
        }

        parts.push(format!("替换词本地 {} 条（不走云端）", local_rules.len()));

        let settings = self.settings.lock().clone();
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = format!("已同步：{}", parts.join("；"));
        Ok(ui.clone())
    }

    pub fn set_input_gain(&self, gain_db: f32) -> Result<UiState, String> {
        let clamped = gain_db.clamp(0.0, MAX_INPUT_GAIN_DB);
        let mut settings = self.settings.lock();
        settings.input_gain_db = clamped;
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = if clamped <= 0.0 {
            "麦克风音量增强已恢复为默认。".into()
        } else {
            format!("麦克风音量增强已设为 +{clamped:.0} dB。")
        };
        Ok(ui.clone())
    }

    pub fn update_recognition_options(
        &self,
        semantic_punctuation_enabled: bool,
        semantic_smoothing_enabled: bool,
        max_sentence_silence_ms: u32,
    ) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        settings.semantic_punctuation_enabled = semantic_punctuation_enabled;
        settings.semantic_smoothing_enabled = semantic_smoothing_enabled;
        settings.max_sentence_silence_ms = if max_sentence_silence_ms == 0 {
            0
        } else {
            max_sentence_silence_ms.max(200)
        };
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = "识别参数已保存。".into();
        Ok(ui.clone())
    }

    pub fn set_history_text_size(&self, size: String) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        settings.history_text_size = settings::normalize_history_text_size(size.trim());
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = "听写文字大小已更新。".into();
        Ok(ui.clone())
    }

    pub fn set_shortcut(&self, shortcut: String) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        settings.shortcut = shortcut.clone();
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        ui.shortcut = shortcut.clone();
        ui.status = format!("免按模式快捷键已更新为 {shortcut}。");
        Ok(ui.clone())
    }

    pub fn set_launch_at_login_flag(&self, enabled: bool) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        settings.launch_at_login = enabled;
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        ui.launch_at_login = enabled;
        ui.status = if enabled {
            "已开启开机时启动应用。".into()
        } else {
            "已关闭开机时启动应用。".into()
        };
        Ok(ui.clone())
    }

    pub fn set_mute_system_audio_during_dictation(&self, enabled: bool) -> Result<UiState, String> {
        if enabled && !crate::output_mute::supported() {
            return Err("当前系统暂不支持听写时自动静音。".into());
        }
        if self.active.lock().is_some() {
            return Err("正在听写中，请结束后再修改系统音频设置。".into());
        }
        let mut settings = self.settings.lock();
        settings.mute_system_audio_during_dictation = enabled;
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = if enabled {
            "已开启听写时临时静音系统音频。".into()
        } else {
            "已关闭听写时临时静音系统音频。".into()
        };
        Ok(ui.clone())
    }

    /// Best-effort process-exit cleanup. Session stop paths call the same
    /// guard earlier, immediately after microphone capture ends.
    pub fn restore_output_audio(&self) {
        let guard = self
            .active
            .lock()
            .as_mut()
            .and_then(|active| active.output_mute.take());
        restore_output_mute(guard);
    }

    /// Persist the "onboarding finished" flag so future launches go straight
    /// to the main UI.
    pub fn complete_onboarding(&self) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        settings.onboarding_completed = true;
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        ui.onboarding_completed = true;
        ui.status = "设置完成，开始使用 JackVoice。".into();
        Ok(ui.clone())
    }

    /// Return the app to the permission walkthrough when a required system
    /// permission has been revoked after onboarding was previously completed.
    pub fn require_onboarding(&self, message: &str) -> Result<UiState, String> {
        let mut settings = self.settings.lock();
        if settings.onboarding_completed {
            settings.onboarding_completed = false;
            self.save_settings(&settings)?;
        }
        let mut ui = self.ui.lock();
        ui.onboarding_completed = false;
        ui.status = message.into();
        Ok(ui.clone())
    }

    pub async fn cancel(&self, app: AppHandle) -> Result<UiState, String> {
        // Silent cancel: stop capture/session and hide overlay.
        // Do NOT focus/show the settings window. Main UI is manual-only.
        let had_active = self.active.lock().is_some();
        *self.cancel_requested.lock() = true;
        if had_active {
            // stop without relying on UI focus changes
            let _ = self.stop(app.clone()).await?;
        }

        {
            let mut ui = self.ui.lock();
            ui.phase = "idle".into();
            ui.recognition_phase = "idle".into();
            ui.status = if had_active {
                "已取消本次听写。".into()
            } else {
                ui.status.clone()
            };
            ui.audio_level = 0.0;
            ui.needs_copy_prompt = false;
            // Emit only for open windows; never force main window forward.
            let _ = app.emit("jackvoice://state", ui.clone());
            let _ = app.emit("jackvoice://level", 0.0_f32);
        }

        crate::overlay::hide_overlay(&app);
        crate::overlay::ensure_main_stays_in_background(&app);
        Ok(self.snapshot())
    }

    pub async fn toggle(&self, app: AppHandle) -> Result<UiState, String> {
        let is_active = self.active.lock().is_some();
        if is_active {
            self.stop(app).await
        } else {
            if !self.snapshot().onboarding_completed {
                if let Some(main) = app.get_webview_window("main") {
                    let _ = main.show();
                    let _ = main.set_focus();
                }
                return Err("请先完成必需权限设置，再开始使用 JackVoice。".into());
            }
            let permissions = crate::onboarding::current_status();
            if !permissions.required_permissions_granted() {
                let message = permissions.missing_permissions_message();
                let ui = self.require_onboarding(&message)?;
                let _ = app.emit("jackvoice://state", ui);
                if let Some(main) = app.get_webview_window("main") {
                    let _ = main.show();
                    let _ = main.set_focus();
                }
                return Err(message);
            }
            self.start(app).await
        }
    }

    async fn start(&self, app: AppHandle) -> Result<UiState, String> {
        // Claim the session before any disk, microphone or UI work. A second
        // shortcut press from this point onward can only stop this exact session.
        let session_id = self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let (stop_tx, stop_rx) = mpsc::channel::<SessionControl>(4);
        let (audio_stop_tx, audio_stop_rx) = std::sync::mpsc::channel::<()>();
        {
            let mut active = self.active.lock();
            if active.is_some() {
                return Ok(self.snapshot());
            }
            *active = Some(ActiveSession {
                id: session_id,
                stop_tx: Some(stop_tx.clone()),
                audio_stop_tx: Some(audio_stop_tx.clone()),
                output_mute: None,
                started_at: None,
                stopping: false,
            });
        }

        self.stop_mic_test_internal();
        *self.cancel_requested.lock() = false;
        let settings = self.settings.lock().clone();

        {
            let mut ui = self.ui.lock();
            ui.phase = "starting".into();
            ui.recognition_phase = if settings.volc_api_key.trim().is_empty() {
                "unavailable".into()
            } else {
                "connecting".into()
            };
            ui.status = "正在启动本地录音…".into();
            ui.transcript.clear();
            ui.last_delivery_message.clear();
            ui.audio_level = 0.0;
            ui.needs_copy_prompt = false;
            let _ = app.emit("jackvoice://state", ui.clone());
        }

        // The recorder exists before CoreAudio starts. Once the first PCM frame
        // arrives there is already a durable local destination for it.
        let record_id = uuid::Uuid::new_v4().to_string();
        let recorder = match AudioRecorder::create(&self.shared_data_dir, &record_id) {
            Ok(recorder) => recorder,
            Err(error) => {
                if self
                    .active
                    .lock()
                    .as_ref()
                    .is_some_and(|active| active.id == session_id)
                {
                    self.active.lock().take();
                }
                let mut ui = self.ui.lock();
                ui.phase = "error".into();
                ui.recognition_phase = "idle".into();
                ui.status = error.clone();
                let _ = app.emit("jackvoice://state", ui.clone());
                drop(ui);
                crate::overlay::show_overlay(&app);
                schedule_error_overlay_hide(&app, session_id);
                return Err(error);
            }
        };

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(LOCAL_AUDIO_QUEUE_CAPACITY);
        let (audio_ready_tx, audio_ready_rx) = oneshot::channel::<Result<(), String>>();
        let selected_device = input_device_preference(&settings);
        let dictation_gain_db = settings.input_gain_db;

        // Microphone fault notices become a transient toast on the capsule.
        let app_for_notice = app.clone();
        let on_notice = move |notice: audio::AudioNotice| {
            if let Some(state) = app_for_notice.try_state::<AppState>() {
                let is_current = state
                    .active
                    .lock()
                    .as_ref()
                    .is_some_and(|active| active.id == session_id && !active.stopping);
                if !is_current {
                    return;
                }
                let ui = state.apply_audio_notice(&notice, true);
                let _ = app_for_notice.emit("jackvoice://state", ui);
            }
        };

        let overflow_reported = Arc::new(AtomicBool::new(false));
        let overflow_for_pcm = overflow_reported.clone();
        let control_for_pcm = stop_tx.clone();

        // Own AudioCapture fully on a dedicated thread so AppState remains Send+Sync.
        let spawn_result = thread::Builder::new()
            .name("jackvoice-audio-owner".into())
            .spawn(move || {
                let capture = match AudioCapture::start_with_device(
                    selected_device,
                    dictation_gain_db,
                    move |pcm| {
                        if let Err(error) = audio_tx.try_send(pcm) {
                            if matches!(error, mpsc::error::TrySendError::Full(_))
                                && !overflow_for_pcm.swap(true, Ordering::AcqRel)
                            {
                                let _ = control_for_pcm.try_send(SessionControl::RecordingFailed(
                                    "本地录音缓冲区已满，录音可能不完整，已停止本次听写。".into(),
                                ));
                            }
                        }
                    },
                    on_notice,
                ) {
                    Ok(capture) => {
                        let _ = audio_ready_tx.send(Ok(()));
                        capture
                    }
                    Err(err) => {
                        let _ = audio_ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };

                let _ = audio_stop_rx.recv();
                capture.stop();
            });

        let ready = match spawn_result {
            Ok(_) => audio_ready_rx
                .await
                .unwrap_or_else(|_| Err("录音线程启动失败。".into())),
            Err(error) => Err(format!("无法启动录音线程：{error}")),
        };
        if let Err(error) = ready {
            if self
                .active
                .lock()
                .as_ref()
                .is_some_and(|active| active.id == session_id)
            {
                self.active.lock().take();
            }
            let mut ui = self.ui.lock();
            ui.phase = "error".into();
            ui.recognition_phase = "idle".into();
            ui.status = error.clone();
            ui.audio_level = 0.0;
            let _ = app.emit("jackvoice://state", ui.clone());
            drop(ui);
            crate::overlay::show_overlay(&app);
            schedule_error_overlay_hide(&app, session_id);
            return Err(error);
        }

        let stopping = {
            let mut active = self.active.lock();
            match active.as_mut().filter(|active| active.id == session_id) {
                Some(active) => {
                    active.started_at = Some(Instant::now());
                    active.stopping
                }
                None => true,
            }
        };

        if !stopping {
            let mut ui = self.ui.lock();
            ui.phase = "recording".into();
            ui.recognition_phase = if settings.volc_api_key.trim().is_empty() {
                "unavailable".into()
            } else {
                "connecting".into()
            };
            ui.status = if ui.recognition_phase == "connecting" {
                format!("正在录音 · 正在连接{ASR_ENGINE_NAME}…")
            } else {
                "正在本地录音 · 尚未配置实时识别".into()
            };
            let _ = app.emit("jackvoice://state", ui.clone());
        }

        let actual_input_device_id = self.ui.lock().active_input_device_id.clone();

        let app_for_task = app.clone();
        tokio::spawn(run_recording_session(
            app_for_task,
            RecordingSession {
                session_id,
                record_id,
                recorder,
                audio_rx,
                control_rx: stop_rx,
                audio_stop_tx: audio_stop_tx.clone(),
                settings: settings.clone(),
                actual_input_device_id,
            },
        ));

        let output_mute = if settings.mute_system_audio_during_dictation && !stopping {
            match OutputMuteGuard::engage(&self.shared_data_dir) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    eprintln!("[output-mute] 本次听写未能静音系统音频：{error}");
                    let mut ui = self.ui.lock();
                    ui.mic_notice = format!("系统音频未静音：{error}");
                    ui.mic_notice_seq = ui.mic_notice_seq.wrapping_add(1);
                    let _ = app.emit("jackvoice://state", ui.clone());
                    None
                }
            }
        } else {
            None
        };

        if let Some(guard) = output_mute {
            let mut active = self.active.lock();
            if let Some(active) = active.as_mut().filter(|active| active.id == session_id) {
                active.output_mute = Some(guard);
            } else {
                drop(active);
                restore_output_mute(Some(guard));
            }
        }

        if !stopping {
            // Any relatively slow positioning/frontmost-app work happens only
            // after the microphone and recorder are already live.
            crate::overlay::show_overlay(&app);
        }

        Ok(self.snapshot())
    }

    async fn stop(&self, app: AppHandle) -> Result<UiState, String> {
        let (stop_tx, audio_stop_tx, output_mute) = {
            let mut active = self.active.lock();
            match active.as_mut() {
                Some(session) => {
                    session.stopping = true;
                    (
                        session.stop_tx.take(),
                        session.audio_stop_tx.take(),
                        session.output_mute.take(),
                    )
                }
                None => (None, None, None),
            }
        };

        if let Some(tx) = audio_stop_tx {
            let _ = tx.send(());
        }
        restore_output_mute(output_mute);

        {
            let mut ui = self.ui.lock();
            ui.phase = "finalizing".into();
            ui.recognition_phase = "finalizing".into();
            ui.status = "正在结束听写并收尾…".into();
            ui.audio_level = 0.0;
            let _ = app.emit("jackvoice://level", 0.0_f32);
            let _ = app.emit("jackvoice://state", ui.clone());
        }

        if let Some(tx) = stop_tx {
            let _ = tx.send(SessionControl::Stop).await;
        }

        Ok(self.snapshot())
    }
}

async fn run_asr_sender(
    session: RealtimeSession,
    mut commands: mpsc::Receiver<AsrCommand>,
    error_tx: mpsc::UnboundedSender<String>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            AsrCommand::Audio(pcm) => {
                if let Err(error) = session.send_audio(pcm).await {
                    let _ = error_tx.send(format_volc_connection_error(&error.to_string()));
                    session.cancel().await;
                    return;
                }
            }
            AsrCommand::Finish(result_tx) => {
                let result = session
                    .finish()
                    .await
                    .map_err(|error| format_volc_connection_error(&error.to_string()));
                let _ = result_tx.send(result);
                return;
            }
        }
    }
    session.cancel().await;
}

fn persist_local_pcm(
    app: &AppHandle,
    session_id: u64,
    recorder: &mut AudioRecorder,
    pcm: &[u8],
    last_level_emit: &mut Instant,
) -> Result<(), String> {
    // Local persistence always happens before the chunk is offered to ASR.
    recorder.write_pcm(pcm)?;
    let level = audio::pcm16_level(pcm);
    if let Some(state) = app.try_state::<AppState>() {
        let is_current = state
            .active
            .lock()
            .as_ref()
            .is_some_and(|active| active.id == session_id);
        if is_current {
            let mut ui = state.ui.lock();
            ui.audio_level = level;
            if last_level_emit.elapsed() >= Duration::from_millis(50) {
                *last_level_emit = Instant::now();
                let _ = app.emit("jackvoice://level", level);
                let _ = app.emit("jackvoice://state", ui.clone());
            }
        }
    }
    Ok(())
}

fn mark_recognition_unavailable(app: &AppHandle, session_id: u64, message: &str) {
    if let Some(state) = app.try_state::<AppState>() {
        let is_current = state
            .active
            .lock()
            .as_ref()
            .is_some_and(|active| active.id == session_id && !active.stopping);
        if !is_current {
            return;
        }
        let mut ui = state.ui.lock();
        ui.recognition_phase = "error".into();
        if ui.has_volc_api_key {
            ui.volc_credential_status = VolcCredentialStatus::Failed;
        }
        ui.status = format!("正在本地录音 · 实时识别不可用：{message}");
        let _ = app.emit("jackvoice://state", ui.clone());
    }
}

async fn run_recording_session(app: AppHandle, session: RecordingSession) {
    let RecordingSession {
        session_id,
        record_id,
        mut recorder,
        mut audio_rx,
        mut control_rx,
        audio_stop_tx,
        settings,
        actual_input_device_id,
    } = session;
    let punctuation = settings.semantic_punctuation_enabled;
    let smoothing = settings.semantic_smoothing_enabled;
    let silence = settings.max_sentence_silence_ms;
    let dictionary = app
        .try_state::<AppState>()
        .map(|state| crate::hotwords::load(&state.shared_data_dir))
        .unwrap_or_default();
    let hotwords = crate::hotwords::recognition_words(&dictionary);
    let replacement_rules = app
        .try_state::<AppState>()
        .map(|state| crate::hotwords::user_replacement_rules(&state.shared_data_dir))
        .unwrap_or_default();
    let recognition_context = RecognitionContext {
        hotwords: hotwords.clone(),
        semantic_punctuation_enabled: punctuation,
        semantic_smoothing_enabled: smoothing,
        max_sentence_silence_ms: silence,
        input_gain_db: settings.input_gain_db,
        input_device_id: actual_input_device_id,
    };

    let mut recognition_error = if settings.volc_api_key.trim().is_empty() {
        Some("尚未配置豆包语音 API Key".to_string())
    } else {
        None
    };
    let mut connect_future: Option<ConnectFuture> = if recognition_error.is_none() {
        let config = VolcAsrConfig {
            api_key: settings.volc_api_key.clone(),
            resource_id: settings.volc_resource_id.clone(),
            boosting_table_id: settings.volc_boosting_table_id.clone(),
        };
        let connect_hotwords = hotwords.clone();
        let app_for_updates = app.clone();
        let replacement_rules_for_updates = replacement_rules.clone();
        Some(Box::pin(async move {
            tokio::time::timeout(
                ASR_CONNECT_TIMEOUT,
                RealtimeSession::connect(
                    config,
                    punctuation,
                    smoothing,
                    silence,
                    connect_hotwords,
                    move |update: TranscriptUpdate| {
                        if let Some(state) = app_for_updates.try_state::<AppState>() {
                            let is_current =
                                state.active.lock().as_ref().is_some_and(|active| {
                                    active.id == session_id && !active.stopping
                                });
                            if !is_current {
                                return;
                            }
                            let mut ui = state.ui.lock();
                            if ui.recognition_phase != "streaming" {
                                return;
                            }
                            ui.transcript = crate::hotwords::apply_replacements(
                                &update.text,
                                &replacement_rules_for_updates,
                            );
                            ui.status = if update.is_final_sentence {
                                "正在录音 · 已生成句子".into()
                            } else {
                                "正在录音 · 实时转写中".into()
                            };
                            let _ = app_for_updates.emit("jackvoice://state", ui.clone());
                        }
                    },
                ),
            )
            .await
            .map_err(|_| "连接豆包语音超时".to_string())?
            .map_err(|error| format_volc_connection_error(&error.to_string()))
        }))
    } else {
        None
    };

    let mut connect_backlog = VecDeque::<Vec<u8>>::new();
    let mut connect_backlog_bytes = 0usize;
    let mut asr_tx: Option<mpsc::Sender<AsrCommand>> = None;
    let (asr_error_tx, mut asr_error_rx) = mpsc::unbounded_channel::<String>();
    let mut asr_error_tx = Some(asr_error_tx);
    let mut local_error: Option<String> = None;
    let mut last_level_emit = Instant::now() - Duration::from_millis(100);
    let mut reached_duration_limit = false;
    let duration_limit = tokio::time::sleep(MAX_DICTATION_DURATION);
    tokio::pin!(duration_limit);

    loop {
        tokio::select! {
            _ = &mut duration_limit => {
                reached_duration_limit = true;
                let output_mute = app.try_state::<AppState>().and_then(|state| {
                    let mut active = state.active.lock();
                    active
                        .as_mut()
                        .filter(|active| active.id == session_id)
                        .and_then(|active| {
                            active.stopping = true;
                            active.output_mute.take()
                        })
                });
                let _ = audio_stop_tx.send(());
                restore_output_mute(output_mute);
                if connect_future.is_some() && recognition_error.is_none() {
                    recognition_error = Some("达到时长上限前实时识别尚未连接完成".into());
                }
                drop(connect_future.take());
                if let Some(state) = app.try_state::<AppState>() {
                    let mut ui = state.ui.lock();
                    ui.phase = "finalizing".into();
                    ui.recognition_phase = "finalizing".into();
                    ui.audio_level = 0.0;
                    ui.status = format!(
                        "已达到单次听写 {MAX_DICTATION_DURATION_LABEL}上限，正在自动结束并整理文字…"
                    );
                    let _ = app.emit("jackvoice://level", 0.0_f32);
                    let _ = app.emit("jackvoice://state", ui.clone());
                }
                break;
            }
            control = control_rx.recv() => {
                match control {
                    Some(SessionControl::Stop) | None => {}
                    Some(SessionControl::RecordingFailed(error)) => local_error = Some(error),
                }
                let _ = audio_stop_tx.send(());
                if connect_future.is_some() && recognition_error.is_none() {
                    recognition_error = Some("录音结束前实时识别尚未连接完成".into());
                }
                drop(connect_future.take());
                break;
            }
            maybe_pcm = audio_rx.recv() => {
                let Some(pcm) = maybe_pcm else {
                    let expected_stop = app
                        .try_state::<AppState>()
                        .and_then(|state| {
                            state
                                .active
                                .lock()
                                .as_ref()
                                .filter(|active| active.id == session_id)
                                .map(|active| active.stopping)
                        })
                        .unwrap_or(false);
                    if !expected_stop {
                        local_error = Some("麦克风录音线程意外结束".into());
                    }
                    if connect_future.is_some() && recognition_error.is_none() {
                        recognition_error = Some("录音结束前实时识别尚未连接完成".into());
                    }
                    drop(connect_future.take());
                    break;
                };
                if let Err(error) = persist_local_pcm(
                    &app,
                    session_id,
                    &mut recorder,
                    &pcm,
                    &mut last_level_emit,
                ) {
                    local_error = Some(error);
                    let _ = audio_stop_tx.send(());
                    drop(connect_future.take());
                    break;
                }

                if let Some(tx) = asr_tx.as_ref() {
                    if tx.try_send(AsrCommand::Audio(pcm)).is_err() {
                        let error = "实时识别发送缓冲区已满".to_string();
                        recognition_error = Some(error.clone());
                        asr_tx = None;
                        mark_recognition_unavailable(&app, session_id, &error);
                    }
                } else if connect_future.is_some() {
                    connect_backlog_bytes = connect_backlog_bytes.saturating_add(pcm.len());
                    if connect_backlog_bytes > MAX_CONNECT_BACKLOG_BYTES {
                        let error = "实时识别连接过慢，已停止本次实时预览".to_string();
                        recognition_error = Some(error.clone());
                        connect_future = None;
                        connect_backlog.clear();
                        connect_backlog_bytes = 0;
                        mark_recognition_unavailable(&app, session_id, &error);
                    } else {
                        connect_backlog.push_back(pcm);
                    }
                }
            }
            connect_result = async {
                connect_future.as_mut().expect("guarded connect future").as_mut().await
            }, if connect_future.is_some() => {
                connect_future = None;
                match connect_result {
                    Ok(session) => {
                        let (command_tx, command_rx) = mpsc::channel(ASR_AUDIO_QUEUE_CAPACITY);
                        tokio::spawn(run_asr_sender(
                            session,
                            command_rx,
                            asr_error_tx.take().expect("ASR error sender is available"),
                        ));
                        let mut backlog_failed = false;
                        while let Some(pcm) = connect_backlog.pop_front() {
                            if command_tx.try_send(AsrCommand::Audio(pcm)).is_err() {
                                backlog_failed = true;
                                break;
                            }
                        }
                        connect_backlog_bytes = 0;
                        if backlog_failed {
                            let error = "实时识别无法追赶连接前的录音".to_string();
                            recognition_error = Some(error.clone());
                            mark_recognition_unavailable(&app, session_id, &error);
                        } else {
                            asr_tx = Some(command_tx);
                            if let Some(state) = app.try_state::<AppState>() {
                                let is_current = state
                                    .active
                                    .lock()
                                    .as_ref()
                                    .is_some_and(|active| active.id == session_id && !active.stopping);
                                if is_current {
                                    let mut ui = state.ui.lock();
                                    ui.recognition_phase = "streaming".into();
                                    ui.volc_credential_status = VolcCredentialStatus::Verified;
                                    ui.status = "正在录音 · 实时识别已连接".into();
                                    let _ = app.emit("jackvoice://state", ui.clone());
                                }
                            }
                        }
                    }
                    Err(error) => {
                        recognition_error = Some(error.clone());
                        connect_backlog.clear();
                        connect_backlog_bytes = 0;
                        mark_recognition_unavailable(&app, session_id, &error);
                    }
                }
            }
            maybe_error = asr_error_rx.recv(), if asr_tx.is_some() => {
                if let Some(error) = maybe_error {
                    recognition_error = Some(error.clone());
                    asr_tx = None;
                    mark_recognition_unavailable(&app, session_id, &error);
                }
            }
        }
    }

    // Capture.stop() joins the CoreAudio thread. Drain everything it emitted
    // before channel close so the tail of the recording is never truncated.
    while let Some(pcm) = audio_rx.recv().await {
        if let Err(error) =
            persist_local_pcm(&app, session_id, &mut recorder, &pcm, &mut last_level_emit)
        {
            local_error.get_or_insert(error);
            break;
        }
        if let Some(tx) = asr_tx.as_ref() {
            if tx.try_send(AsrCommand::Audio(pcm)).is_err() {
                let error = "实时识别发送缓冲区已满".to_string();
                recognition_error = Some(error);
                asr_tx = None;
            }
        }
    }

    let is_cancel = app
        .try_state::<AppState>()
        .map(|state| *state.cancel_requested.lock())
        .unwrap_or(false);
    if is_cancel {
        drop(asr_tx.take());
        recognition_error = Some("用户已取消文字识别".into());
    }

    if !is_cancel {
        if let Some(state) = app.try_state::<AppState>() {
            let mut ui = state.ui.lock();
            ui.phase = "finalizing".into();
            ui.recognition_phase = "finalizing".into();
            ui.audio_level = 0.0;
            ui.status = if reached_duration_limit {
                format!(
                    "已达到单次听写 {MAX_DICTATION_DURATION_LABEL}上限，录音已停止，正在整理文字…"
                )
            } else {
                "本地录音已完成，正在整理文字…".into()
            };
            let _ = app.emit("jackvoice://level", 0.0_f32);
            let _ = app.emit("jackvoice://state", ui.clone());
        }
    } else if let Some(state) = app.try_state::<AppState>() {
        let mut ui = state.ui.lock();
        ui.phase = "idle".into();
        ui.recognition_phase = "idle".into();
        ui.audio_level = 0.0;
        ui.status = "正在保存已取消听写的本地录音…".into();
        let _ = app.emit("jackvoice://level", 0.0_f32);
        let _ = app.emit("jackvoice://state", ui.clone());
    }

    // Commit the local WAV before waiting for any network finalization.
    let saved_audio = match recorder.finish() {
        Ok(audio) => Some(audio),
        Err(error) => {
            local_error.get_or_insert(error);
            None
        }
    };

    let mut final_text = app
        .try_state::<AppState>()
        .map(|state| state.ui.lock().transcript.clone())
        .unwrap_or_default();
    if local_error.is_none() && !is_cancel {
        if let Some(tx) = asr_tx.take() {
            let (result_tx, result_rx) = oneshot::channel();
            if tx.send(AsrCommand::Finish(result_tx)).await.is_ok() {
                match result_rx.await {
                    Ok(Ok(text)) => {
                        final_text = crate::hotwords::apply_replacements(&text, &replacement_rules);
                        recognition_error = None;
                    }
                    Ok(Err(error)) => recognition_error = Some(error),
                    Err(_) => recognition_error = Some("实时识别收尾任务意外结束".into()),
                }
            } else {
                recognition_error = Some("实时识别会话已经关闭".into());
            }
        }
    } else {
        drop(asr_tx);
    }

    if let Some(state) = app.try_state::<AppState>() {
        let elapsed_ms = state
            .active
            .lock()
            .as_ref()
            .filter(|active| active.id == session_id)
            .and_then(|active| active.started_at)
            .map(|started| started.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let duration_ms = saved_audio
            .as_ref()
            .map(|audio| audio.duration_ms)
            .unwrap_or(elapsed_ms);
        let history_result = crate::history::append(
            &state.shared_data_dir,
            HistoryAppend {
                id: record_id,
                text: &final_text,
                duration_ms,
                audio: saved_audio,
                recognition: Some(recognition_context),
                recording_error: local_error.clone(),
                recognition_error: recognition_error.clone(),
            },
        );
        if let Err(error) = history_result {
            eprintln!("[history] 保存听写记录失败：{error}");
            local_error.get_or_insert(format!("录音文件已保存，但写入历史记录失败：{error}"));
        } else {
            let _ = app.emit("jackvoice://history", true);
        }

        let mut delivery_result = None;
        let mut needs_copy_prompt = false;
        if !final_text.trim().is_empty() && local_error.is_none() && recognition_error.is_none() {
            let initial_target = crate::overlay::remembered_frontmost_app();
            let current_target = crate::overlay::current_frontmost_app();
            let current_probe = current_target
                .as_deref()
                .map(|target| delivery::probe_insertion_target(Some(target)))
                .unwrap_or(delivery::InsertionProbe::Unknown);
            let target =
                delivery::choose_delivery_target(initial_target.clone(), current_target.clone());
            eprintln!(
                "[delivery] target initial={initial_target:?} current={current_target:?} current_probe={current_probe:?} selected={target:?}"
            );
            let reactivate_target = target.is_some() && target != current_target;
            crate::overlay::set_remembered_frontmost_app(target.clone());
            crate::overlay::hide_overlay_for_delivery(&app, reactivate_target);
            // Keep the original working delay for every delivery, including
            // when the target app never changed. The global shortcut fires
            // while Option is still physically held; posting Cmd+V
            // immediately can therefore become Cmd+Option+V in the target.
            tokio::time::sleep(Duration::from_millis(350)).await;
            let probe = if reactivate_target {
                // Probe after a genuinely different target has been restored.
                delivery::probe_insertion_target(target.as_deref())
            } else {
                // Preserve the already-valid caret and the probe captured while
                // its exact window was still active.
                current_probe
            };
            let delivery = delivery::deliver_text(&app, &final_text, probe).await;
            needs_copy_prompt = !delivery.pasted;
            delivery_result = Some(delivery);
        } else if local_error.is_none() {
            crate::overlay::hide_overlay(&app);
            crate::overlay::ensure_main_stays_in_background(&app);
        }

        let guard = {
            let mut active = state.active.lock();
            if active
                .as_ref()
                .is_some_and(|active| active.id == session_id)
            {
                active
                    .take()
                    .and_then(|mut active| active.output_mute.take())
            } else {
                None
            }
        };
        restore_output_mute(guard);

        let mut ui = state.ui.lock();
        ui.audio_level = 0.0;
        ui.transcript = final_text;
        ui.needs_copy_prompt = needs_copy_prompt;
        ui.recognition_phase = "idle".into();
        if let Some(error) = local_error {
            ui.phase = "error".into();
            ui.status = error;
        } else {
            ui.phase = "idle".into();
            ui.status = if is_cancel {
                "已取消文字处理，本地录音已保存。".into()
            } else if let Some(error) = recognition_error {
                if reached_duration_limit {
                    format!(
                        "已达到单次听写 {MAX_DICTATION_DURATION_LABEL}上限并自动结束；本地录音已保存；实时识别未完成：{error}"
                    )
                } else {
                    format!("本地录音已保存；实时识别未完成：{error}")
                }
            } else if ui.transcript.trim().is_empty() {
                if reached_duration_limit {
                    format!(
                        "已达到单次听写 {MAX_DICTATION_DURATION_LABEL}上限并自动结束；本地录音已保存，本次未识别到文字。"
                    )
                } else {
                    "本地录音已保存，本次未识别到文字。".into()
                }
            } else if reached_duration_limit {
                format!(
                    "已达到单次听写 {MAX_DICTATION_DURATION_LABEL}上限并自动结束，本地录音已保存。"
                )
            } else {
                "听写结束，本地录音已保存。".into()
            };
        }
        if let Some(delivery) = delivery_result.as_ref() {
            ui.last_delivery_message = delivery.message.clone();
        }
        let phase_is_error = ui.phase == "error";
        let _ = app.emit("jackvoice://state", ui.clone());
        drop(ui);
        if let Some(delivery) = delivery_result {
            let _ = app.emit("jackvoice://delivery", delivery);
        }
        if phase_is_error {
            crate::overlay::show_overlay(&app);
            schedule_error_overlay_hide(&app, session_id);
        } else if needs_copy_prompt {
            crate::overlay::show_overlay(&app);
        }
    }
}

fn apply_settings_to_ui(ui: &mut UiState, settings: &AppSettings) {
    let previously_had_key = ui.has_volc_api_key;
    ui.has_volc_api_key = !settings.volc_api_key.trim().is_empty();
    if !ui.has_volc_api_key {
        ui.volc_credential_status = VolcCredentialStatus::Missing;
    } else if !previously_had_key {
        ui.volc_credential_status = VolcCredentialStatus::Configured;
    }
    ui.masked_volc_api_key = settings::mask_api_key(&settings.volc_api_key);
    ui.volc_resource_id = settings.volc_resource_id.clone();
    ui.volc_boosting_table_id = settings.volc_boosting_table_id.clone();
    ui.semantic_punctuation_enabled = settings.semantic_punctuation_enabled;
    ui.semantic_smoothing_enabled = settings.semantic_smoothing_enabled;
    ui.max_sentence_silence_ms = settings.max_sentence_silence_ms;
    ui.input_gain_db = settings.input_gain_db;
    ui.selected_input_device_id = settings.selected_input_device_id.clone();
    ui.selected_input_device_name = selected_input_device_display_name(settings).to_string();
    ui.shortcut = settings.shortcut.clone();
    ui.launch_at_login = settings.launch_at_login;
    ui.mute_system_audio_during_dictation = settings.mute_system_audio_during_dictation;
    ui.system_audio_mute_supported = crate::output_mute::supported();
    ui.onboarding_completed = settings.onboarding_completed;
    ui.history_text_size = settings.history_text_size.clone();
}

fn input_device_preference(settings: &AppSettings) -> Option<InputDevicePreference> {
    let id = settings.selected_input_device_id.trim();
    if id.is_empty() {
        None
    } else {
        Some(InputDevicePreference {
            id: id.to_string(),
            name: selected_input_device_display_name(settings).to_string(),
        })
    }
}

fn selected_input_device_display_name(settings: &AppSettings) -> &str {
    if settings.selected_input_device_name.trim().is_empty() {
        settings.selected_input_device_id.trim()
    } else {
        settings.selected_input_device_name.trim()
    }
}

fn find_selected_input_device<'a>(
    devices: &'a [InputDeviceInfo],
    settings: &AppSettings,
) -> Option<&'a InputDeviceInfo> {
    let selected_id = settings.selected_input_device_id.trim();
    if selected_id.is_empty() {
        return None;
    }
    devices
        .iter()
        .find(|device| device.id == selected_id)
        .or_else(|| {
            // Pre-stable-ID releases persisted the display name as the ID.
            (!selected_id.starts_with("coreaudio:"))
                .then(|| devices.iter().find(|device| device.name == selected_id))
                .flatten()
        })
}

fn sync_resolved_input_device(ui: &mut UiState, settings: &AppSettings) {
    let selected = find_selected_input_device(&ui.input_devices, settings);
    ui.selected_input_device_available =
        settings.selected_input_device_id.is_empty() || selected.is_some();
    let resolved = selected.or_else(|| {
        ui.input_devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| ui.input_devices.first())
    });
    if let Some(device) = resolved {
        ui.active_input_device_id = device.id.clone();
        ui.active_input_device_name = device.name.clone();
    } else {
        ui.active_input_device_id.clear();
        ui.active_input_device_name.clear();
    }
    ui.using_input_device_fallback =
        !settings.selected_input_device_id.is_empty() && selected.is_none() && resolved.is_some();
}

fn active_matches_preference(
    preference: &InputDevicePreference,
    device: &audio::ActiveInputDevice,
) -> bool {
    device.id == preference.id
        || (!preference.id.starts_with("coreaudio:") && device.name == preference.id)
}

fn fallback_notice(
    preference: &InputDevicePreference,
    actual: &audio::ActiveInputDevice,
) -> String {
    let preferred_name = if preference.name.trim().is_empty() {
        &preference.id
    } else {
        &preference.name
    };
    format!(
        "首选麦克风「{preferred_name}」未连接，本次使用「{}」",
        actual.name
    )
}

fn update_audio_device_state(
    ui: &mut UiState,
    fallback_state: &mut MicFallbackState,
    preference: Option<&InputDevicePreference>,
    notice: &audio::AudioNotice,
) -> Option<String> {
    match notice {
        audio::AudioNotice::CaptureStarted {
            actual,
            using_fallback,
        } => {
            ui.active_input_device_id = actual.id.clone();
            ui.active_input_device_name = actual.name.clone();
            ui.using_input_device_fallback = *using_fallback;
            ui.selected_input_device_available = preference.is_none() || !using_fallback;
            fallback_state.lost_active_device_id = None;
            if *using_fallback {
                let preference = preference?;
                if fallback_state.unavailable_preference_id.as_deref()
                    == Some(preference.id.as_str())
                {
                    None
                } else {
                    fallback_state.unavailable_preference_id = Some(preference.id.clone());
                    Some(fallback_notice(preference, actual))
                }
            } else {
                fallback_state.unavailable_preference_id = None;
                None
            }
        }
        audio::AudioNotice::DeviceLost { previous } => {
            ui.active_input_device_id.clear();
            ui.active_input_device_name.clear();
            ui.using_input_device_fallback = false;
            if preference.is_some_and(|preferred| active_matches_preference(preferred, previous)) {
                ui.selected_input_device_available = false;
            }
            fallback_state.lost_active_device_id = Some(previous.id.clone());
            Some(format!(
                "麦克风「{}」已断开，正在寻找可用麦克风…",
                previous.name
            ))
        }
        audio::AudioNotice::DeviceChanged {
            previous,
            actual,
            using_fallback,
        } => {
            let recovering_from_loss = fallback_state.lost_active_device_id.take().is_some();
            let preference_was_unavailable = fallback_state.unavailable_preference_id.is_some();
            ui.active_input_device_id = actual.id.clone();
            ui.active_input_device_name = actual.name.clone();
            ui.using_input_device_fallback = *using_fallback;
            ui.selected_input_device_available = preference.is_none() || !using_fallback;

            if *using_fallback {
                let preference = preference?;
                let first_fallback_notice = fallback_state.unavailable_preference_id.as_deref()
                    != Some(preference.id.as_str());
                fallback_state.unavailable_preference_id = Some(preference.id.clone());
                if first_fallback_notice {
                    Some(fallback_notice(preference, actual))
                } else if recovering_from_loss || previous != actual {
                    Some(if previous == actual {
                        format!("备用麦克风「{}」已恢复", actual.name)
                    } else {
                        format!("已继续使用备用麦克风「{}」", actual.name)
                    })
                } else {
                    None
                }
            } else {
                fallback_state.unavailable_preference_id = None;
                if previous == actual {
                    recovering_from_loss.then(|| format!("麦克风「{}」已恢复", actual.name))
                } else if preference_was_unavailable
                    && preference
                        .is_some_and(|preferred| active_matches_preference(preferred, actual))
                {
                    Some(format!("首选麦克风「{}」已恢复", actual.name))
                } else {
                    Some(format!("已切换到麦克风「{}」", actual.name))
                }
            }
        }
    }
}

fn initial_volc_credential_status(api_key: &str, warning: &str) -> VolcCredentialStatus {
    if !api_key.trim().is_empty() {
        VolcCredentialStatus::Configured
    } else if !warning.trim().is_empty() {
        VolcCredentialStatus::Unavailable
    } else {
        VolcCredentialStatus::Missing
    }
}

async fn verify_volc_service(api_key: &str, resource_id: &str) -> Result<String, String> {
    let config = VolcAsrConfig {
        api_key: api_key.trim().to_string(),
        resource_id: resource_id.trim().to_string(),
        // 连接测试只验证 Key 与识别资源，不让可选热词表影响结果。
        boosting_table_id: String::new(),
    };
    match crate::asr::test_connection(config).await {
        Ok(()) => Ok("豆包语音服务验证通过。".into()),
        Err(error) => {
            let raw = error.to_string();
            eprintln!("[volc-connection-test] {raw}");
            Err(format_volc_connection_error(&raw))
        }
    }
}

fn restore_output_mute(mut guard: Option<OutputMuteGuard>) {
    if let Some(guard) = guard.as_mut() {
        if let Err(error) = guard.restore() {
            eprintln!("[output-mute] 恢复系统音频失败：{error}");
        }
    }
}

/// 将服务端/网络层的原始连接错误转成用户能采取行动的提示。
/// 原始错误仍写入开发日志，便于定位火山引擎返回的具体状态。
fn format_volc_connection_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("unauthenticated")
        || raw.contains("未认证")
        || raw.contains("鉴权失败")
    {
        return format!(
            "API Key 未通过认证。请确认填写的是豆包语音控制台“API Key 管理”中的 API Key，而不是 Access Key、Secret Key 或其他产品的凭据。服务端错误：{raw}"
        );
    }
    if lower.contains("403")
        || lower.contains("requested grant not found")
        || raw.contains("服务未授权")
    {
        return format!(
            "豆包语音服务拒绝访问。请确认当前账号已经开通所需的语音识别套餐。服务端错误：{raw}"
        );
    }
    if lower.contains("timed out") || raw.contains("超时") {
        return format!("连接豆包语音服务超时，请检查网络后重试。原始错误：{raw}");
    }
    if raw.contains("资源 ID") {
        return "豆包语音服务配置异常，请重新连接后再试。".into();
    }
    format!("豆包语音连接测试失败：{raw}")
}

/// Keep an error capsule visible long enough to read, then hide it. If a
/// newer session started in the meantime (epoch changed) or the phase moved
/// on, do nothing.
fn schedule_error_overlay_hide(app: &AppHandle, epoch: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(6)).await;
        if let Some(state) = app.try_state::<AppState>() {
            let should_hide = {
                let ui = state.ui.lock();
                state.session_epoch.load(Ordering::SeqCst) == epoch && ui.phase == "error"
            };
            if should_hide {
                crate::overlay::hide_overlay(&app);
            }
        }
    });
}

#[allow(dead_code)]
pub type SharedDelivery = DeliveryResult;

#[cfg(test)]
mod volc_connection_error_tests {
    use super::{
        format_volc_connection_error, initial_volc_credential_status, sync_resolved_input_device,
        update_audio_device_state, AppSettings, MicFallbackState, UiState, VolcCredentialStatus,
        MAX_DICTATION_DURATION,
    };
    use crate::audio::{ActiveInputDevice, AudioNotice, InputDeviceInfo, InputDevicePreference};
    use std::time::Duration;

    #[test]
    fn limits_each_dictation_to_thirty_minutes() {
        assert_eq!(MAX_DICTATION_DURATION, Duration::from_secs(30 * 60));
    }

    #[test]
    fn credential_status_distinguishes_missing_unavailable_and_configured() {
        assert_eq!(
            initial_volc_credential_status("", ""),
            VolcCredentialStatus::Missing
        );
        assert_eq!(
            initial_volc_credential_status("", "keychain unavailable"),
            VolcCredentialStatus::Unavailable
        );
        assert_eq!(
            initial_volc_credential_status("stored-key", "migration warning"),
            VolcCredentialStatus::Configured
        );
    }

    #[test]
    fn explains_unauthenticated_key() {
        let message = format_volc_connection_error("HTTP error: 401 Unauthorized");
        assert!(message.contains("API Key 未通过认证"));
        assert!(message.contains("豆包语音控制台"));
    }

    #[test]
    fn explains_missing_resource_grant() {
        let message = format_volc_connection_error("requested grant not found (403)");
        assert!(message.contains("服务拒绝访问"));
        assert!(message.contains("语音识别套餐"));
    }

    #[test]
    fn preserves_unknown_service_error() {
        let message = format_volc_connection_error("remote closed the connection");
        assert!(message.contains("remote closed the connection"));
    }

    #[test]
    fn offline_preference_is_preserved_while_default_becomes_runtime_fallback() {
        let settings = AppSettings {
            selected_input_device_id: "coreaudio:wireless".into(),
            selected_input_device_name: "Wireless Mic".into(),
            ..AppSettings::default()
        };
        let mut ui = UiState {
            input_devices: vec![InputDeviceInfo {
                id: "coreaudio:macbook".into(),
                name: "MacBook Pro 麦克风".into(),
                is_default: true,
            }],
            ..UiState::default()
        };

        sync_resolved_input_device(&mut ui, &settings);

        assert_eq!(settings.selected_input_device_id, "coreaudio:wireless");
        assert!(!ui.selected_input_device_available);
        assert!(ui.using_input_device_fallback);
        assert_eq!(ui.active_input_device_id, "coreaudio:macbook");
    }

    #[test]
    fn fallback_notice_is_emitted_once_per_offline_episode() {
        let preference = InputDevicePreference {
            id: "coreaudio:wireless".into(),
            name: "Wireless Mic".into(),
        };
        let fallback = ActiveInputDevice {
            id: "coreaudio:macbook".into(),
            name: "MacBook Pro 麦克风".into(),
        };
        let preferred = ActiveInputDevice {
            id: preference.id.clone(),
            name: preference.name.clone(),
        };
        let notice = AudioNotice::CaptureStarted {
            actual: fallback.clone(),
            using_fallback: true,
        };
        let mut ui = UiState::default();
        let mut state = MicFallbackState::default();

        assert!(
            update_audio_device_state(&mut ui, &mut state, Some(&preference), &notice).is_some()
        );
        assert!(
            update_audio_device_state(&mut ui, &mut state, Some(&preference), &notice).is_none()
        );

        let alternate_fallback = AudioNotice::DeviceChanged {
            previous: fallback,
            actual: ActiveInputDevice {
                id: "coreaudio:usb-backup".into(),
                name: "USB 备用麦克风".into(),
            },
            using_fallback: true,
        };
        assert!(update_audio_device_state(
            &mut ui,
            &mut state,
            Some(&preference),
            &alternate_fallback
        )
        .is_some());

        let restored = AudioNotice::CaptureStarted {
            actual: preferred,
            using_fallback: false,
        };
        assert!(
            update_audio_device_state(&mut ui, &mut state, Some(&preference), &restored).is_none()
        );
        assert!(
            update_audio_device_state(&mut ui, &mut state, Some(&preference), &notice).is_some()
        );
    }
}
