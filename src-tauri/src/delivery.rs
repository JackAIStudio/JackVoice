use tauri::{AppHandle, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryResult {
    pub pasted: bool,
    pub copied: bool,
    pub message: String,
}

/// Whether the dictation target currently has a focused, text-insertable element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionProbe {
    /// A focused text field / text area was found: safe to auto-paste.
    Insertable,
    /// The app is reachable but nothing text-editable is focused: skip paste.
    NotInsertable,
    /// Could not determine (probe error, timeout, unsupported platform):
    /// fall back to the legacy best-effort paste.
    Unknown,
}

/// Roles that represent a real text insertion point (native controls and
/// Chromium-exposed web text fields).
const TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox"];

pub fn deliver_text<R: Runtime>(
    app: &AppHandle<R>,
    text: &str,
    probe: InsertionProbe,
) -> DeliveryResult {
    let text = text.trim();
    if text.is_empty() {
        return DeliveryResult {
            pasted: false,
            copied: false,
            message: "没有可插入的文本。".into(),
        };
    }

    // Always copy first so users never lose the result.
    let copied = app.clipboard().write_text(text.to_string()).is_ok();

    let pasted = if probe == InsertionProbe::NotInsertable {
        // No focused caret in the target app: skip the paste entirely so text
        // does not land in some unintended input box. The clipboard copy above
        // plus the manual "copy" capsule cover the user.
        false
    } else {
        // Paste must run on the main thread on macOS.
        // Calling CGEvent / TSM APIs from a Tokio worker thread previously crashed
        // with HIToolbox dispatch_assert_queue.
        let app_handle = app.clone();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let _ = app.run_on_main_thread(move || {
            let ok = simulate_paste_main_thread();
            let _ = tx.send(ok);
            let _ = app_handle;
        });

        rx.recv_timeout(std::time::Duration::from_millis(800))
            .unwrap_or(false)
    };

    let message = if pasted && copied {
        "已插入到当前输入位置。".into()
    } else if copied {
        if probe == InsertionProbe::NotInsertable {
            "未检测到输入焦点，未自动插入，已复制到剪贴板。可点悬浮胶囊上的“复制”按钮重新复制。"
                .into()
        } else {
            "未检测到插入，已复制到剪贴板。可点悬浮胶囊上的“复制”按钮手动复制。".into()
        }
    } else {
        "未检测到插入，自动复制也失败。请点悬浮胶囊上的“复制”按钮手动复制。".into()
    };

    DeliveryResult {
        pasted,
        copied,
        message,
    }
}

/// Probe the target app's focused UI element through the macOS Accessibility
/// API (via osascript) and decide whether there is a real text insertion point.
///
/// `process_name` should be the app that will receive the paste (the app that
/// was frontmost when the capsule showed). When unavailable, probe the
/// current frontmost app instead.
pub fn probe_insertion_target(process_name: Option<&str>) -> InsertionProbe {
    #[cfg(target_os = "macos")]
    {
        let selector = match process_name {
            Some(name) if !name.trim().is_empty() => {
                let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    "set p to first application process whose name is \"{}\"",
                    escaped
                )
            }
            _ => "set p to first application process whose frontmost is true".to_string(),
        };
        // Also report whether the focused element exposes a live insertion
        // point. Chrome exposes a focused <textarea> as AXGroup, and some
        // Electron/web surfaces only expose AXWebArea — role alone is not
        // enough to tell a blinking caret from page-level focus.
        let script = format!(
            concat!(
                "tell application \"System Events\"\n",
                "\t{selector}\n",
                "\ttry\n",
                "\t\tset el to value of attribute \"AXFocusedUIElement\" of p\n",
                "\t\tif el is missing value then return \"__missing__\"\n",
                "\t\tset r to value of attribute \"AXRole\" of el as string\n",
                "\t\tset sel to \"absent\"\n",
                "\t\ttry\n",
                "\t\t\tset tmp to value of attribute \"AXSelectedTextRange\" of el\n",
                "\t\t\tset sel to \"present\"\n",
                "\t\tend try\n",
                "\t\tset ins to \"missing\"\n",
                "\t\ttry\n",
                "\t\t\tset ins to (value of attribute \"AXInsertionPointLineNumber\" of el) as string\n",
                "\t\tend try\n",
                "\t\treturn r & \"|\" & sel & \"|\" & ins\n",
                "\ton error\n",
                "\t\treturn \"__error__\"\n",
                "\tend try\n",
                "end tell"
            ),
            selector = selector
        );
        run_probe_script(&script)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = process_name;
        InsertionProbe::Unknown
    }
}

