use crate::storage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const SHARED_SETTINGS_FILE: &str = "settings.json";
const VARIANT_SETTINGS_FILE: &str = "variant-settings.json";

/// Runtime view composed from shared recognition preferences and per-client
/// operating-system state. The frontend API stays stable while persistence is
/// intentionally split between the production and development identities.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// 豆包流式录音识别所需的 APP Key（X-Api-Key）。
    #[serde(default)]
    pub volc_api_key: String,
    /// 豆包流式录音识别资源 ID，默认小时版 Seed-ASR 2.0。
    #[serde(default = "default_volc_resource_id")]
    pub volc_resource_id: String,
    /// 自学习平台热词表 ID（可选）；为空时使用请求级热词直传。
    #[serde(default)]
    pub volc_boosting_table_id: String,
    pub semantic_punctuation_enabled: bool,
    pub max_sentence_silence_ms: u32,
    #[serde(default)]
    pub selected_input_device_id: String,
    #[serde(default = "default_shortcut")]
    pub shortcut: String,
    #[serde(default)]
    pub launch_at_login: bool,
    #[serde(default)]
    pub input_gain_db: f32,
    #[serde(default)]
    pub onboarding_completed: bool,
    /// 本地 WAV 的保留策略：never / sevenDays / thirtyDays / forever。
    #[serde(default = "default_audio_retention")]
    pub audio_retention: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self::from_parts(SharedSettings::default(), VariantSettings::default(), false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedSettings {
    /// 只用于读取旧版明文设置。序列化时永远不再写回磁盘。
    #[serde(default, skip_serializing)]
    volc_api_key: String,
    #[serde(default = "default_volc_resource_id")]
    volc_resource_id: String,
    #[serde(default)]
    volc_boosting_table_id: String,
    #[serde(default = "default_semantic_punctuation_enabled")]
    semantic_punctuation_enabled: bool,
    #[serde(default = "default_max_sentence_silence_ms")]
    max_sentence_silence_ms: u32,
    #[serde(default)]
    selected_input_device_id: String,
    #[serde(default = "default_shortcut")]
    shortcut: String,
    #[serde(default)]
    input_gain_db: f32,
    #[serde(default)]
    audio_retention: String,
}

impl Default for SharedSettings {
    fn default() -> Self {
        Self {
            volc_api_key: String::new(),
            volc_resource_id: default_volc_resource_id(),
            volc_boosting_table_id: String::new(),
            semantic_punctuation_enabled: default_semantic_punctuation_enabled(),
            max_sentence_silence_ms: default_max_sentence_silence_ms(),
            selected_input_device_id: String::new(),
            shortcut: default_shortcut(),
            input_gain_db: 0.0,
            audio_retention: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VariantSettings {
    #[serde(default)]
    launch_at_login: bool,
    #[serde(default)]
    onboarding_completed: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyVariantSettings {
    launch_at_login: Option<bool>,
    onboarding_completed: Option<bool>,
}

impl AppSettings {
    fn from_parts(
        shared: SharedSettings,
        variant: VariantSettings,
        existing_shared_settings: bool,
    ) -> Self {
        // 早期版本默认永久保存 WAV。旧设置中没有该字段时继续保留，
        // 避免升级过程静默删除已有录音；全新安装才采用 30 天默认值。
        let audio_retention = if shared.audio_retention.trim().is_empty() {
            if existing_shared_settings {
                "forever".to_string()
            } else {
                default_audio_retention()
            }
        } else {
            normalize_audio_retention(&shared.audio_retention)
        };
        Self {
            volc_api_key: shared.volc_api_key,
            volc_resource_id: shared.volc_resource_id,
            volc_boosting_table_id: shared.volc_boosting_table_id,
            semantic_punctuation_enabled: shared.semantic_punctuation_enabled,
            max_sentence_silence_ms: shared.max_sentence_silence_ms,
            selected_input_device_id: shared.selected_input_device_id,
            shortcut: shared.shortcut,
            launch_at_login: variant.launch_at_login,
            input_gain_db: shared.input_gain_db,
            onboarding_completed: variant.onboarding_completed,
            audio_retention,
        }
    }

    fn shared(&self) -> SharedSettings {
        SharedSettings {
            volc_api_key: self.volc_api_key.clone(),
            volc_resource_id: self.volc_resource_id.clone(),
            volc_boosting_table_id: self.volc_boosting_table_id.clone(),
            semantic_punctuation_enabled: self.semantic_punctuation_enabled,
            max_sentence_silence_ms: self.max_sentence_silence_ms,
            selected_input_device_id: self.selected_input_device_id.clone(),
            shortcut: self.shortcut.clone(),
            input_gain_db: self.input_gain_db,
            audio_retention: normalize_audio_retention(&self.audio_retention),
        }
    }

    fn variant(&self) -> VariantSettings {
        VariantSettings {
            launch_at_login: self.launch_at_login,
            onboarding_completed: self.onboarding_completed,
        }
    }
}

pub fn load_settings(
    shared_data_dir: &Path,
    variant_data_dir: &Path,
    production_variant_data_dir: &Path,
) -> Result<AppSettings, String> {
    let shared_path = shared_data_dir.join(SHARED_SETTINGS_FILE);
    let shared_raw = match fs::read_to_string(&shared_path) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("读取 JackVoice 共享设置失败：{error}")),
    };

    let shared = match shared_raw.as_deref() {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| format!("解析 JackVoice 共享设置失败：{error}"))?,
        None => SharedSettings::default(),
    };

    if let Some(raw) = shared_raw.as_deref() {
        migrate_legacy_variant_settings(raw, production_variant_data_dir)?;
    }
    let variant = load_variant_settings(variant_data_dir)?;

    // Rewriting an old combined settings file removes operating-system state
    // from the neutral shared directory after it has been migrated.
    if let Some(raw) = shared_raw.as_deref() {
        let legacy: LegacyVariantSettings = serde_json::from_str(raw).unwrap_or_default();
        if legacy.launch_at_login.is_some() || legacy.onboarding_completed.is_some() {
            write_shared_settings(shared_data_dir, &shared)?;
        }
    }

    Ok(AppSettings::from_parts(
        shared,
        variant,
        shared_raw.is_some(),
    ))
}

pub fn save_settings(
    shared_data_dir: &Path,
    variant_data_dir: &Path,
    settings: &AppSettings,
) -> Result<(), String> {
    write_shared_settings(shared_data_dir, &settings.shared())?;
    write_variant_settings(variant_data_dir, &settings.variant())
}

fn load_variant_settings(variant_data_dir: &Path) -> Result<VariantSettings, String> {
    let path = variant_data_dir.join(VARIANT_SETTINGS_FILE);
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| format!("解析 JackVoice 客户端设置失败：{error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(VariantSettings::default())
        }
        Err(error) => Err(format!("读取 JackVoice 客户端设置失败：{error}")),
    }
}

fn migrate_legacy_variant_settings(raw: &str, production_data_dir: &Path) -> Result<(), String> {
    let destination = production_data_dir.join(VARIANT_SETTINGS_FILE);
    if destination.exists() {
        return Ok(());
    }
    let legacy: LegacyVariantSettings = serde_json::from_str(raw).unwrap_or_default();
    if legacy.launch_at_login.is_none() && legacy.onboarding_completed.is_none() {
        return Ok(());
    }
    write_variant_settings(
        production_data_dir,
        &VariantSettings {
            launch_at_login: legacy.launch_at_login.unwrap_or(false),
            onboarding_completed: legacy.onboarding_completed.unwrap_or(false),
        },
    )
}

fn write_shared_settings(directory: &Path, settings: &SharedSettings) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("序列化 JackVoice 共享设置失败：{error}"))?;
    storage::write_atomic(&directory.join(SHARED_SETTINGS_FILE), &raw, true)
}

