use crate::asr::{RealtimeSession, TranscriptUpdate, VolcAsrConfig, ASR_ENGINE_NAME};
use crate::audio::{self, AudioCapture, InputDeviceInfo};
use crate::credentials::{CredentialMode, CredentialSource};
use crate::delivery::{self, DeliveryResult};
use crate::history::{AudioRecorder, RecognitionContext};
use crate::output_mute::OutputMuteGuard;
use crate::settings::{self, AppSettings};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

/// Upper bound for the user-facing digital input gain (dB).
pub const MAX_INPUT_GAIN_DB: f32 = 24.0;

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
    pub status: String,
    pub transcript: String,
    pub has_volc_api_key: bool,
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
    pub input_devices: Vec<InputDeviceInfo>,
    pub audio_level: f32,
    pub mic_testing: bool,
    pub last_delivery_message: String,
    /// True when the last dictation ended without a detected insertion,
    /// so the overlay should offer a manual copy button.
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
    pub audio_retention: String,
    pub history_text_size: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            status: "准备就绪。按 Option+Space 开始听写。".into(),
            transcript: String::new(),
            has_volc_api_key: false,
            masked_volc_api_key: String::new(),
            volc_credential_source: CredentialSource::Missing.as_str().into(),
            volc_credential_warning: String::new(),
            volc_resource_id: "volc.seedasr.sauc.duration".into(),
            volc_boosting_table_id: String::new(),
            semantic_punctuation_enabled: true,
            semantic_smoothing_enabled: false,
            max_sentence_silence_ms: 1300,
            input_gain_db: 0.0,
            selected_input_device_id: String::new(),
            input_devices: Vec::new(),
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
            audio_retention: "thirtyDays".into(),
            history_text_size: "standard".into(),
        }
    }
}

struct ActiveSession {
    stop_tx: Option<mpsc::Sender<()>>,
    /// Stop the dedicated audio owner thread (Send).
    audio_stop_tx: Option<std::sync::mpsc::Sender<()>>,
    /// Owns only the mute state applied by this dictation session.
    output_mute: Option<OutputMuteGuard>,
    started_at: Option<Instant>,
}

struct MonitorSession {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
}