#[cfg(target_os = "macos")]
fn run_probe_script(script: &str) -> InsertionProbe {
    use std::io::Read;

    let mut child = match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return InsertionProbe::Unknown,
    };

    // Bound the wait: AX queries against unresponsive apps can hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut out);
                }
                if !status.success() {
                    return InsertionProbe::Unknown;
                }
                let line = out.trim();
                return match line {
                    "__missing__" => InsertionProbe::NotInsertable,
                    "__error__" | "" => InsertionProbe::Unknown,
                    _ => classify_probe_line(line),
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return InsertionProbe::Unknown;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            Err(_) => return InsertionProbe::Unknown,
        }
    }
}

/// Classify the probe output line `ROLE|SEL|INS`.
///
/// Insertable when the focused element is a known text role, or when it
/// exposes a sane `AXInsertionPointLineNumber` (a live caret). Page-level
/// `AXWebArea` focus reports an out-of-range line number, so it stays
/// NotInsertable and Cmd+V is never fired without a real insertion point.
#[cfg(target_os = "macos")]
fn classify_probe_line(line: &str) -> InsertionProbe {
    let mut parts = line.split('|');
    let role = parts.next().unwrap_or("").trim();
    let sel = parts.next().unwrap_or("").trim();
    let ins = parts.next().unwrap_or("").trim();

    let ins_line = ins.parse::<f64>().ok();
    let has_caret = matches!(ins_line, Some(v) if (0.0..1_000_000_000.0).contains(&v));
    let probe = if TEXT_ROLES.contains(&role) || has_caret {
        InsertionProbe::Insertable
    } else {
        InsertionProbe::NotInsertable
    };

    eprintln!(
        "[delivery] probe role={role} selectedTextRange={sel} insertionPointLine={ins} -> {probe:?}"
    );
    probe
}

/// Best-effort paste. We always copied first, so if the frontmost app has no
/// text focus, the paste simply does nothing useful and the clipboard still holds text.
fn simulate_paste_main_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // osascript runs outside our worker-thread constraints and is more stable
        // than driving HIToolbox key mapping ourselves.
        // It posts Cmd+V to the frontmost app.
        let status = Command::new("osascript")
            .args([
                "-e",
                r#"tell application "System Events" to keystroke "v" using command down"#,
            ])
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return true;
        }

        // Fallback for dev builds: osascript's keystroke can be rejected by
        // TCC with "osascript is not allowed to send keystrokes" even when the
        // hosting terminal app is trusted (e.g. embedded terminals like the
        // ChatGPT desktop app). Posting the key event directly from this
        // process uses the responsible app's trust instead of requiring
        // /usr/bin/osascript to be added to Accessibility separately.
        post_paste_via_cgevent()
    }

    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        // PowerShell SendKeys paste. Requires the target window already focused.
        let status = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v')",
            ])
            .status();
        matches!(status, Ok(s) if s.success())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Post Cmd+V directly through Core Graphics.
#[cfg(target_os = "macos")]
fn post_paste_via_cgevent() -> bool {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, KeyCode};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(source) => source,
        Err(()) => return false,
    };
    let key_down = match CGEvent::new_keyboard_event(source.clone(), KeyCode::ANSI_V, true) {
        Ok(event) => event,
        Err(()) => return false,
    };
    let key_up = match CGEvent::new_keyboard_event(source, KeyCode::ANSI_V, false) {
        Ok(event) => event,
        Err(()) => return false,
    };

    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    key_up.set_flags(CGEventFlags::CGEventFlagCommand);
    key_down.post(CGEventTapLocation::HID);
    key_up.post(CGEventTapLocation::HID);
    true
}
