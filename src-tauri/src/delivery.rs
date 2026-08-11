use tauri::{AppHandle, Runtime};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use tauri_plugin_clipboard_manager::ClipboardExt;

const CLIPBOARD_RESTORE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

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

/// Keep the app that owned the caret when dictation started unless the app
/// currently in front is confidently text-insertable. Transient utilities
/// such as screenshot tools may become frontmost during recording, but they
/// must not replace the original dictation destination.
pub(crate) fn choose_delivery_target(
    initial_target: Option<String>,
    current_target: Option<String>,
    current_probe: InsertionProbe,
) -> Option<String> {
    if current_probe == InsertionProbe::Insertable {
        current_target.or(initial_target)
    } else {
        initial_target.or(current_target)
    }
}

/// Roles that represent a real text insertion point (native controls and
/// Chromium-exposed web text fields).
const TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox"];

pub async fn deliver_text<R: Runtime>(
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

    if probe == InsertionProbe::NotInsertable {
        return DeliveryResult {
            pasted: false,
            copied: false,
            message: "未检测到输入焦点，未自动插入；原剪贴板保持不变。可重新选择输入位置后重试。"
                .into(),
        };
    }

    // Keep the insertion mechanism identical to the original working path:
    // write the transcript to the pasteboard and post Cmd+V. Clipboard
    // preservation is an outer transaction only; it must never replace the
    // proven paste path with an AXSelectedText write. Multi-line AXTextArea
    // controls can report that such writes succeeded while discarding them.
    let app_handle = app.clone();
    let owned_text = text.to_string();
    let prepared = run_on_main_thread(app, move || {
        let transaction = ClipboardTransaction::begin(&app_handle, &owned_text)?;
        let paste_triggered = simulate_paste_main_thread();
        Ok::<_, String>((transaction, paste_triggered))
    })
    .await;

    let (transaction, paste_triggered) = match prepared {
        Ok(Ok(value)) => value,
        Ok(Err(error)) | Err(error) => {
            eprintln!("[delivery] 无法建立透明剪贴板事务：{error}");
            return DeliveryResult {
                pasted: false,
                copied: false,
                message: if error.contains("恢复原剪贴板也失败") {
                    "本次未自动粘贴，但原剪贴板恢复失败。请立即检查剪贴板；听写文字仍可从历史记录找回。"
                        .into()
                } else {
                    "为避免覆盖原剪贴板，本次未自动粘贴。可重新选择输入位置后重试，或明确点击“复制”。"
                        .into()
                },
            };
        }
    };

    if paste_triggered {
        tokio::time::sleep(CLIPBOARD_RESTORE_DELAY).await;
    }

    let app_handle = app.clone();
    let restored = run_on_main_thread(app, move || transaction.finish(&app_handle)).await;
    let restore_outcome = match restored {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("[delivery] 恢复剪贴板任务失败：{error}");
            ClipboardRestoreOutcome::Failed
        }
    };

    if !paste_triggered {
        return DeliveryResult {
            pasted: false,
            copied: false,
            message: match restore_outcome {
                ClipboardRestoreOutcome::Restored => {
                    "未能触发自动粘贴，原剪贴板已恢复。可重试或明确点击“复制”。".into()
                }
                ClipboardRestoreOutcome::ExternalChangePreserved => {
                    "未能触发自动粘贴；已保留你刚刚复制的新内容。可重试或明确点击“复制”。".into()
                }
                ClipboardRestoreOutcome::Failed => {
                    "未能触发自动粘贴，且恢复原剪贴板失败。请立即检查剪贴板，并从听写历史找回文字。"
                        .into()
                }
            },
        };
    }

    let message = match restore_outcome {
        ClipboardRestoreOutcome::Restored => "已插入到当前输入位置，原剪贴板已恢复。".into(),
        ClipboardRestoreOutcome::ExternalChangePreserved => {
            "已插入到当前输入位置，并保留了你刚刚复制的新内容。".into()
        }
        ClipboardRestoreOutcome::Failed => {
            "文字已插入，但原剪贴板恢复失败；听写结果仍可从历史记录找回。".into()
        }
    };

    DeliveryResult {
        pasted: true,
        copied: false,
        message,
    }
}

