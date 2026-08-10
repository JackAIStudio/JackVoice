use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_FN: u64 = 0x0080_0000;
const SHORTCUT_MODIFIER_MASK: u64 =
    FLAG_SHIFT | FLAG_CONTROL | FLAG_OPTION | FLAG_COMMAND | FLAG_FN;

#[derive(Default)]
pub struct ShortcutCaptureState {
    recording: AtomicBool,
}

impl ShortcutCaptureState {
    pub fn start(&self) {
        self.recording.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.recording.store(false, Ordering::Release);
    }

    fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutCaptureEvent {
    accelerator: String,
    error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FunctionShortcut {
    key_code: u16,
    modifiers: u64,
}

pub fn uses_fn_modifier(accelerator: &str) -> bool {
    accelerator
        .split('+')
        .any(|token| token.trim().eq_ignore_ascii_case("fn"))
}

pub fn validate_fn_shortcut(accelerator: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        FunctionShortcut::parse(accelerator).map(|_| ())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = accelerator;
        Err("Fn 组合快捷键目前仅支持 macOS。".into())
    }
}

impl FunctionShortcut {
    fn parse(accelerator: &str) -> Result<Self, String> {
        let tokens = accelerator.split('+').map(str::trim).collect::<Vec<_>>();
        if tokens.len() < 2 || tokens.iter().any(|token| token.is_empty()) {
            return Err("Fn 快捷键格式无效，请按 Fn 与另一个按键。".into());
        }

        let (key, modifier_tokens) = tokens
            .split_last()
            .expect("Fn shortcut always contains at least two tokens");
        let mut modifiers = 0;
        for token in modifier_tokens {
            let flag = match token.to_ascii_lowercase().as_str() {
                "fn" => FLAG_FN,
                "control" | "ctrl" => FLAG_CONTROL,
                "alt" | "option" => FLAG_OPTION,
                "shift" => FLAG_SHIFT,
                "meta" | "command" | "cmd" | "super" => FLAG_COMMAND,
                _ => return Err(format!("不支持的快捷键修饰键：{token}")),
            };
            if modifiers & flag != 0 {
                return Err(format!("快捷键修饰键重复：{token}"));
            }
            modifiers |= flag;
        }
        if modifiers & FLAG_FN == 0 {
            return Err("Fn 快捷键必须包含 Fn 修饰键。".into());
        }

        let key_code = key_code_for_token(key)
            .ok_or_else(|| format!("Fn 暂不支持与 {key} 组合，请换一个按键。"))?;
        Ok(Self {
            key_code,
            modifiers,
        })
    }

