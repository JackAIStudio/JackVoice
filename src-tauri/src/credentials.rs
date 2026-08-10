use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const PRODUCTION_SERVICE: &str = "com.jackvoice.app";
const DEVELOPMENT_SERVICE: &str = "com.jackvoice.app.dev";
const LEGACY_SHARED_SERVICE: &str = "com.jackvoice.shared";
const VOLC_API_KEY_ACCOUNT: &str = "volc-api-key";
const DEVELOPMENT_CREDENTIAL_FILE: &str = "dev-credentials.json";
const DEVELOPMENT_MIGRATION_MARKER_FILE: &str = ".dev-credential-migration-complete";
pub const DEVELOPMENT_ENV_VAR: &str = "JACKVOICE_VOLC_API_KEY";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevelopmentCredentials {
    volc_api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    Production,
    Development,
}

impl CredentialMode {
    pub fn from_identifier(identifier: &str) -> Self {
        if identifier == crate::storage::PRODUCTION_IDENTIFIER {
            Self::Production
        } else {
            Self::Development
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Missing,
    SystemStore,
    DevelopmentFile,
    Environment,
    Session,
    LegacyMigration,
}

impl CredentialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::SystemStore => "systemStore",
            Self::DevelopmentFile => "developmentFile",
            Self::Environment => "environment",
            Self::Session => "session",
            Self::LegacyMigration => "legacyMigration",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialLoad {
    pub value: String,
    pub source: CredentialSource,
    /// A credential failure is recoverable: callers should surface this message
    /// in the app UI, never fail the complete application startup.
    pub warning: String,
}

impl CredentialLoad {
    fn missing(warning: impl Into<String>) -> Self {
        Self {
            value: String::new(),
            source: CredentialSource::Missing,
            warning: warning.into(),
        }
    }
}

trait SecretStore {
    fn load(&self, service: &str) -> Result<Option<String>, String>;
    fn save(&self, service: &str, value: &str) -> Result<(), String>;
}

struct NativeSecretStore;

impl SecretStore for NativeSecretStore {
    fn load(&self, service: &str) -> Result<Option<String>, String> {
        with_noninteractive_system_store(|| match entry(service)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("无法从系统凭据库读取豆包 App Key：{error}")),
        })
    }

    fn save(&self, service: &str, value: &str) -> Result<(), String> {
        with_noninteractive_system_store(|| {
            let entry = entry(service)?;
            let value = value.trim();
            if value.is_empty() {
                return match entry.delete_credential() {
                    Ok(()) | Err(Error::NoEntry) => Ok(()),
                    Err(error) => Err(format!("无法从系统凭据库删除豆包 App Key：{error}")),
                };
            }
            entry
                .set_password(value)
                .map_err(|error| format!("无法把豆包 App Key 保存到系统凭据库：{error}"))
        })
    }
}

fn entry(service: &str) -> Result<Entry, String> {
    #[cfg(target_os = "windows")]
    {
        use std::collections::HashMap;

        // API keys should stay on this computer. The Windows store defaults to
        // Enterprise persistence, which may roam with a managed user profile.
        return Entry::new_with_modifiers(
            service,
            VOLC_API_KEY_ACCOUNT,
            &HashMap::from([("persistence", "Local")]),
        )
        .map_err(|error| format!("无法访问系统凭据库：{error}"));
    }

    #[cfg(not(target_os = "windows"))]
    Entry::new(service, VOLC_API_KEY_ACCOUNT)
        .map_err(|error| format!("无法访问系统凭据库：{error}"))
}

#[cfg(target_os = "macos")]
fn with_noninteractive_system_store<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    use security_framework::os::macos::keychain::SecKeychain;
    use std::sync::Mutex;

    // SecKeychain's interaction flag is process-global. Serialize the short
    // operations and use the RAII guard so it is always restored afterwards.
    static INTERACTION_GUARD: Mutex<()> = Mutex::new(());
    let _serial = INTERACTION_GUARD
        .lock()
        .map_err(|_| "系统凭据库访问锁已损坏。".to_string())?;
    let _no_ui = SecKeychain::disable_user_interaction()
        .map_err(|error| format!("无法关闭系统凭据库交互界面：{error}"))?;
    operation()
}