async fn run_on_main_thread<R, T, F>(app: &AppHandle<R>, operation: F) -> Result<T, String>
where
    R: Runtime,
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(operation());
    })
    .map_err(|error| format!("无法切换到主线程：{error}"))?;
    rx.await.map_err(|_| "主线程任务意外结束".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardRestoreOutcome {
    Restored,
    ExternalChangePreserved,
    Failed,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardRepresentation {
    type_name: String,
    data: Vec<u8>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardSnapshot {
    items: Vec<Vec<ClipboardRepresentation>>,
}

#[cfg(target_os = "macos")]
struct ClipboardTransaction {
    snapshot: ClipboardSnapshot,
    temporary_change_count: isize,
}

#[cfg(target_os = "macos")]
impl ClipboardTransaction {
    fn begin<R: Runtime>(_app: &AppHandle<R>, text: &str) -> Result<Self, String> {
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::generalPasteboard();
        let original_change_count = pasteboard.changeCount();
        let snapshot = capture_macos_clipboard(&pasteboard)?;
        if pasteboard.changeCount() != original_change_count {
            return Err("读取期间剪贴板发生了变化，已保留用户的新内容".into());
        }
        if let Err(error) = write_temporary_macos_text(&pasteboard, text, original_change_count) {
            if !error.live_clipboard_changed {
                return Err(error.message);
            }
            return match write_macos_snapshot(&pasteboard, &snapshot) {
                Ok(()) => Err(error.message),
                Err(restore_error) => Err(format!(
                    "{}；恢复原剪贴板也失败：{restore_error}",
                    error.message
                )),
            };
        }

        Ok(Self {
            snapshot,
            temporary_change_count: pasteboard.changeCount(),
        })
    }

    fn finish<R: Runtime>(self, _app: &AppHandle<R>) -> ClipboardRestoreOutcome {
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::generalPasteboard();
        if !clipboard_is_still_temporary(
            self.temporary_change_count,
            pasteboard.changeCount(),
            macos_clipboard_has_temporary_marker(&pasteboard),
        ) {
            // The user (or another app) copied something while JackVoice was
            // delivering. Their newest clipboard always wins.
            return ClipboardRestoreOutcome::ExternalChangePreserved;
        }

        match write_macos_snapshot(&pasteboard, &self.snapshot) {
            Ok(()) => ClipboardRestoreOutcome::Restored,
            Err(error) => {
                eprintln!("[delivery] 恢复 macOS 剪贴板失败：{error}");
                ClipboardRestoreOutcome::Failed
            }
        }
    }
}

fn clipboard_is_still_temporary(
    expected_change_count: isize,
    current_change_count: isize,
    has_temporary_marker: bool,
) -> bool {
    expected_change_count == current_change_count && has_temporary_marker
}

#[cfg(target_os = "macos")]
const MACOS_TRANSIENT_TYPE: &str = "org.nspasteboard.TransientType";
#[cfg(target_os = "macos")]
const MACOS_AUTO_GENERATED_TYPE: &str = "org.nspasteboard.AutoGeneratedType";
#[cfg(target_os = "macos")]
const MACOS_CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MacosTemporaryWriteError {
    message: String,
    live_clipboard_changed: bool,
}

#[cfg(target_os = "macos")]
fn capture_macos_clipboard(
    pasteboard: &objc2_app_kit::NSPasteboard,
) -> Result<ClipboardSnapshot, String> {
    let Some(native_items) = pasteboard.pasteboardItems() else {
        return Ok(ClipboardSnapshot { items: Vec::new() });
    };

    let mut items = Vec::with_capacity(native_items.len());
    for native_item in native_items.iter() {
        let native_types = native_item.types();
        let mut representations = Vec::with_capacity(native_types.len());
        for native_type in native_types.iter() {
            let type_name = native_type.to_string();
            let data = native_item
                .dataForType(&native_type)
                .ok_or_else(|| format!("无法完整读取剪贴板格式 {type_name}，已取消临时覆盖"))?;
            representations.push(ClipboardRepresentation {
                type_name,
                data: data.to_vec(),
            });
        }
        items.push(representations);
    }
    Ok(ClipboardSnapshot { items })
}

#[cfg(target_os = "macos")]
fn write_temporary_macos_text(
    pasteboard: &objc2_app_kit::NSPasteboard,
    text: &str,
    expected_change_count: isize,
) -> Result<(), MacosTemporaryWriteError> {
    use objc2::AnyThread;
    use objc2_app_kit::{NSPasteboardItem, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSData, NSString};

    let item = NSPasteboardItem::init(NSPasteboardItem::alloc());
    let text = NSString::from_str(text);
    if !item.setString_forType(&text, unsafe { NSPasteboardTypeString }) {
        return Err(MacosTemporaryWriteError {
            message: "无法把听写文字写入临时剪贴板".into(),
            live_clipboard_changed: false,
        });
    }

    // Community-standard markers understood by common clipboard managers.
    // They keep this implementation detail out of clipboard history.
    let empty = NSData::with_bytes(&[]);
    for marker in [
        MACOS_TRANSIENT_TYPE,
        MACOS_AUTO_GENERATED_TYPE,
        MACOS_CONCEALED_TYPE,
    ] {
        let marker = NSString::from_str(marker);
        if !item.setData_forType(&empty, &marker) {
            return Err(MacosTemporaryWriteError {
                message: "无法为临时剪贴板写入安全标记".into(),
                live_clipboard_changed: false,
            });
        }
    }

    let objects =
        NSArray::from_retained_slice(&[objc2::runtime::ProtocolObject::from_retained(item)]);
    if pasteboard.changeCount() != expected_change_count {
        return Err(MacosTemporaryWriteError {
            message: "写入前剪贴板发生了变化，已保留用户的新内容".into(),
            live_clipboard_changed: false,
        });
    }
    pasteboard.clearContents();
    if pasteboard.writeObjects(&objects) {
        Ok(())
    } else {
        Err(MacosTemporaryWriteError {
            message: "系统拒绝写入临时剪贴板".into(),
            live_clipboard_changed: true,
        })
    }
}

#[cfg(target_os = "macos")]
fn macos_clipboard_has_temporary_marker(pasteboard: &objc2_app_kit::NSPasteboard) -> bool {
    pasteboard.pasteboardItems().is_some_and(|items| {
        items.iter().next().is_some_and(|item| {
            item.types()
                .iter()
                .any(|native_type| native_type.to_string() == MACOS_TRANSIENT_TYPE)
        })
    })
}

#[cfg(target_os = "macos")]
fn write_macos_snapshot(
    pasteboard: &objc2_app_kit::NSPasteboard,
    snapshot: &ClipboardSnapshot,
) -> Result<(), String> {
    use objc2::AnyThread;
    use objc2_app_kit::NSPasteboardItem;
    use objc2_foundation::{NSArray, NSData, NSString};

    if snapshot.items.is_empty() {
        pasteboard.clearContents();
        return Ok(());
    }

    let mut native_items = Vec::with_capacity(snapshot.items.len());
    for representations in &snapshot.items {
        let item = NSPasteboardItem::init(NSPasteboardItem::alloc());
        for representation in representations {
            let native_type = NSString::from_str(&representation.type_name);
            let data = NSData::with_bytes(&representation.data);
            if !item.setData_forType(&data, &native_type) {
                return Err(format!("恢复剪贴板格式 {} 失败", representation.type_name));
            }
        }
        native_items.push(objc2::runtime::ProtocolObject::from_retained(item));
    }

    let objects = NSArray::from_retained_slice(&native_items);
    // Only replace the live clipboard after every original representation has
    // been rebuilt successfully in memory.
    pasteboard.clearContents();
    if pasteboard.writeObjects(&objects) {
        Ok(())
    } else {
        Err("系统拒绝恢复原剪贴板内容".into())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct ClipboardTransaction {
    previous_text: Option<String>,
    temporary_text: String,
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl ClipboardTransaction {
    fn begin<R: Runtime>(app: &AppHandle<R>, text: &str) -> Result<Self, String> {
        let previous_text = app.clipboard().read_text().ok();
        app.clipboard()
            .write_text(text.to_string())
            .map_err(|error| format!("无法写入临时剪贴板：{error}"))?;
        Ok(Self {
            previous_text,
            temporary_text: text.to_string(),
        })
    }

    fn finish<R: Runtime>(self, app: &AppHandle<R>) -> ClipboardRestoreOutcome {
        if app.clipboard().read_text().ok().as_deref() != Some(self.temporary_text.as_str()) {
            return ClipboardRestoreOutcome::ExternalChangePreserved;
        }
        let restored = match self.previous_text {
            Some(text) => app.clipboard().write_text(text),
            None => app.clipboard().clear(),
        };
        if restored.is_ok() {
            ClipboardRestoreOutcome::Restored
        } else {
            ClipboardRestoreOutcome::Failed
        }
    }
}

#[cfg(target_os = "windows")]
struct ClipboardTransaction {
    formats: Vec<(u32, Vec<u8>)>,
    temporary_sequence: u32,
}

#[cfg(target_os = "windows")]
impl ClipboardTransaction {
    fn begin<R: Runtime>(_app: &AppHandle<R>, text: &str) -> Result<Self, String> {
        use clipboard_win::{raw, Clipboard, EnumFormats};

        let _clipboard =
            Clipboard::new_attempts(20).map_err(|error| format!("无法读取当前剪贴板：{error}"))?;
        let mut formats = Vec::new();
        for format in EnumFormats::new() {
            let mut data = Vec::new();
            raw::get_vec(format, &mut data).map_err(|error| {
                format!("无法完整保存剪贴板格式 {format}：{error}，已取消临时覆盖")
            })?;
            formats.push((format, data));
        }

        let marker = raw::register_format("ExcludeClipboardContentFromMonitorProcessing");
        raw::empty().map_err(|error| format!("无法准备临时剪贴板：{error}"))?;
        let write_temporary = || -> Result<u32, String> {
            raw::set_string_with(text, clipboard_win::options::NoClear)
                .map_err(|error| format!("无法写入临时听写文字：{error}"))?;

            // Official Windows opt-out for clipboard monitoring/history. This
            // keeps an implementation detail out of Win+V and cloud clipboard.
            if let Some(format) = marker {
                raw::set_without_clear(format.get(), &0_u32.to_ne_bytes())
                    .map_err(|error| format!("无法标记临时剪贴板：{error}"))?;
            }

            raw::seq_num()
                .map(|value| value.get())
                .ok_or_else(|| "无法读取剪贴板版本号".to_string())
        };
        let temporary_sequence = match write_temporary() {
            Ok(sequence) => sequence,
            Err(error) => {
                return match write_windows_snapshot(&formats) {
                    Ok(()) => Err(error),
                    Err(restore_error) => {
                        Err(format!("{error}；恢复原剪贴板也失败：{restore_error}"))
                    }
                };
            }
        };
        Ok(Self {
            formats,
            temporary_sequence,
        })
    }

    fn finish<R: Runtime>(self, _app: &AppHandle<R>) -> ClipboardRestoreOutcome {
        use clipboard_win::{raw, Clipboard};

        if raw::seq_num().map(|value| value.get()) != Some(self.temporary_sequence) {
            return ClipboardRestoreOutcome::ExternalChangePreserved;
        }
        let _clipboard = match Clipboard::new_attempts(20) {
            Ok(clipboard) => clipboard,
            Err(error) => {
                eprintln!("[delivery] 无法打开 Windows 剪贴板以恢复：{error}");
                return ClipboardRestoreOutcome::Failed;
            }
        };
        if raw::seq_num().map(|value| value.get()) != Some(self.temporary_sequence) {
            return ClipboardRestoreOutcome::ExternalChangePreserved;
        }
        if let Err(error) = write_windows_snapshot(&self.formats) {
            eprintln!("[delivery] 恢复 Windows 剪贴板失败：{error}");
            return ClipboardRestoreOutcome::Failed;
        }
        ClipboardRestoreOutcome::Restored
    }
}

#[cfg(target_os = "windows")]
fn write_windows_snapshot(formats: &[(u32, Vec<u8>)]) -> Result<(), String> {
    use clipboard_win::raw;

    raw::empty().map_err(|error| format!("无法清空临时剪贴板：{error}"))?;
    for (format, data) in formats {
        raw::set_without_clear(*format, data)
            .map_err(|error| format!("恢复剪贴板格式 {format} 失败：{error}"))?;
    }
    Ok(())
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
                format!("set p to first application process whose name is \"{escaped}\"")
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

/// Best-effort paste trigger used only inside a guarded clipboard transaction.
/// The caller restores the original clipboard unless newer user data has replaced it.
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

#[cfg(test)]
mod clipboard_transaction_tests {
    use super::{choose_delivery_target, clipboard_is_still_temporary, InsertionProbe};

    #[cfg(target_os = "macos")]
    fn assert_snapshot_preserves_original(
        captured: &super::ClipboardSnapshot,
        original: &super::ClipboardSnapshot,
    ) {
        assert_eq!(captured.items.len(), original.items.len());
        for (captured_item, original_item) in captured.items.iter().zip(&original.items) {
            for original_representation in original_item {
                assert!(captured_item.contains(original_representation));
            }
        }
    }

    #[test]
    fn restores_only_while_jackvoice_still_owns_the_temporary_clipboard() {
        assert!(clipboard_is_still_temporary(42, 42, true));
        assert!(!clipboard_is_still_temporary(42, 43, true));
        assert!(!clipboard_is_still_temporary(42, 42, false));
    }

    #[test]
    fn screenshot_utility_does_not_replace_the_original_dictation_target() {
        assert_eq!(
            choose_delivery_target(
                Some("Notes".into()),
                Some("Snipaste".into()),
                InsertionProbe::NotInsertable,
            ),
            Some("Notes".into())
        );
        assert_eq!(
            choose_delivery_target(
                Some("Notes".into()),
                Some("Snipaste".into()),
                InsertionProbe::Unknown,
            ),
            Some("Notes".into())
        );
    }

    #[test]
    fn deliberate_switch_to_another_text_app_becomes_the_delivery_target() {
        assert_eq!(
            choose_delivery_target(
                Some("Notes".into()),
                Some("Chrome".into()),
                InsertionProbe::Insertable,
            ),
            Some("Chrome".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_snapshot_round_trips_multiple_items_and_formats() {
        use super::{
            capture_macos_clipboard, write_macos_snapshot, ClipboardRepresentation,
            ClipboardSnapshot,
        };
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let original = ClipboardSnapshot {
            items: vec![
                vec![
                    ClipboardRepresentation {
                        type_name: "public.utf8-plain-text".into(),
                        data: "保留文字".as_bytes().to_vec(),
                    },
                    ClipboardRepresentation {
                        type_name: "public.rtf".into(),
                        data: br#"{\rtf1 preserved}"#.to_vec(),
                    },
                ],
                vec![
                    ClipboardRepresentation {
                        type_name: "public.png".into(),
                        data: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a],
                    },
                    ClipboardRepresentation {
                        type_name: "public.file-url".into(),
                        data: b"file:///tmp/example.png".to_vec(),
                    },
                ],
            ],
        };

        write_macos_snapshot(&pasteboard, &original).expect("write test pasteboard");
        let captured = capture_macos_clipboard(&pasteboard).expect("capture test pasteboard");
        // AppKit may synthesize equivalent representations (for example UTF-16 text),
        // but every original item and representation must survive byte-for-byte.
        assert_snapshot_preserves_original(&captured, &original);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn temporary_text_is_marked_and_original_snapshot_can_be_restored() {
        use super::{
            capture_macos_clipboard, macos_clipboard_has_temporary_marker, write_macos_snapshot,
            write_temporary_macos_text, ClipboardRepresentation, ClipboardSnapshot,
        };
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let original = ClipboardSnapshot {
            items: vec![vec![ClipboardRepresentation {
                type_name: "public.png".into(),
                data: vec![1, 2, 3, 4, 5],
            }]],
        };
        write_macos_snapshot(&pasteboard, &original).expect("seed image clipboard");

        let original_change_count = pasteboard.changeCount();
        write_temporary_macos_text(&pasteboard, "临时听写", original_change_count)
            .expect("write temporary text");
        assert!(macos_clipboard_has_temporary_marker(&pasteboard));

        write_macos_snapshot(&pasteboard, &original).expect("restore image clipboard");
        let restored = capture_macos_clipboard(&pasteboard).expect("read restored clipboard");
        assert_snapshot_preserves_original(&restored, &original);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn temporary_write_refuses_to_overwrite_a_newer_clipboard() {
        use super::{
            capture_macos_clipboard, write_macos_snapshot, write_temporary_macos_text,
            ClipboardRepresentation, ClipboardSnapshot,
        };
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let old = ClipboardSnapshot {
            items: vec![vec![ClipboardRepresentation {
                type_name: "public.png".into(),
                data: vec![1, 2, 3],
            }]],
        };
        write_macos_snapshot(&pasteboard, &old).expect("seed old clipboard");
        let stale_change_count = pasteboard.changeCount();

        let newer = ClipboardSnapshot {
            items: vec![vec![ClipboardRepresentation {
                type_name: "public.png".into(),
                data: vec![4, 5, 6],
            }]],
        };
        write_macos_snapshot(&pasteboard, &newer).expect("simulate newer user copy");

        let error = write_temporary_macos_text(&pasteboard, "不得覆盖", stale_change_count)
            .expect_err("stale transaction must be rejected");
        assert!(!error.live_clipboard_changed);
        let current = capture_macos_clipboard(&pasteboard).expect("read newer clipboard");
        assert_snapshot_preserves_original(&current, &newer);
    }
}
