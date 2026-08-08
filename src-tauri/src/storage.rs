use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

pub const PRODUCTION_IDENTIFIER: &str = "com.jackvoice.app";
const SHARED_DIRECTORY_NAME: &str = "com.jackvoice.shared";
const MIGRATION_LOCK_FILE: &str = ".com.jackvoice.storage-migration.lock";
const INSTANCE_LOCK_FILE: &str = ".jackvoice-instance.lock";
const OVERLAY_POSITION_FILE: &str = "overlay-position.json";
pub const INSTANCE_CONFLICT_MESSAGE: &str =
    "另一个 JackVoice 正式版或开发版正在运行，请先退出后再启动。";

#[derive(Debug)]
pub struct AppDirectories {
    pub shared: PathBuf,
    pub variant: PathBuf,
    pub production_variant: PathBuf,
}

/// Kept in Tauri managed state for the complete process lifetime.
pub struct SharedInstanceGuard {
    _file: File,
}

pub fn prepare_directories(app: &AppHandle) -> Result<AppDirectories, String> {
    let data_root = app
        .path()
        .data_dir()
        .map_err(|error| format!("获取 JackVoice 数据根目录失败：{error}"))?;
    fs::create_dir_all(&data_root)
        .map_err(|error| format!("创建 JackVoice 数据根目录失败：{error}"))?;

    let production_variant = data_root.join(PRODUCTION_IDENTIFIER);
    let shared = data_root.join(SHARED_DIRECTORY_NAME);
    with_migration_lock(&data_root, || {
        migrate_legacy_directory(&production_variant, &shared)?;
        fs::create_dir_all(&shared)
            .map_err(|error| format!("创建 JackVoice 共享数据目录失败：{error}"))?;
        fs::create_dir_all(&production_variant)
            .map_err(|error| format!("创建 JackVoice 正式版数据目录失败：{error}"))?;
        move_production_overlay_out_of_shared(&shared, &production_variant)?;
        set_private_tree_permissions(&shared)?;
        set_private_tree_permissions(&production_variant)
    })?;

    let variant = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("获取 JackVoice 客户端数据目录失败：{error}"))?;
    fs::create_dir_all(&variant)
        .map_err(|error| format!("创建 JackVoice 客户端数据目录失败：{error}"))?;
    set_private_tree_permissions(&variant)?;

    Ok(AppDirectories {
        shared,
        variant,
        production_variant,
    })
}

pub fn acquire_shared_instance_lock(shared_dir: &Path) -> Result<SharedInstanceGuard, String> {
    fs::create_dir_all(shared_dir)
        .map_err(|error| format!("创建 JackVoice 共享数据目录失败：{error}"))?;
    let path = shared_dir.join(INSTANCE_LOCK_FILE);
    let file = open_lock_file(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(SharedInstanceGuard { _file: file }),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(INSTANCE_CONFLICT_MESSAGE.to_string())
        }
        Err(error) => Err(format!("锁定 JackVoice 运行实例失败：{error}")),
    }
}

pub fn write_atomic(path: &Path, contents: &[u8], keep_backup: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "JackVoice 数据路径无效。".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建 JackVoice 数据目录失败：{error}"))?;
    set_private_directory_permissions(parent)?;

    if keep_backup && path.is_file() {
        let backup_path = path.with_extension("json.bak");
        fs::copy(path, &backup_path)
            .map_err(|error| format!("备份 JackVoice 数据失败：{error}"))?;
        set_private_file_permissions(&backup_path)?;
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("data");
    let temporary_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temporary_path, contents)
        .map_err(|error| format!("写入 JackVoice 临时数据失败：{error}"))?;
    set_private_file_permissions(&temporary_path)?;

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("替换 JackVoice 数据失败：{error}"))?;
    }

    fs::rename(&temporary_path, path)
        .map_err(|error| format!("保存 JackVoice 数据失败：{error}"))?;
    set_private_file_permissions(path)
}

fn with_migration_lock<T>(
    data_root: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let lock_path = data_root.join(MIGRATION_LOCK_FILE);
    let file = open_lock_file(&lock_path)?;
    file.lock_exclusive()
        .map_err(|error| format!("锁定 JackVoice 数据迁移失败：{error}"))?;
    let result = action();
    FileExt::unlock(&file).map_err(|error| format!("释放 JackVoice 数据迁移锁失败：{error}"))?;
    result
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("打开 JackVoice 锁文件失败：{error}"))?;
    set_private_file_permissions(path)?;
    Ok(file)
}

