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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionStatus {
    pub microphone: bool,
    pub accessibility: bool,
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use std::os::raw::c_int;

    type CFDictionaryRef = *const std::ffi::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> c_int;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> c_int;
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

    /// Read the TCC microphone authorization status for this app via
    /// `AVCaptureDevice authorizationStatusForMediaType:`.
    /// Returns `true` only when the status is Authorized.
    pub fn microphone_permission() -> bool {
        use objc2::exception::catch;
        use objc2::msg_send;
        use objc2::runtime::AnyClass;
        use objc2_foundation::ns_string;

        // Resolve AVFoundation lazily through the Objective-C runtime; the
        // class is guaranteed present on any macOS that runs a Tauri app.
        let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
            return true;
        };

        // AVMediaTypeAudio's constant value is the UTI "public.avf-audio".
        let media_type = ns_string!("public.avf-audio");
        // On recent macOS (e.g. 26.x), `authorizationStatusForMediaType:` can
        // throw an Objective-C exception while the app's TCC status is still
        // "not determined" (it internally kicks off the request flow). Rust
        // has no ObjC exception handler, so an uncaught exception would cross
        // the FFI boundary and abort the whole process. Catch it and treat
        // the status as "not granted" instead of crashing.
        let status = catch(std::panic::AssertUnwindSafe(|| unsafe {
            let status: i64 = msg_send![cls, authorizationStatusForMediaType: media_type];
            status
        }));
        match status {
            Ok(status) => status == 3, // AVAuthorizationStatusAuthorized
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "macos")]
pub fn current_status() -> PermissionStatus {
    PermissionStatus {
        microphone: microphone_permission(),
        accessibility: accessibility_is_trusted(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn current_status() -> PermissionStatus {
    PermissionStatus {
        microphone: true,
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
pub fn microphone_permission() -> bool {
    true
}