fn write_variant_settings(directory: &Path, settings: &VariantSettings) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("序列化 JackVoice 客户端设置失败：{error}"))?;
    storage::write_atomic(&directory.join(VARIANT_SETTINGS_FILE), &raw, true)
}

fn default_shortcut() -> String {
    "Alt+Space".into()
}

fn default_volc_resource_id() -> String {
    "volc.seedasr.sauc.duration".into()
}

fn default_semantic_punctuation_enabled() -> bool {
    true
}

fn default_max_sentence_silence_ms() -> u32 {
    1300
}

fn default_audio_retention() -> String {
    "thirtyDays".into()
}

pub fn normalize_audio_retention(value: &str) -> String {
    match value {
        "never" | "sevenDays" | "thirtyDays" | "forever" => value.to_string(),
        _ => default_audio_retention(),
    }
}

pub fn remove_legacy_settings_backup(directory: &Path) -> Result<(), String> {
    let backup = directory.join("settings.json.bak");
    match fs::remove_file(backup) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("清理旧版明文设置备份失败：{error}")),
    }
}

pub fn mask_api_key(api_key: &str) -> String {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= 8 {
        return "••••••••".to_string();
    }
    let prefix = &trimmed[..4];
    let suffix = &trimmed[trimmed.len() - 4..];
    format!("{prefix}••••{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "jackvoice-settings-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn migrates_combined_settings_and_keeps_dev_onboarding_separate() {
        let root = test_dir("migration");
        let shared = root.join("shared");
        let production = root.join("production");
        let development = root.join("development");
        fs::create_dir_all(&shared).unwrap();
        fs::write(
            shared.join(SHARED_SETTINGS_FILE),
            r#"{
              "volcApiKey": "key",
              "semanticPunctuationEnabled": true,
              "maxSentenceSilenceMs": 1300,
              "launchAtLogin": true,
              "onboardingCompleted": true
            }"#,
        )
        .unwrap();

        let dev = load_settings(&shared, &development, &production).unwrap();
        assert_eq!(dev.volc_api_key, "key");
        assert_eq!(dev.audio_retention, "forever");
        assert!(!dev.launch_at_login);
        assert!(!dev.onboarding_completed);

        let prod = load_settings(&shared, &production, &production).unwrap();
        assert!(prod.launch_at_login);
        assert!(prod.onboarding_completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saves_shared_and_variant_fields_to_different_files() {
        let root = test_dir("split");
        let shared = root.join("shared");
        let variant = root.join("variant");
        let mut settings = AppSettings::default();
        settings.volc_api_key = "secret".into();
        settings.onboarding_completed = true;

        save_settings(&shared, &variant, &settings).unwrap();

        let shared_raw = fs::read_to_string(shared.join(SHARED_SETTINGS_FILE)).unwrap();
        let variant_raw = fs::read_to_string(variant.join(VARIANT_SETTINGS_FILE)).unwrap();
        assert!(!shared_raw.contains("secret"));
        assert!(!shared_raw.contains("volcApiKey"));
        assert!(!shared_raw.contains("onboardingCompleted"));
        assert!(variant_raw.contains("onboardingCompleted"));
        fs::remove_dir_all(root).unwrap();
    }
}
