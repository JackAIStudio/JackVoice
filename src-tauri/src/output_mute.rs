//! Temporarily mute the system output while dictation owns the microphone.
//!
//! Muting is deliberately best-effort: unsupported output devices must never
//! prevent dictation. A small recovery record lets the next launch repair a
//! mute left behind by a crash, while the in-memory guard covers every normal
//! stop/error/exit path.

#[cfg(target_os = "macos")]
mod platform {
    use crate::storage;
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2_core_audio::{
        kAudioDevicePropertyDeviceUID, kAudioDevicePropertyMute,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectSetPropertyData,
    };
    use serde::{Deserialize, Serialize};
    use std::ffi::c_void;
    use std::fs;
    use std::mem::{size_of, MaybeUninit};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, NonNull};
    use std::time::{SystemTime, UNIX_EPOCH};

    const RECOVERY_FILE: &str = "output-mute-recovery.json";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RecoveryState {
        device_id: AudioObjectID,
        device_uid: String,
        property_scope: u32,
        original_muted: bool,
        process_id: u32,
        started_at_ms: u64,
    }

    pub struct OutputMuteGuard {
        state: Option<RecoveryState>,
        recovery_path: PathBuf,
    }

    impl OutputMuteGuard {
        pub fn engage(data_dir: &Path) -> Result<Self, String> {
            let recovery_path = data_dir.join(RECOVERY_FILE);
            let device_id = default_output_device()?;
            let device_uid = device_uid(device_id)?;
            let mut last_error = None;

            for scope in [
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyScopeGlobal,
            ] {
                let address = mute_address(scope);
                let original_muted = match get_property::<u32>(device_id, &address) {
                    Ok(value) => value != 0,
                    Err(error) => {
                        last_error = Some(error);
                        continue;
                    }
                };

                // The user already owns this muted state; do not create a
                // recovery record and never unmute it later.
                if original_muted {
                    return Ok(Self {
                        state: None,
                        recovery_path,
                    });
                }

                let state = RecoveryState {
                    device_id,
                    device_uid: device_uid.clone(),
                    property_scope: scope,
                    original_muted,
                    process_id: std::process::id(),
                    started_at_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                };

                // Write first: a crash between this write and the actual mute
                // is harmless because recovery sees that output is unmuted.
                persist_recovery(&recovery_path, &state)?;
                if let Err(error) = set_property(device_id, &address, 1_u32) {
                    let _ = clear_recovery(&recovery_path);
                    last_error = Some(error);
                    continue;
                }

                return Ok(Self {
                    state: Some(state),
                    recovery_path,
                });
            }

            Err(last_error.unwrap_or_else(|| "当前输出设备不支持系统静音。".into()))
        }

        /// Restore only a mute state that JackVoice actually applied. If the
        /// user has already unmuted during dictation, their newer choice wins.
        pub fn restore(&mut self) -> Result<(), String> {
            let Some(state) = self.state.as_ref() else {
                return Ok(());
            };
            restore_state(&self.recovery_path, state)?;
            self.state = None;
            Ok(())
        }
    }

    impl Drop for OutputMuteGuard {
        fn drop(&mut self) {
            if let Err(error) = self.restore() {
                eprintln!("[output-mute] 恢复系统音频失败：{error}");
            }
        }
    }

    pub const fn supported() -> bool {
        true
    }

    /// Repair a mute record left by a terminated process. The device UID is
    /// checked before writing so a recycled CoreAudio object ID can never
    /// affect an unrelated output device.
    pub fn restore_stale(data_dir: &Path) -> Result<bool, String> {
        let path = data_dir.join(RECOVERY_FILE);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("读取系统音频恢复记录失败：{error}")),
        };
        let state: RecoveryState = serde_json::from_slice(&raw)
            .map_err(|error| format!("解析系统音频恢复记录失败：{error}"))?;
        restore_state(&path, &state)?;
        Ok(true)
    }

    fn restore_state(path: &Path, state: &RecoveryState) -> Result<(), String> {
        let device_id = resolve_device(state)?;
        let mut scopes = vec![
            state.property_scope,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyScopeGlobal,
        ];
        scopes.dedup();
        let mut last_error = None;

        for scope in scopes {
            let address = mute_address(scope);
            let currently_muted = match get_property::<u32>(device_id, &address) {
                Ok(value) => value != 0,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };

            if currently_muted != state.original_muted {
                if let Err(error) = set_property(
                    device_id,
                    &address,
                    if state.original_muted { 1_u32 } else { 0_u32 },
                ) {
                    last_error = Some(error);
                    continue;
                }
            }

            clear_recovery(path)?;
            return Ok(());
        }

        Err(last_error.unwrap_or_else(|| "当前输出设备无法恢复静音状态。".into()))
    }

    fn resolve_device(state: &RecoveryState) -> Result<AudioObjectID, String> {
        if device_uid(state.device_id).as_deref() == Ok(state.device_uid.as_str()) {
            return Ok(state.device_id);
        }
        let current = default_output_device()?;
        if device_uid(current).as_deref() == Ok(state.device_uid.as_str()) {
            return Ok(current);
        }
        Err("上次使用的输出设备当前不可用，已保留恢复记录。".into())
    }

    fn default_output_device() -> Result<AudioObjectID, String> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let device_id = get_property::<AudioObjectID>(kAudioObjectSystemObject as u32, &address)?;
        if device_id == 0 {
            Err("没有可用的系统输出设备。".into())
        } else {
            Ok(device_id)
        }
    }

    fn device_uid(device_id: AudioObjectID) -> Result<String, String> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceUID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let raw = get_property::<CFStringRef>(device_id, &address)?;
        if raw.is_null() {
            return Err("系统输出设备没有稳定标识。".into());
        }
        let value = unsafe { CFString::wrap_under_get_rule(raw) };
        Ok(value.to_string())
    }

    fn mute_address(scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn get_property<T: Copy>(
        object_id: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<T, String> {
        let mut value = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let output = NonNull::new(value.as_mut_ptr().cast::<c_void>())
            .ok_or_else(|| "CoreAudio 返回了空的数据地址。".to_string())?;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(address),
                0,
                null(),
                NonNull::from(&mut size),
                output,
            )
        };
        if status != 0 {
            return Err(format!("读取系统输出属性失败（CoreAudio {status}）。"));
        }
        if size != size_of::<T>() as u32 {
            return Err("系统输出属性的数据大小不符合预期。".into());
        }
        Ok(unsafe { value.assume_init() })
    }

    fn set_property<T>(
        object_id: AudioObjectID,
        address: &AudioObjectPropertyAddress,
        mut value: T,
    ) -> Result<(), String> {
        let input = NonNull::from(&mut value).cast::<c_void>();
        let status = unsafe {
            AudioObjectSetPropertyData(
                object_id,
                NonNull::from(address),
                0,
                null(),
                size_of::<T>() as u32,
                input,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("设置系统输出静音失败（CoreAudio {status}）。"))
        }
    }

    fn persist_recovery(path: &Path, state: &RecoveryState) -> Result<(), String> {
        let raw = serde_json::to_vec_pretty(state)
            .map_err(|error| format!("序列化系统音频恢复记录失败：{error}"))?;
        storage::write_atomic(path, &raw, false)
    }

    fn clear_recovery(path: &Path) -> Result<(), String> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("清理系统音频恢复记录失败：{error}")),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::path::Path;

    pub struct OutputMuteGuard;

    impl OutputMuteGuard {
        pub fn engage(_data_dir: &Path) -> Result<Self, String> {
            Err("当前系统暂不支持听写时自动静音。".into())
        }

        pub fn restore(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    pub const fn supported() -> bool {
        false
    }

    pub fn restore_stale(_data_dir: &Path) -> Result<bool, String> {
        Ok(false)
    }
}

pub use platform::{restore_stale, supported, OutputMuteGuard};
