//! First-run onboarding support: macOS permission detection and prompting.
//!
//! JackVoice needs two system permissions to work as intended:
//! - **Microphone** (audio capture for dictation) — granted through the
//!   system TCC prompt, which fires the first time an audio input stream is
//!   opened. Detection here reads `AVCaptureDevice` authorization status.
//! - **Accessibility** (auto-paste the transcript into the focused text
//!   field) — granted in System Settings. We detect with `AXIsProcessTrusted`
//!   and prompt with `AXIsProcessTrustedWithOptions`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MicrophoneAuthorization {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

impl MicrophoneAuthorization {
    pub fn is_authorized(self) -> bool {
        self == Self::Authorized
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionStatus {
    pub microphone: bool,
    pub microphone_authorization: MicrophoneAuthorization,
    pub accessibility: bool,
}

impl PermissionStatus {
    pub fn required_permissions_granted(self) -> bool {
        self.microphone && self.accessibility
    }

    pub fn missing_permissions_message(self) -> String {
        match (self.microphone, self.accessibility) {
            (false, false) => "请先开启麦克风和辅助功能权限，才能使用 JackVoice。".into(),
            (false, true) => microphone_permission_error(self.microphone_authorization),
            (true, false) => "请先开启辅助功能权限，才能使用 JackVoice。".into(),
            (true, true) => String::new(),
        }
    }
}

pub fn microphone_permission_error(status: MicrophoneAuthorization) -> String {
    match status {
        MicrophoneAuthorization::Denied => {
            "麦克风权限已被拒绝。请到系统设置的「隐私与安全性 → 麦克风」中开启 JackVoice。".into()
        }
        MicrophoneAuthorization::Restricted => {
            "系统限制了麦克风访问，请检查屏幕使用时间、设备管理或系统隐私设置。".into()
        }
        MicrophoneAuthorization::NotDetermined => {
            "麦克风尚未授权，请在系统授权窗口中选择「允许」后重试。".into()
        }
        MicrophoneAuthorization::Authorized => String::new(),
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use objc2_foundation::NSString;
    use std::os::raw::c_int;

    type CFDictionaryRef = *const std::ffi::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> c_int;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> c_int;
    }

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "AVMediaTypeAudio"]
        static AV_MEDIA_TYPE_AUDIO: *const NSString;
    }

    pub fn accessibility_is_trusted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    /// Show the system "wants to control this computer" prompt and return
    /// whether the process is trusted afterwards (it usually is not yet,
    /// because the user has to flip the switch in System Settings first).
    pub fn request_accessibility_prompt() -> bool {
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
        unsafe {
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as CFDictionaryRef) != 0
        }
    }

    /// Read the complete TCC microphone authorization status for this app via
    /// `AVCaptureDevice authorizationStatusForMediaType:`. Keeping denied and
    /// not-determined separate lets the onboarding UI explain the right next
    /// action instead of treating a successfully-created audio stream as proof
    /// that permission was granted.
    pub fn microphone_authorization() -> super::MicrophoneAuthorization {
        use super::MicrophoneAuthorization;
        use objc2::exception::catch;
        use objc2::msg_send;
        use objc2::runtime::AnyClass;

        // Resolve AVFoundation lazily through the Objective-C runtime; the
        // class is guaranteed present on any macOS that runs a Tauri app.
        let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
            return MicrophoneAuthorization::Restricted;
        };

        // Pass AVFoundation's exported AVMediaTypeAudio constant verbatim.
        // A UTI-like replacement such as "public.avf-audio" is not accepted:
        // Apple's API throws NSInvalidArgumentException for any value other
        // than AVMediaTypeAudio or AVMediaTypeVideo.
        let Some(media_type) = (unsafe { AV_MEDIA_TYPE_AUDIO.as_ref() }) else {
            return MicrophoneAuthorization::Restricted;
        };
        // An Objective-C exception must never cross the Rust FFI boundary.
        let status = catch(std::panic::AssertUnwindSafe(|| unsafe {
            let status: i64 = msg_send![cls, authorizationStatusForMediaType: media_type];
            status
        }));
        match status {
            Ok(0) => MicrophoneAuthorization::NotDetermined,
            Ok(1) => MicrophoneAuthorization::Restricted,
            Ok(2) => MicrophoneAuthorization::Denied,
            Ok(3) => MicrophoneAuthorization::Authorized,
            Ok(_) | Err(_) => MicrophoneAuthorization::Restricted,
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "macos")]
pub fn current_status() -> PermissionStatus {
    let microphone_authorization = microphone_authorization();
    PermissionStatus {
        microphone: microphone_authorization.is_authorized(),
        microphone_authorization,
        accessibility: accessibility_is_trusted(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn current_status() -> PermissionStatus {
    PermissionStatus {
        microphone: true,
        microphone_authorization: MicrophoneAuthorization::Authorized,
        accessibility: true,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn accessibility_is_trusted() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
pub fn request_accessibility_prompt() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
pub fn microphone_authorization() -> MicrophoneAuthorization {
    MicrophoneAuthorization::Authorized
}

pub fn validate_completion(
    microphone_granted: bool,
    accessibility_granted: bool,
    privacy_confirmed: bool,
) -> Result<(), String> {
    if !microphone_granted {
        return Err("请先授权并成功测试麦克风。麦克风是本地录音的必要权限。".into());
    }
    if !accessibility_granted {
        return Err("请先开启辅助功能权限。辅助功能是全局快捷键和自动插入的必要权限。".into());
    }
    if !privacy_confirmed {
        return Err("请先确认你已了解音频的云端处理和本地保存方式。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_completion;

    #[test]
    fn completion_requires_both_permissions_and_privacy_confirmation() {
        assert!(validate_completion(false, false, false).is_err());
        assert!(validate_completion(false, true, true).is_err());
        assert!(validate_completion(true, false, true).is_err());
        assert!(validate_completion(true, true, false).is_err());
        assert!(validate_completion(true, true, true).is_ok());
    }
}