pub struct AppState {
    shared_data_dir: PathBuf,
    variant_data_dir: PathBuf,
    credential_mode: CredentialMode,
    settings: Mutex<AppSettings>,
    ui: Mutex<UiState>,
    active: Mutex<Option<ActiveSession>>,
    monitor: Mutex<Option<MonitorSession>>,
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
                        format!("旧版 App Key 本次仍可使用，但无法安全迁移：{error}");
                }
            }
        } else {
            settings.volc_api_key.clear();
        }
        if legacy_migration_complete {
            settings::remove_legacy_settings_backup(&shared_data_dir)?;
        }
        crate::history::apply_audio_retention(&shared_data_dir, &settings.audio_retention)?;
        match crate::output_mute::restore_stale(&shared_data_dir) {
            Ok(true) => eprintln!("[output-mute] 已恢复上次异常退出遗留的系统音频状态"),
            Ok(false) => {}
            Err(error) => eprintln!("[output-mute] 启动恢复失败：{error}"),
        }
        let mut ui = UiState::default();
        apply_settings_to_ui(&mut ui, &settings);
        ui.volc_credential_source = credential_source.as_str().into();
        ui.volc_credential_warning = credential_warning;
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
            cancel_requested: Mutex::new(false),
            session_epoch: AtomicU64::new(0),
        })
    }

    pub fn snapshot(&self) -> UiState {
        self.ui.lock().clone()
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

    pub fn refresh_input_devices(&self) -> Result<UiState, String> {
        let devices = AudioCapture::list_input_devices().map_err(|e| e.to_string())?;
        let mut settings = self.settings.lock();
        let mut ui = self.ui.lock();
        ui.input_devices = devices;

        let selected_still_exists = ui
            .input_devices
            .iter()
            .any(|d| d.id == settings.selected_input_device_id);
        if !selected_still_exists {
            if let Some(default_device) = ui.input_devices.iter().find(|d| d.is_default) {
                settings.selected_input_device_id = default_device.id.clone();
            } else if let Some(first) = ui.input_devices.first() {
                settings.selected_input_device_id = first.id.clone();
            } else {
                settings.selected_input_device_id.clear();
            }
            self.save_settings(&settings)?;
        }

        apply_settings_to_ui(&mut ui, &settings);
        ui.status = format!("已刷新麦克风列表（{} 个）", ui.input_devices.len());
        Ok(ui.clone())
    }

    pub fn set_input_device(&self, device_id: String) -> Result<UiState, String> {
        // Switching mic should release any temporary test capture.
        self.stop_mic_test_internal();

        let devices = {
            let ui = self.ui.lock();
            ui.input_devices.clone()
        };
        if !device_id.trim().is_empty() && !devices.iter().any(|d| d.id == device_id) {
            // Refresh once in case wireless mic just appeared.
            let refreshed = AudioCapture::list_input_devices().map_err(|e| e.to_string())?;
            if !refreshed.iter().any(|d| d.id == device_id) {
                return Err(format!("找不到麦克风：{device_id}"));
            }
            let mut ui = self.ui.lock();
            ui.input_devices = refreshed;
        }

        let mut settings = self.settings.lock();
        settings.selected_input_device_id = device_id.trim().to_string();
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = if settings.selected_input_device_id.is_empty() {
            "已切换到默认麦克风。".into()
        } else {
            format!("已选择麦克风：{}", settings.selected_input_device_id)
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
            (
                settings.selected_input_device_id.clone(),
                settings.input_gain_db,
            )
        };

        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let app_for_notice = app.clone();
        let on_notice = move |notice: audio::AudioNotice| {
            if let Some(state) = app_for_notice.try_state::<AppState>() {
                let message = mic_notice_message(&notice);
                let mut ui = state.ui.lock();
                if ui.mic_testing {
                    ui.status = message;
                    let _ = app_for_notice.emit("jackvoice://state", ui.clone());
                }
            }
        };

        thread::Builder::new()
            .name("jackvoice-mic-test".into())
            .spawn(move || {
                let capture = match AudioCapture::start_with_device(
                    if selected_device.trim().is_empty() {
                        None
                    } else {
                        Some(selected_device)
                    },
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
            ui.status = "麦克风测试中。对着麦说话，确认波形是否跳动。".into();
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
    pub fn save_volc_settings(
        &self,
        api_key: String,
        resource_id: String,
        boosting_table_id: String,
    ) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再修改豆包识别配置。".into());
        }
        let mut settings = self.settings.lock();
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err("请输入豆包语音 App Key。".into());
        }
        let credential_source = crate::credentials::save_volc_api_key(
            self.credential_mode,
            &self.variant_data_dir,
            &api_key,
        )?;
        settings.volc_api_key = api_key;
        let resource_id = resource_id.trim().to_string();
        settings.volc_resource_id = if resource_id.is_empty() {
            "volc.seedasr.sauc.duration".into()
        } else {
            resource_id
        };
        settings.volc_boosting_table_id = boosting_table_id.trim().to_string();
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.volc_credential_source = credential_source.as_str().into();
        ui.volc_credential_warning.clear();
        ui.status = "豆包语音 App Key 已保存。".into();
        Ok(ui.clone())
    }

    /// 显式移除当前应用身份保存的凭据，避免空输入框被误当成删除操作。
    pub fn remove_volc_api_key(&self) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再移除 App Key。".into());
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
        ui.volc_credential_warning.clear();
        ui.status = "豆包语音 App Key 已移除。".into();
        Ok(ui.clone())
    }

    /// 使用真实识别服务验证 App Key 与资源 ID，但不打开麦克风或发送音频。
    /// 输入框为空时测试当前已保存的 App Key，便于用户随时重新验证。
    pub async fn test_volc_connection(
        &self,
        api_key: String,
        resource_id: String,
    ) -> Result<String, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请先结束听写再测试识别连接。".into());
        }

        let settings = self.settings.lock().clone();
        let api_key = if api_key.trim().is_empty() {
            settings.volc_api_key.trim().to_string()
        } else {
            api_key.trim().to_string()
        };
        if api_key.is_empty() {
            return Err("请先填写或保存豆包语音 App Key。".into());
        }
        let resource_id = if resource_id.trim().is_empty() {
            settings.volc_resource_id.trim().to_string()
        } else {
            resource_id.trim().to_string()
        };
        let config = VolcAsrConfig {
            api_key,
            resource_id,
            // 连接测试只验证 Key 与识别资源，不让可选热词表影响结果。
            boosting_table_id: String::new(),
        };

        match crate::asr::test_connection(config).await {
            Ok(()) => {
                let message = "豆包语音服务连接正常。".to_string();
                self.ui.lock().status = message.clone();
                Ok(message)
            }
            Err(error) => {
                let raw = error.to_string();
                eprintln!("[volc-connection-test] {raw}");
                let message = format_volc_connection_error(&raw);
                self.ui.lock().status = message.clone();
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
            return Err("请先填写豆包语音 App Key。".into());
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
        settings.max_sentence_silence_ms = max_sentence_silence_ms.max(200);
        self.save_settings(&settings)?;
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = "识别参数已保存。".into();
        Ok(ui.clone())
    }

    pub fn set_audio_retention(&self, retention: String) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Err("正在听写中，请结束后再修改录音保留策略。".into());
        }
        let mut settings = self.settings.lock();
        let previous = settings.audio_retention.clone();
        settings.audio_retention = settings::normalize_audio_retention(retention.trim());
        self.save_settings(&settings)?;
        if let Err(error) =
            crate::history::apply_audio_retention(&self.shared_data_dir, &settings.audio_retention)
        {
            settings.audio_retention = previous;
            if let Err(rollback_error) = self.save_settings(&settings) {
                return Err(format!("{error}；恢复原录音策略时也失败：{rollback_error}"));
            }
            return Err(error);
        }
        let mut ui = self.ui.lock();
        apply_settings_to_ui(&mut ui, &settings);
        ui.status = "录音保留策略已更新。".into();
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
        *self.active.lock() = None;
        Ok(self.snapshot())
    }

    pub async fn toggle(&self, app: AppHandle) -> Result<UiState, String> {
        let is_active = self.active.lock().is_some();
        if is_active {
            self.stop(app).await
        } else {
            self.start(app).await
        }
    }

    async fn start(&self, app: AppHandle) -> Result<UiState, String> {
        if self.active.lock().is_some() {
            return Ok(self.snapshot());
        }

        // A new session attempt begins; any pending error-capsule auto-hide
        // from a previous attempt must not touch this one.
        let epoch = self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1;

        // Dictation takes over the mic; stop temporary test capture first.
        self.stop_mic_test_internal();
        *self.cancel_requested.lock() = false;

        let settings = self.settings.lock().clone();
        if settings.volc_api_key.trim().is_empty() {
            let mut ui = self.ui.lock();
            ui.phase = "error".into();
            ui.status = if ui.volc_credential_warning.is_empty() {
                "尚未连接豆包语音，请打开 JackVoice 设置 → 识别。".into()
            } else {
                ui.volc_credential_warning.clone()
            };
            ui.audio_level = 0.0;
            let message = ui.status.clone();
            let _ = app.emit("jackvoice://state", ui.clone());
            drop(ui);
            crate::overlay::show_overlay(&app);
            schedule_error_overlay_hide(&app, epoch);
            return Err(message);
        }

        {
            let mut ui = self.ui.lock();
            ui.phase = "connecting".into();
            ui.status = format!("正在连接{ASR_ENGINE_NAME}…");
            ui.transcript.clear();
            ui.last_delivery_message.clear();
            ui.audio_level = 0.0;
            ui.needs_copy_prompt = false;
            let _ = app.emit("jackvoice://state", ui.clone());
            crate::overlay::show_overlay(&app);
        }

        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
        let (audio_stop_tx, audio_stop_rx) = std::sync::mpsc::channel::<()>();
        let (audio_ready_tx, audio_ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let selected_device = settings.selected_input_device_id.clone();
        let dictation_gain_db = settings.input_gain_db;

        // Microphone fault notices become a transient toast on the capsule.
        let app_for_notice = app.clone();
        let on_notice = move |notice: audio::AudioNotice| {
            if let Some(state) = app_for_notice.try_state::<AppState>() {
                let message = mic_notice_message(&notice);
                let mut ui = state.ui.lock();
                ui.mic_notice = message;
                ui.mic_notice_seq = ui.mic_notice_seq.wrapping_add(1);
                let _ = app_for_notice.emit("jackvoice://state", ui.clone());
            }
        };

        // Own AudioCapture fully on a dedicated thread so AppState remains Send+Sync.
        thread::Builder::new()
            .name("jackvoice-audio-owner".into())
            .spawn(move || {
                let capture = match AudioCapture::start_with_device(
                    if selected_device.trim().is_empty() {
                        None
                    } else {
                        Some(selected_device)
                    },
                    dictation_gain_db,
                    move |pcm| {
                        let _ = audio_tx.try_send(pcm);
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
            })
            .map_err(|e| format!("无法启动录音线程：{e}"))?;

        match audio_ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let mut ui = self.ui.lock();
                ui.phase = "error".into();
                ui.status = err.clone();
                ui.audio_level = 0.0;
                let _ = app.emit("jackvoice://state", ui.clone());
                // Keep the capsule up for a few seconds so the user can read
                // why dictation did not start, instead of a silent flash.
                schedule_error_overlay_hide(&app, epoch);
                return Err(err);
            }
            Err(_) => {
                let err = "录音线程启动失败。".to_string();
                let mut ui = self.ui.lock();
                ui.phase = "error".into();
                ui.status = err.clone();
                ui.audio_level = 0.0;
                let _ = app.emit("jackvoice://state", ui.clone());
                schedule_error_overlay_hide(&app, epoch);
                return Err(err);
            }
        }

        let output_mute = if settings.mute_system_audio_during_dictation {
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

        {
            let mut active = self.active.lock();
            *active = Some(ActiveSession {
                stop_tx: Some(stop_tx),
                audio_stop_tx: Some(audio_stop_tx),
                output_mute,
                started_at: Some(Instant::now()),
            });
        }

        let app_for_task = app.clone();
        let punctuation = settings.semantic_punctuation_enabled;
        let smoothing = settings.semantic_smoothing_enabled;
        let silence = settings.max_sentence_silence_ms;
        let volc_config = VolcAsrConfig {
            api_key: settings.volc_api_key.clone(),
            resource_id: settings.volc_resource_id.clone(),
            boosting_table_id: settings.volc_boosting_table_id.clone(),
        };
        // 热词随每次会话下发：平台热词表优先，否则请求级直传。
        let dictionary = crate::hotwords::load(&self.shared_data_dir);
        let hotwords = crate::hotwords::recognition_words(&dictionary);
        let replacement_rules = crate::hotwords::user_replacement_rules(&self.shared_data_dir);
        let record_id = uuid::Uuid::new_v4().to_string();
        let recording_dir = self.shared_data_dir.clone();
        let save_audio = crate::history::should_save_audio(&settings.audio_retention);
        let recognition_context = RecognitionContext {
            hotwords: hotwords.clone(),
            semantic_punctuation_enabled: punctuation,
            semantic_smoothing_enabled: smoothing,
            max_sentence_silence_ms: silence,
            input_gain_db: settings.input_gain_db,
            input_device_id: settings.selected_input_device_id.clone(),
        };

        tokio::spawn(async move {
            let session_result =
                RealtimeSession::connect(volc_config, punctuation, smoothing, silence, hotwords, {
                    let app = app_for_task.clone();
                    let replacement_rules_for_updates = replacement_rules.clone();
                    move |update: TranscriptUpdate| {
                        if let Some(state) = app.try_state::<AppState>() {
                            let mut ui = state.ui.lock();
                            ui.phase = "recording".into();
                            ui.transcript = crate::hotwords::apply_replacements(
                                &update.text,
                                &replacement_rules_for_updates,
                            );
                            ui.status = if update.is_final_sentence {
                                "正在听写 · 已生成句子".into()
                            } else {
                                "正在听写 · 实时转写中".into()
                            };
                            let _ = app.emit("jackvoice://state", ui.clone());
                        }
                    }
                })
                .await;

            let session = match session_result {
                Ok(session) => session,
                Err(err) => {
                    if let Some(state) = app_for_task.try_state::<AppState>() {
                        if let Some(active) = state.active.lock().take() {
                            if let Some(tx) = active.audio_stop_tx {
                                let _ = tx.send(());
                            }
                        }
                        let mut ui = state.ui.lock();
                        ui.phase = "error".into();
                        ui.status = err.to_string();
                        ui.audio_level = 0.0;
                        let _ = app_for_task.emit("jackvoice://state", ui.clone());
                        // Linger so the error can be read; focus still goes back.
                        schedule_error_overlay_hide(&app_for_task, epoch);
                        crate::overlay::ensure_main_stays_in_background(&app_for_task);
                    }
                    return;
                }
            };

            if let Some(state) = app_for_task.try_state::<AppState>() {
                let mut ui = state.ui.lock();
                ui.phase = "recording".into();
                ui.status = "正在听写。可自由切换应用，预览窗会持续更新。".into();
                let _ = app_for_task.emit("jackvoice://state", ui.clone());
            }

            // 保存的是经过重采样和增益处理、实际送入 ASR 的 16 kHz 单声道
            // PCM16。流式落盘避免长听写把整段音频留在内存里。
            let mut audio_recorder = if save_audio {
                match AudioRecorder::create(&recording_dir, &record_id) {
                    Ok(recorder) => Some(recorder),
                    Err(err) => {
                        eprintln!("[history] 无法开始保存本次录音：{err}");
                        None
                    }
                }
            } else {
                None
            };

            let mut last_level_emit = Instant::now() - Duration::from_millis(100);
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => {
                        break;
                    }
                    maybe_audio = audio_rx.recv() => {
                        match maybe_audio {
                            Some(pcm) => {
                                let level = audio::pcm16_level(&pcm);
                                if let Some(state) = app_for_task.try_state::<AppState>() {
                                    let mut ui = state.ui.lock();
                                    ui.audio_level = level;
                                    // Throttle waveform updates so UI stays smooth.
                                    if last_level_emit.elapsed() >= Duration::from_millis(50) {
                                        last_level_emit = Instant::now();
                                        let _ = app_for_task.emit("jackvoice://level", level);
                                        let _ = app_for_task.emit("jackvoice://state", ui.clone());
                                    }
                                }

                                if let Some(recorder) = audio_recorder.as_mut() {
                                    if let Err(err) = recorder.write_pcm(&pcm) {
                                        eprintln!("[history] 保存录音中断：{err}");
                                        // Drop 会删除未完成的 .part 文件；文本识别继续。
                                        audio_recorder = None;
                                    }
                                }

                                if let Err(err) = session.send_audio(pcm).await {
                                    if let Some(state) = app_for_task.try_state::<AppState>() {
                                        if let Some(active) = state.active.lock().take() {
                                            if let Some(tx) = active.audio_stop_tx {
                                                let _ = tx.send(());
                                            }
                                        }
                                        let mut ui = state.ui.lock();
                                        ui.phase = "error".into();
                                        ui.status = err.to_string();
                                        ui.audio_level = 0.0;
                                        let _ = app_for_task.emit("jackvoice://state", ui.clone());
                                        schedule_error_overlay_hide(&app_for_task, epoch);
                        crate::overlay::ensure_main_stays_in_background(&app_for_task);
                                    }
                                    return;
                                }
                            }
                            None => {
                                break;
                            }
                        }
                    }
                }
            }

            // Stop microphone before finalization.
            if let Some(state) = app_for_task.try_state::<AppState>() {
                let (audio_stop_tx, output_mute) = {
                    let mut active = state.active.lock();
                    match active.as_mut() {
                        Some(active) => (active.audio_stop_tx.take(), active.output_mute.take()),
                        None => (None, None),
                    }
                };
                if let Some(tx) = audio_stop_tx {
                    let _ = tx.send(());
                }
                restore_output_mute(output_mute);
                let mut ui = state.ui.lock();
                ui.audio_level = 0.0;
                ui.phase = "finalizing".into();
                ui.status = "正在等待最终结果…".into();
                let _ = app_for_task.emit("jackvoice://level", 0.0_f32);
                let _ = app_for_task.emit("jackvoice://state", ui.clone());
            }

            let is_cancel = app_for_task
                .try_state::<AppState>()
                .map(|s| *s.cancel_requested.lock())
                .unwrap_or(false);

            if is_cancel {
                // Silent cancel: discard audio/result, no copy, no paste.
                session.cancel().await;
                if let Some(state) = app_for_task.try_state::<AppState>() {
                    let mut ui = state.ui.lock();
                    ui.phase = "idle".into();
                    ui.status = "已取消本次听写。".into();
                    ui.audio_level = 0.0;
                    let _ = app_for_task.emit("jackvoice://state", ui.clone());
                    *state.active.lock() = None;
                    crate::overlay::hide_overlay(&app_for_task);
                    crate::overlay::ensure_main_stays_in_background(&app_for_task);
                }
                return;
            }

            match session.finish().await {
                Ok(final_text) => {
                    let final_text =
                        crate::hotwords::apply_replacements(&final_text, &replacement_rules);
                    let saved_audio = if final_text.trim().is_empty() {
                        None
                    } else {
                        audio_recorder
                            .take()
                            .and_then(|recorder| match recorder.finish() {
                                Ok(audio) => Some(audio),
                                Err(err) => {
                                    eprintln!("[history] 无法完成本次录音：{err}");
                                    None
                                }
                            })
                    };
                    if let Some(state) = app_for_task.try_state::<AppState>() {
                        let elapsed_ms = state
                            .active
                            .lock()
                            .as_ref()
                            .and_then(|a| a.started_at)
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        // 有音频时以采样数计算的时长为准；它比包含联网和收尾等待的
                        // 墙钟耗时更适合作为口述统计与后续识别效率基准。
                        let duration_ms = saved_audio
                            .as_ref()
                            .map(|audio| audio.duration_ms)
                            .unwrap_or(elapsed_ms);
                        if !final_text.trim().is_empty() {
                            if let Err(err) = crate::history::append(
                                &state.shared_data_dir,
                                record_id,
                                &final_text,
                                duration_ms,
                                saved_audio,
                                Some(recognition_context),
                            ) {
                                eprintln!("[history] 保存听写记录失败：{err}");
                            }
                            let _ = app_for_task.emit("jackvoice://history", true);
                        }
                        // Before disturbing focus in any way, check whether the app
                        // that will receive the paste actually has a focused text
                        // insertion point. If not, we skip auto-paste entirely so
                        // text never lands in some unintended input box.
                        let probe = if final_text.trim().is_empty() {
                            delivery::InsertionProbe::Unknown
                        } else {
                            // Refresh once more: finalization takes a moment
                            // and the user may have moved to another app.
                            crate::overlay::remember_frontmost_app();
                            let target = crate::overlay::remembered_frontmost_app();
                            delivery::probe_insertion_target(target.as_deref())
                        };

                        // Hide capsule and hand focus back to the user's app FIRST,
                        // so the auto-paste lands in the right place.
                        crate::overlay::hide_overlay(&app_for_task);
                        crate::overlay::ensure_main_stays_in_background(&app_for_task);
                        tokio::time::sleep(Duration::from_millis(350)).await;

                        // deliver_text copies first, then best-effort pastes.
                        let delivery = delivery::deliver_text(&app_for_task, &final_text, probe);
                        // When insertion was not detected, bring the capsule back with a
                        // manual "copy" affordance so nothing is silently lost.
                        let needs_copy_prompt = !delivery.pasted && !final_text.trim().is_empty();
                        let mut ui = state.ui.lock();
                        ui.phase = "idle".into();
                        ui.transcript = final_text;
                        ui.status = "听写结束。".into();
                        ui.audio_level = 0.0;
                        ui.last_delivery_message = delivery.message.clone();
                        ui.needs_copy_prompt = needs_copy_prompt;
                        let _ = app_for_task.emit("jackvoice://state", ui.clone());
                        let _ = app_for_task.emit("jackvoice://delivery", delivery);
                        drop(ui);
                        if needs_copy_prompt {
                            crate::overlay::show_overlay(&app_for_task);
                        }
                        *state.active.lock() = None;
                    }
                }
                Err(err) => {
                    let raw = err.to_string();
                    eprintln!("[asr-connect] {raw}");
                    let message = format_volc_connection_error(&raw);
                    if let Some(state) = app_for_task.try_state::<AppState>() {
                        let mut ui = state.ui.lock();
                        ui.phase = "error".into();
                        ui.status = message;
                        ui.audio_level = 0.0;
                        let _ = app_for_task.emit("jackvoice://state", ui.clone());
                        *state.active.lock() = None;
                        schedule_error_overlay_hide(&app_for_task, epoch);
                        crate::overlay::ensure_main_stays_in_background(&app_for_task);
                    }
                }
            }
        });

        Ok(self.snapshot())
    }

    async fn stop(&self, app: AppHandle) -> Result<UiState, String> {
        // The user may have switched apps during dictation; re-capture the
        // app that is frontmost right now so the final paste targets the
        // window they are actually typing in.
        crate::overlay::remember_frontmost_app();

        let (stop_tx, audio_stop_tx, output_mute) = {
            let mut active = self.active.lock();
            match active.as_mut() {
                Some(session) => (
                    session.stop_tx.take(),
                    session.audio_stop_tx.take(),
                    session.output_mute.take(),
                ),
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
            ui.status = "正在结束听写并收尾…".into();
            ui.audio_level = 0.0;
            let _ = app.emit("jackvoice://level", 0.0_f32);
            let _ = app.emit("jackvoice://state", ui.clone());
        }

        if let Some(tx) = stop_tx {
            let _ = tx.send(()).await;
        }

        Ok(self.snapshot())
    }
}

