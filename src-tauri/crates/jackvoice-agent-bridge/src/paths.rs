use std::path::PathBuf;

pub const SHARED_DIRECTORY_NAME: &str = "com.jackvoice.shared";
pub const SHARED_DATA_DIR_ENV: &str = "JACKVOICE_SHARED_DATA_DIR";

/// 正式版和开发版共用的业务数据目录（热词、替换词、历史、录音）。
pub fn shared_data_dir() -> Result<PathBuf, String> {
    if let Some(override_dir) = std::env::var_os(SHARED_DATA_DIR_ENV) {
        let path = PathBuf::from(override_dir);
        if path.as_os_str().is_empty() {
            return Err("JACKVOICE_SHARED_DATA_DIR 不能为空。".into());
        }
        return Ok(path);
    }
    Ok(platform_data_root()?.join(SHARED_DIRECTORY_NAME))
}

fn platform_data_root() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(home_dir()?.join("Library/Application Support"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| "无法确定 APPDATA 目录。".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg));
        }
        Ok(home_dir()?.join(".local/share"))
    }
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "无法确定用户主目录。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shared_dir_ends_with_shared_directory_name() {
        if std::env::var_os(SHARED_DATA_DIR_ENV).is_some() {
            return;
        }
        let path = shared_data_dir().unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(SHARED_DIRECTORY_NAME)
        );
    }
}