#[cfg(not(target_os = "macos"))]
fn with_noninteractive_system_store<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    operation()
}

/// Load the credential appropriate for the current application identity.
///
/// Development first honors an explicit environment override, then reads a
/// private file from its isolated app-data directory. A local file is used for
/// Debug builds because macOS can reject Keychain access whenever an ad-hoc
/// development binary is rebuilt. Production keeps using the OS credential
/// store. All failures are recoverable and never abort application startup.
pub fn load_volc_api_key(mode: CredentialMode, variant_data_dir: &Path) -> CredentialLoad {
    let env_value = std::env::var_os(DEVELOPMENT_ENV_VAR);
    match mode {
        CredentialMode::Development => load_development(
            env_value,
            &development_credential_path(variant_data_dir),
            &NativeSecretStore,
        ),
        CredentialMode::Production => load_production(&NativeSecretStore),
    }
}

fn load_development(
    development_env: Option<OsString>,
    path: &Path,
    legacy_store: &impl SecretStore,
) -> CredentialLoad {
    if let Some(value) = development_env {
        match value.into_string() {
            Ok(value) if !value.trim().is_empty() => {
                return CredentialLoad {
                    value: value.trim().to_string(),
                    source: CredentialSource::Environment,
                    warning: String::new(),
                };
            }
            Ok(_) => {}
            Err(_) => {
                return CredentialLoad::missing(format!(
                    "开发环境变量 {DEVELOPMENT_ENV_VAR} 不是有效文本，已忽略。"
                ));
            }
        }
    }

    match load_development_file(path) {
        Ok(Some(value)) => {
            let _ = mark_development_migration_complete(path);
            return CredentialLoad {
                value,
                source: CredentialSource::DevelopmentFile,
                warning: String::new(),
            };
        }
        Ok(None) => {}
        Err(error) => {
            return CredentialLoad::missing(format!(
                "开发版本地凭据暂不可用，JackVoice 已继续启动。请重新填写 App Key。{error}"
            ));
        }
    }

    // Migration is intentionally one-shot. In particular, an explicit key
    // removal must not resurrect an old development Keychain entry on the next
    // launch. The first completed check leaves a private marker beside the
    // development credential file.
    if development_migration_marker_path(path).is_file() {
        return CredentialLoad::missing(
            "开发版尚未连接豆包语音。请在设置中填写 App Key；保存后会写入开发版私有凭据文件。",
        );
    }

    // One-time best-effort migration from the earlier development Keychain
    // entry. Access remains non-interactive; an inaccessible stale item is
    // simply ignored so rebuilds never show a system password prompt.
    if let Ok(Some(value)) = legacy_store.load(DEVELOPMENT_SERVICE) {
        let value = value.trim().to_string();
        if !value.is_empty() && save_development_file(path, &value).is_ok() {
            return CredentialLoad {
                value,
                source: CredentialSource::DevelopmentFile,
                warning: String::new(),
            };
        }
    }

    let _ = mark_development_migration_complete(path);

    CredentialLoad::missing(
        "开发版尚未连接豆包语音。请在设置中填写 App Key；保存后会写入开发版私有凭据文件。",
    )
}

fn load_production(store: &impl SecretStore) -> CredentialLoad {
    match store.load(PRODUCTION_SERVICE) {
        Ok(Some(value)) if !value.trim().is_empty() => CredentialLoad {
            value: value.trim().to_string(),
            source: CredentialSource::SystemStore,
            warning: String::new(),
        },
        Ok(_) => migrate_legacy_credential(store),
        Err(error) => CredentialLoad::missing(format!(
            "系统凭据库暂时不可用，JackVoice 已继续启动。请在设置中重新填写 App Key。{error}"
        )),
    }
}

fn development_credential_path(variant_data_dir: &Path) -> PathBuf {
    variant_data_dir.join(DEVELOPMENT_CREDENTIAL_FILE)
}

fn development_migration_marker_path(credential_path: &Path) -> PathBuf {
    credential_path.with_file_name(DEVELOPMENT_MIGRATION_MARKER_FILE)
}

fn mark_development_migration_complete(credential_path: &Path) -> Result<(), String> {
    crate::storage::write_atomic(
        &development_migration_marker_path(credential_path),
        b"version=1\n",
        false,
    )
}