fn migrate_legacy_directory(legacy: &Path, shared: &Path) -> Result<(), String> {
    if shared.exists() || !legacy.is_dir() {
        return Ok(());
    }

    if fs::rename(legacy, shared).is_ok() {
        return Ok(());
    }

    let parent = shared
        .parent()
        .ok_or_else(|| "JackVoice 共享数据路径无效。".to_string())?;
    let temporary = parent.join(format!(
        ".{SHARED_DIRECTORY_NAME}.migration-{}",
        std::process::id()
    ));
    if temporary.exists() {
        fs::remove_dir_all(&temporary)
            .map_err(|error| format!("清理 JackVoice 迁移暂存目录失败：{error}"))?;
    }
    copy_directory(legacy, &temporary)?;
    fs::rename(&temporary, shared)
        .map_err(|error| format!("完成 JackVoice 共享数据迁移失败：{error}"))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("创建 JackVoice 迁移目录失败：{error}"))?;
    for entry in
        fs::read_dir(source).map_err(|error| format!("读取 JackVoice 原数据目录失败：{error}"))?
    {
        let entry = entry.map_err(|error| format!("读取 JackVoice 原数据失败：{error}"))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 JackVoice 原数据类型失败：{error}"))?;
        if file_type.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)
                .map_err(|error| format!("复制 JackVoice 原数据失败：{error}"))?;
        }
    }
    Ok(())
}

fn move_production_overlay_out_of_shared(shared: &Path, production: &Path) -> Result<(), String> {
    let source = shared.join(OVERLAY_POSITION_FILE);
    let destination = production.join(OVERLAY_POSITION_FILE);
    if !source.is_file() || destination.exists() {
        return Ok(());
    }
    if fs::rename(&source, &destination).is_ok() {
        return Ok(());
    }
    fs::copy(&source, &destination)
        .map_err(|error| format!("迁移 JackVoice 悬浮条位置失败：{error}"))?;
    fs::remove_file(source).map_err(|error| format!("清理 JackVoice 旧悬浮条位置失败：{error}"))
}

pub fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("设置 JackVoice 数据文件权限失败：{error}"))?;
    }
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("设置 JackVoice 数据目录权限失败：{error}"))?;
    }
    Ok(())
}

fn set_private_tree_permissions(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        return set_private_file_permissions(path);
    }
    for entry in
        fs::read_dir(path).map_err(|error| format!("读取 JackVoice 数据权限目录失败：{error}"))?
    {
        let entry = entry.map_err(|error| format!("读取 JackVoice 数据权限项失败：{error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 JackVoice 数据权限类型失败：{error}"))?;
        if file_type.is_dir() {
            set_private_tree_permissions(&entry.path())?;
        } else if file_type.is_file() {
            set_private_file_permissions(&entry.path())?;
        }
    }
    set_private_directory_permissions(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("jackvoice-storage-{}-{unique}", std::process::id()))
    }

    #[test]
    fn migrates_legacy_directory_to_neutral_shared_directory() {
        let root = test_dir();
        let legacy = root.join(PRODUCTION_IDENTIFIER);
        let shared = root.join(SHARED_DIRECTORY_NAME);
        fs::create_dir_all(legacy.join("audio")).unwrap();
        fs::write(legacy.join("settings.json"), b"settings").unwrap();
        fs::write(legacy.join("audio/recording.wav"), b"audio").unwrap();

        migrate_legacy_directory(&legacy, &shared).unwrap();

        assert!(!legacy.exists());
        assert_eq!(fs::read(shared.join("settings.json")).unwrap(), b"settings");
        assert_eq!(
            fs::read(shared.join("audio/recording.wav")).unwrap(),
            b"audio"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_write_keeps_previous_json_backup() {
        let root = test_dir();
        let path = root.join("settings.json");
        write_atomic(&path, b"first", true).unwrap();
        write_atomic(&path, b"second", true).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read(path.with_extension("json.bak")).unwrap(), b"first");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_instance_lock_blocks_the_other_client_variant() {
        let root = test_dir();
        let first = acquire_shared_instance_lock(&root).unwrap();
        let second = acquire_shared_instance_lock(&root);
        assert!(second.is_err());

        drop(first);
        assert!(acquire_shared_instance_lock(&root).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