    fn matches(self, key_code: u16, event_flags: u64) -> bool {
        self.key_code == key_code && self.modifiers == event_flags & SHORTCUT_MODIFIER_MASK
    }
}

fn key_code_for_token(token: &str) -> Option<u16> {
    Some(match token.to_ascii_uppercase().as_str() {
        "A" | "KEYA" => 0x00,
        "S" | "KEYS" => 0x01,
        "D" | "KEYD" => 0x02,
        "F" | "KEYF" => 0x03,
        "H" | "KEYH" => 0x04,
        "G" | "KEYG" => 0x05,
        "Z" | "KEYZ" => 0x06,
        "X" | "KEYX" => 0x07,
        "C" | "KEYC" => 0x08,
        "V" | "KEYV" => 0x09,
        "B" | "KEYB" => 0x0b,
        "Q" | "KEYQ" => 0x0c,
        "W" | "KEYW" => 0x0d,
        "E" | "KEYE" => 0x0e,
        "R" | "KEYR" => 0x0f,
        "Y" | "KEYY" => 0x10,
        "T" | "KEYT" => 0x11,
        "1" | "DIGIT1" => 0x12,
        "2" | "DIGIT2" => 0x13,
        "3" | "DIGIT3" => 0x14,
        "4" | "DIGIT4" => 0x15,
        "6" | "DIGIT6" => 0x16,
        "5" | "DIGIT5" => 0x17,
        "=" | "EQUAL" => 0x18,
        "9" | "DIGIT9" => 0x19,
        "7" | "DIGIT7" => 0x1a,
        "-" | "MINUS" => 0x1b,
        "8" | "DIGIT8" => 0x1c,
        "0" | "DIGIT0" => 0x1d,
        "]" | "BRACKETRIGHT" => 0x1e,
        "O" | "KEYO" => 0x1f,
        "U" | "KEYU" => 0x20,
        "[" | "BRACKETLEFT" => 0x21,
        "I" | "KEYI" => 0x22,
        "P" | "KEYP" => 0x23,
        "ENTER" | "RETURN" => 0x24,
        "L" | "KEYL" => 0x25,
        "J" | "KEYJ" => 0x26,
        "'" | "QUOTE" => 0x27,
        "K" | "KEYK" => 0x28,
        ";" | "SEMICOLON" => 0x29,
        "\\" | "BACKSLASH" => 0x2a,
        "," | "COMMA" => 0x2b,
        "/" | "SLASH" => 0x2c,
        "N" | "KEYN" => 0x2d,
        "M" | "KEYM" => 0x2e,
        "." | "PERIOD" => 0x2f,
        "TAB" => 0x30,
        "SPACE" => 0x31,
        "`" | "BACKQUOTE" => 0x32,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F10" => 0x6d,
        "F12" => 0x6f,
        "F4" => 0x76,
        "F2" => 0x78,
        "F1" => 0x7a,
        "LEFT" | "ARROWLEFT" => 0x7b,
        "RIGHT" | "ARROWRIGHT" => 0x7c,
        "DOWN" | "ARROWDOWN" => 0x7d,
        "UP" | "ARROWUP" => 0x7e,
        _ => return None,
    })
}

fn token_for_key_code(key_code: u16) -> Option<&'static str> {
    Some(match key_code {
        0x00 => "A",
        0x01 => "S",
        0x02 => "D",
        0x03 => "F",
        0x04 => "H",
        0x05 => "G",
        0x06 => "Z",
        0x07 => "X",
        0x08 => "C",
        0x09 => "V",
        0x0b => "B",
        0x0c => "Q",
        0x0d => "W",
        0x0e => "E",
        0x0f => "R",
        0x10 => "Y",
        0x11 => "T",
        0x12 => "1",
        0x13 => "2",
        0x14 => "3",
        0x15 => "4",
        0x16 => "6",
        0x17 => "5",
        0x18 => "Equal",
        0x19 => "9",
        0x1a => "7",
        0x1b => "-",
        0x1c => "8",
        0x1d => "0",
        0x1e => "]",
        0x1f => "O",
        0x20 => "U",
        0x21 => "[",
        0x22 => "I",
        0x23 => "P",
        0x24 => "Enter",
        0x25 => "L",
        0x26 => "J",
        0x27 => "'",
        0x28 => "K",
        0x29 => ";",
        0x2a => "\\",
        0x2b => ",",
        0x2c => "/",
        0x2d => "N",
        0x2e => "M",
        0x2f => ".",
        0x30 => "Tab",
        0x31 => "Space",
        0x32 => "`",
        0x60 => "F5",
        0x61 => "F6",
        0x62 => "F7",
        0x63 => "F3",
        0x64 => "F8",
        0x65 => "F9",
        0x67 => "F11",
        0x6d => "F10",
        0x6f => "F12",
        0x76 => "F4",
        0x78 => "F2",
        0x7a => "F1",
        0x7b => "Left",
        0x7c => "Right",
        0x7d => "Down",
        0x7e => "Up",
        _ => return None,
    })
}

fn accelerator_for_fn_event(key_code: u16, event_flags: u64) -> Result<String, String> {
    let key = token_for_key_code(key_code)
        .ok_or_else(|| "Fn 暂不支持与这个按键组合，请换一个按键。".to_string())?;
    let mut tokens = vec!["Fn"];
    if event_flags & FLAG_CONTROL != 0 {
        tokens.push("Control");
    }
    if event_flags & FLAG_OPTION != 0 {
        tokens.push("Alt");
    }
    if event_flags & FLAG_SHIFT != 0 {
        tokens.push("Shift");
    }
    if event_flags & FLAG_COMMAND != 0 {
        tokens.push("Meta");
    }
    tokens.push(key);
    Ok(tokens.join("+"))
}