fn load_development_file(path: &Path) -> Result<Option<String>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取开发版凭据文件失败：{error}")),
    };
    let credentials: DevelopmentCredentials =
        serde_json::from_str(&raw).map_err(|error| format!("解析开发版凭据文件失败：{error}"))?;
    let value = credentials.volc_api_key.trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

fn save_development_file(path: &Path, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("删除开发版凭据文件失败：{error}")),
        }?;
        return mark_development_migration_complete(path);
    }
    let raw = serde_json::to_vec_pretty(&DevelopmentCredentials {
        volc_api_key: value.to_string(),
    })
    .map_err(|error| format!("序列化开发版凭据失败：{error}"))?;
    crate::storage::write_atomic(path, &raw, false)?;
    mark_development_migration_complete(path)
}

fn migrate_legacy_credential(store: &impl SecretStore) -> CredentialLoad {
    match store.load(LEGACY_SHARED_SERVICE) {
        Ok(Some(value)) if !value.trim().is_empty() => {
            let value = value.trim().to_string();
            match store.save(PRODUCTION_SERVICE, &value) {
                Ok(()) => CredentialLoad {
                    value,
                    source: CredentialSource::LegacyMigration,
                    warning: String::new(),
                },
                Err(error) => CredentialLoad {
                    value,
                    source: CredentialSource::LegacyMigration,
                    warning: format!(
                        "旧版 App Key 本次可用，但迁移到新的正式版凭据条目失败：{error}"
                    ),
                },
            }
        }
        Ok(_) => CredentialLoad::missing(String::new()),
        Err(error) => CredentialLoad::missing(format!(
            "检测旧版凭据时遇到问题，已跳过且不会弹出系统密码框。请在设置中重新填写 App Key。{error}"
        )),
    }
}

/// Save a user-entered credential for the current build identity.
pub fn save_volc_api_key(
    mode: CredentialMode,
    variant_data_dir: &Path,
    value: &str,
) -> Result<CredentialSource, String> {
    match mode {
        CredentialMode::Development => {
            save_development_file(&development_credential_path(variant_data_dir), value)?;
            Ok(if value.trim().is_empty() {
                CredentialSource::Missing
            } else {
                CredentialSource::DevelopmentFile
            })
        }
        CredentialMode::Production => save_production(value, &NativeSecretStore),
    }
}