fn apply_settings_to_ui(ui: &mut UiState, settings: &AppSettings) {
    ui.has_volc_api_key = !settings.volc_api_key.trim().is_empty();
    ui.masked_volc_api_key = settings::mask_api_key(&settings.volc_api_key);
    ui.volc_resource_id = settings.volc_resource_id.clone();
    ui.volc_boosting_table_id = settings.volc_boosting_table_id.clone();
    ui.semantic_punctuation_enabled = settings.semantic_punctuation_enabled;
    ui.semantic_smoothing_enabled = settings.semantic_smoothing_enabled;
    ui.max_sentence_silence_ms = settings.max_sentence_silence_ms;
    ui.input_gain_db = settings.input_gain_db;
    ui.selected_input_device_id = settings.selected_input_device_id.clone();
    ui.shortcut = settings.shortcut.clone();
    ui.launch_at_login = settings.launch_at_login;
    ui.mute_system_audio_during_dictation = settings.mute_system_audio_during_dictation;
    ui.system_audio_mute_supported = crate::output_mute::supported();
    ui.onboarding_completed = settings.onboarding_completed;
    ui.audio_retention = settings.audio_retention.clone();
    ui.history_text_size = settings.history_text_size.clone();
}

fn restore_output_mute(mut guard: Option<OutputMuteGuard>) {
    if let Some(guard) = guard.as_mut() {
        if let Err(error) = guard.restore() {
            eprintln!("[output-mute] 恢复系统音频失败：{error}");
        }
    }
}

/// Human-readable text for microphone fault notices.
fn mic_notice_message(notice: &audio::AudioNotice) -> String {
    match notice {
        audio::AudioNotice::FallbackAtStart { requested, actual } => {
            format!("麦克风「{requested}」不可用，已切换到「{actual}」")
        }
        audio::AudioNotice::DeviceLost { previous } => {
            format!("麦克风「{previous}」已断开，正在尝试重新连接…")
        }
        audio::AudioNotice::DeviceRestored { previous, actual } => {
            if previous == actual {
                format!("麦克风「{actual}」已恢复")
            } else {
                format!("麦克风「{previous}」已断开，已切换到「{actual}」")
            }
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
            "App Key 未通过认证。请确认填写的是豆包语音控制台中的 App Key，而不是 Access Key、Secret Key 或其他产品的凭据。服务端错误：{raw}"
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
    use super::format_volc_connection_error;

    #[test]
    fn explains_unauthenticated_key() {
        let message = format_volc_connection_error("HTTP error: 401 Unauthorized");
        assert!(message.contains("App Key 未通过认证"));
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
}