#[cfg(target_os = "macos")]
pub fn install_fn_shortcut_monitor(app: AppHandle) {
    use core_foundation::runloop::CFRunLoop;
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
    };
    use std::time::Duration;

    let _ = std::thread::Builder::new()
        .name("jackvoice-fn-shortcut".into())
        .spawn(move || {
            let mut waiting_for_permission_logged = false;
            loop {
                let callback_app = app.clone();
                let result = CGEventTap::with_enabled(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    vec![CGEventType::KeyDown, CGEventType::KeyUp],
                    move |_proxy, event_type, event| {
                        if matches!(
                            event_type,
                            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
                        ) {
                            CFRunLoop::get_current().stop();
                            return CallbackResult::Keep;
                        }

                        let key_code = event
                            .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                            as u16;
                        let flags = event.get_flags().bits();
                        let is_repeat = event
                            .get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT)
                            != 0;

                        if let Some(capture) = callback_app.try_state::<ShortcutCaptureState>() {
                            if capture.is_recording()
                                && flags & FLAG_FN != 0
                                && matches!(event_type, CGEventType::KeyDown)
                            {
                                if is_repeat {
                                    return CallbackResult::Drop;
                                }
                                capture.stop();
                                let (accelerator, error) =
                                    match accelerator_for_fn_event(key_code, flags) {
                                        Ok(accelerator) => (accelerator, String::new()),
                                        Err(error) => (String::new(), error),
                                    };
                                let _ = callback_app.emit(
                                    "jackvoice://shortcut-captured",
                                    ShortcutCaptureEvent { accelerator, error },
                                );
                                return CallbackResult::Drop;
                            }
                        }

                        let Some(state) = callback_app.try_state::<crate::session::AppState>()
                        else {
                            return CallbackResult::Keep;
                        };
                        let configured = state.shortcut();
                        let Ok(shortcut) = FunctionShortcut::parse(&configured) else {
                            return CallbackResult::Keep;
                        };
                        if !shortcut.matches(key_code, flags) {
                            return CallbackResult::Keep;
                        }

                        if matches!(event_type, CGEventType::KeyDown) && !is_repeat {
                            let toggle_app = callback_app.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Some(state) =
                                    toggle_app.try_state::<crate::session::AppState>()
                                {
                                    let _ = state.toggle(toggle_app.clone()).await;
                                }
                            });
                        }
                        CallbackResult::Drop
                    },
                    || {
                        waiting_for_permission_logged = false;
                        eprintln!("[shortcut] Fn 全局快捷键监听已启动");
                        CFRunLoop::run_current();
                    },
                );

                if result.is_err() {
                    if !waiting_for_permission_logged {
                        eprintln!("[shortcut] Fn 监听等待辅助功能权限");
                        waiting_for_permission_logged = true;
                    }
                } else {
                    eprintln!("[shortcut] Fn 监听已被系统停用，正在恢复");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

#[cfg(not(target_os = "macos"))]
pub fn install_fn_shortcut_monitor(_app: AppHandle) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_fn_as_a_modifier_token_only() {
        assert!(uses_fn_modifier("Fn+Space"));
        assert!(uses_fn_modifier("Control+fn+K"));
        assert!(!uses_fn_modifier("F1"));
        assert!(!uses_fn_modifier("Function+Space"));
    }

    #[test]
    fn fn_space_matches_the_native_flags_exactly() {
        let shortcut = FunctionShortcut::parse("Fn+Space").unwrap();
        assert!(shortcut.matches(0x31, FLAG_FN));
        assert!(!shortcut.matches(0x31, FLAG_FN | FLAG_SHIFT));
        assert!(!shortcut.matches(0x00, FLAG_FN));
    }

    #[test]
    fn fn_can_be_combined_with_other_modifiers() {
        let shortcut = FunctionShortcut::parse("Fn+Control+Shift+K").unwrap();
        assert!(shortcut.matches(0x28, FLAG_FN | FLAG_CONTROL | FLAG_SHIFT));
    }

    #[test]
    fn native_fn_space_is_serialized_for_settings() {
        assert_eq!(accelerator_for_fn_event(0x31, FLAG_FN).unwrap(), "Fn+Space");
        assert_eq!(
            accelerator_for_fn_event(0x28, FLAG_FN | FLAG_COMMAND).unwrap(),
            "Fn+Meta+K"
        );
    }

    #[test]
    fn rejects_incomplete_or_unknown_fn_shortcuts() {
        assert!(FunctionShortcut::parse("Fn").is_err());
        assert!(FunctionShortcut::parse("Fn+Escape").is_err());
        assert!(FunctionShortcut::parse("Fn+Fn+Space").is_err());
    }
}