fn save_production(value: &str, store: &impl SecretStore) -> Result<CredentialSource, String> {
    store.save(PRODUCTION_SERVICE, value)?;
    Ok(if value.trim().is_empty() {
        CredentialSource::Missing
    } else {
        CredentialSource::SystemStore
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jackvoice-credentials-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[derive(Default)]
    struct FakeStore {
        values: RefCell<HashMap<String, String>>,
        load_error: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
    }

    impl SecretStore for FakeStore {
        fn load(&self, service: &str) -> Result<Option<String>, String> {
            self.calls.borrow_mut().push(format!("load:{service}"));
            if let Some(error) = self.load_error.borrow().clone() {
                return Err(error);
            }
            Ok(self.values.borrow().get(service).cloned())
        }

        fn save(&self, service: &str, value: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("save:{service}"));
            self.values
                .borrow_mut()
                .insert(service.to_string(), value.to_string());
            Ok(())
        }
    }

    #[test]
    fn development_environment_overrides_file_and_legacy_store() {
        let dir = test_dir("env");
        let path = development_credential_path(&dir);
        save_development_file(&path, "file-secret").unwrap();
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(DEVELOPMENT_SERVICE.into(), "stored-secret".into());
        let loaded = load_development(Some(OsString::from(" dev-secret ")), &path, &store);
        assert_eq!(loaded.value, "dev-secret");
        assert_eq!(loaded.source, CredentialSource::Environment);
        assert!(store.calls.borrow().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_loads_private_file_without_production_store() {
        let dir = test_dir("file");
        let path = development_credential_path(&dir);
        save_development_file(&path, "development-secret").unwrap();
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(PRODUCTION_SERVICE.into(), "production-secret".into());
        let loaded = load_development(None, &path, &store);
        assert_eq!(loaded.value, "development-secret");
        assert_eq!(loaded.source, CredentialSource::DevelopmentFile);
        assert!(store.calls.borrow().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_missing_key_is_recoverable() {
        let dir = test_dir("missing");
        let store = FakeStore::default();
        let loaded = load_development(None, &development_credential_path(&dir), &store);
        assert!(loaded.value.is_empty());
        assert_eq!(loaded.source, CredentialSource::Missing);
        assert!(loaded.warning.contains("设置中填写"));
        assert_eq!(
            store.calls.borrow().as_slice(),
            [format!("load:{DEVELOPMENT_SERVICE}")]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_inaccessible_legacy_store_is_recoverable() {
        let dir = test_dir("legacy-denied");
        let store = FakeStore::default();
        *store.load_error.borrow_mut() = Some("interaction not allowed".into());
        let loaded = load_development(None, &development_credential_path(&dir), &store);
        assert!(loaded.value.is_empty());
        assert_eq!(loaded.source, CredentialSource::Missing);
        assert!(loaded.warning.contains("私有凭据文件"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_saves_private_file_with_private_permissions() {
        let dir = test_dir("save");
        let source = save_volc_api_key(CredentialMode::Development, &dir, "dev-secret").unwrap();
        assert_eq!(source, CredentialSource::DevelopmentFile);
        let path = development_credential_path(&dir);
        assert_eq!(
            load_development_file(&path).unwrap().as_deref(),
            Some("dev-secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_removes_private_file_only_on_explicit_empty_save() {
        let dir = test_dir("remove");
        save_volc_api_key(CredentialMode::Development, &dir, "dev-secret").unwrap();

        let source = save_volc_api_key(CredentialMode::Development, &dir, "").unwrap();

        assert_eq!(source, CredentialSource::Missing);
        assert!(!development_credential_path(&dir).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_explicit_removal_does_not_remigrate_old_keychain_entry() {
        let dir = test_dir("remove-no-remigrate");
        let path = development_credential_path(&dir);
        save_development_file(&path, "dev-secret").unwrap();
        save_development_file(&path, "").unwrap();
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(DEVELOPMENT_SERVICE.into(), "legacy-dev-secret".into());

        let loaded = load_development(None, &path, &store);

        assert!(loaded.value.is_empty());
        assert_eq!(loaded.source, CredentialSource::Missing);
        assert!(store.calls.borrow().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn development_migrates_accessible_old_keychain_entry_to_file() {
        let dir = test_dir("legacy-migrate");
        let path = development_credential_path(&dir);
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(DEVELOPMENT_SERVICE.into(), "legacy-dev-secret".into());
        let loaded = load_development(None, &path, &store);
        assert_eq!(loaded.value, "legacy-dev-secret");
        assert_eq!(loaded.source, CredentialSource::DevelopmentFile);
        assert_eq!(
            load_development_file(&path).unwrap().as_deref(),
            Some("legacy-dev-secret")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn production_store_failure_becomes_warning() {
        let store = FakeStore::default();
        *store.load_error.borrow_mut() = Some("user canceled".into());
        let loaded = load_production(&store);
        assert!(loaded.value.is_empty());
        assert_eq!(loaded.source, CredentialSource::Missing);
        assert!(loaded.warning.contains("继续启动"));
    }

    #[test]
    fn production_reads_only_production_store() {
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(PRODUCTION_SERVICE.into(), "production-secret".into());
        let loaded = load_production(&store);
        assert_eq!(loaded.value, "production-secret");
        assert_eq!(loaded.source, CredentialSource::SystemStore);
        assert_eq!(
            store.calls.borrow().as_slice(),
            [format!("load:{PRODUCTION_SERVICE}")]
        );
    }

    #[test]
    fn production_migrates_accessible_legacy_value() {
        let store = FakeStore::default();
        store
            .values
            .borrow_mut()
            .insert(LEGACY_SHARED_SERVICE.into(), "legacy-secret".into());
        let loaded = load_production(&store);
        assert_eq!(loaded.value, "legacy-secret");
        assert_eq!(loaded.source, CredentialSource::LegacyMigration);
        assert_eq!(
            store
                .values
                .borrow()
                .get(PRODUCTION_SERVICE)
                .map(String::as_str),
            Some("legacy-secret")
        );
    }
}
