use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Runtime};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use tauri_plugin_clipboard_manager::ClipboardExt;

/// 临时写入的听写文字必须在目标应用真正读取之后再换回原剪贴板。
/// Electron / 大段文本读取剪贴板可能晚于 500ms，恢复得太早会把听写文字
/// 换成旧内容，表现为“光标在闪却粘贴不进去”。
const CLIPBOARD_RESTORE_DELAY: std::time::Duration = std::time::Duration::from_millis(900);
/// 目标应用触发读取后，再短暂保留临时剪贴板的宽限时间：
/// 部分应用（如 Chromium / Electron）一次粘贴可能连续读取多次。
#[cfg(target_os = "macos")]
const POST_CONSUME_GRACE_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(target_os = "macos")]
const PASTE_CONSUME_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(15);
/// 用户按完 Option+Space 后可能还按着修饰键，此时合成的 Cmd+V 会变成
/// ⌥⌘V，多数应用不会粘贴。等修饰键松开再发按键。
const MODIFIER_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(900);
/// 激活目标应用是异步的：必须等它真的回到前台再粘贴，否则 Cmd+V 会落到
/// 别的应用（甚至 JackVoice 自己）。
const FRONTMOST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(900);

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
const TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];

/// Roles that definitely represent non-text, read-only system or UI controls
/// where Cmd+V paste makes no sense and should be skipped (unless the element
/// explicitly reports editable attributes).
const DEFINITELY_NON_TEXT_ROLES: &[&str] = &[
    "AXMenuBar",
    "AXMenuBarItem",
    "AXMenu",
    "AXMenuItem",
    "AXDock",
    "AXButton",
    "AXRadioButton",
    "AXCheckBox",
    "AXImage",
    "AXScrollBar",
    "AXProgressIndicator",
    "AXSlider",
    "AXColorWell",
    "AXStaticText",
];

/// 结构/列表/空白网页角色：自身不可编辑、且无 `AXEditableAncestor`、且
/// `AXSelectedTextRange` 也不可写时，属于明确的无光标区域（如 Chrome/Safari
/// 空白网页、Finder/IDE 文件树、Telegram 列表、DaVinci Resolve 分屏），
/// 直接跳过 Cmd+V，既避免误触发网页全局 onpaste，也免去系统错误提示音。
const NON_EDITABLE_STRUCTURE_ROLES: &[&str] = &[
    "AXToolbar",
    "AXTabGroup",
    "AXTable",
    "AXList",
    "AXOutline",
    "AXLink",
    "AXHeading",
    "AXCell",
    "AXRow",
    "AXColumn",
    "AXSplitGroup",
    "AXWebArea",
];

/// 通用窗口/容器角色：在严格遵守原生 AppKit AX 树的苹果系统应用里，没有
/// 可编辑属性就说明没有激活光标；在第三方自绘应用（如 GPU 终端、远程桌面）
/// 里仍可能承载真实光标，按 Unknown 放行并交由剪贴板消费确认裁决。
const GENERIC_CONTAINER_ROLES: &[&str] = &[
    "AXWindow",
    "AXSheet",
    "AXDrawer",
    "AXScrollArea",
    "AXGroup",
    "AXUnknown",
    "AXLayoutArea",
    "AXLayoutItem",
];

/// 把文字投递到 `target_pid` 指向的应用（`None` 表示只按当前前台应用处理）。
///
/// 顺序很重要：先等用户松开 Option/Space，再确认目标应用真的回到前台，
/// 最后才写剪贴板并合成 Cmd+V。任何一个环节提前，按键都会落到错误的应用。
pub async fn deliver_text_to<R: Runtime>(
    app: &AppHandle<R>,
    text: &str,
    probe: InsertionProbe,
    target_pid: Option<i32>,
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

    wait_for_shortcut_modifiers_released(MODIFIER_RELEASE_TIMEOUT).await;
    if let Some(pid) = target_pid {
        if !wait_until_frontmost(pid, FRONTMOST_TIMEOUT).await {
            // 目标应用没有回到前台时粘贴必然落到别处，宁可让用户手动重试。
            return DeliveryResult {
                pasted: false,
                copied: false,
                message: "目标应用没有回到前台，本次未自动插入；可点击“重试”或手动复制。".into(),
            };
        }
    }

    // Keep the insertion mechanism identical to the original working path:
    // write the transcript to the pasteboard and post Cmd+V. Clipboard
    // preservation is an outer transaction only; it must never replace the
    // proven paste path with an AXSelectedText write. Multi-line AXTextArea
    // controls can report that such writes succeeded while discarding them.
    //
    // 主线程只负责建立剪贴板事务（含延迟数据提供者）后立即返回，把 Cmd+V
    // 投递放到后台线程：这样目标应用读取剪贴板时，AppKit 主线程 RunLoop
    // 处于完全空闲状态，可以微秒级回调 provideDataForType:，不会发生死锁。
    let app_handle = app.clone();
    let owned_text = text.to_string();
    let prepared = run_on_main_thread(app, move || {
        ClipboardTransaction::begin(&app_handle, &owned_text)
    })
    .await;

    let transaction = match prepared {
        Ok(Ok(value)) => value,
        Ok(Err(error)) | Err(error) => {
            eprintln!("[delivery] 无法建立透明剪贴板事务：{error}");
            // 保留原剪贴板失败时也要让文字落地：退化成直接写入听写文字并粘贴，
            // 用户至少拿得到结果，而不是既没粘贴又丢了剪贴板。
            let fallback_text = text.to_string();
            let app_handle = app.clone();
            let fallback = run_on_main_thread(app, move || {
                write_temporary_text_ignoring_previous(&app_handle, &fallback_text)
            })
            .await;
            if !matches!(fallback, Ok(Ok(()))) {
                return DeliveryResult {
                    pasted: false,
                    copied: false,
                    message: "本次未自动粘贴，也无法写入剪贴板。听写文字仍可从历史记录找回。"
                        .into(),
                };
            }
            let paste_triggered = tokio::task::spawn_blocking(simulate_paste)
                .await
                .unwrap_or(false);
            if paste_triggered {
                tokio::time::sleep(CLIPBOARD_RESTORE_DELAY).await;
            }
            return DeliveryResult {
                pasted: paste_triggered,
                copied: false,
                message: if paste_triggered {
                    "已插入到当前输入位置；原剪贴板未能保留，现在剪贴板里是本次听写文字。".into()
                } else {
                    "未能触发自动粘贴；听写文字已放入剪贴板，可直接粘贴。".into()
                },
            };
        }
    };

    let consumed_flag = transaction.consumed_flag();
    let paste_triggered = tokio::task::spawn_blocking(simulate_paste)
        .await
        .unwrap_or(false);

    let waited_consumed = if paste_triggered {
        if let Some(flag) = consumed_flag.as_ref() {
            #[cfg(target_os = "macos")]
            {
                wait_for_paste_consumption(flag, CLIPBOARD_RESTORE_DELAY, POST_CONSUME_GRACE_DELAY)
                    .await
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = flag;
                tokio::time::sleep(CLIPBOARD_RESTORE_DELAY).await;
                true
            }
        } else {
            tokio::time::sleep(CLIPBOARD_RESTORE_DELAY).await;
            true
        }
    } else {
        false
    };

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

    let target_consumed = match consumed_flag.as_ref() {
        Some(flag) => waited_consumed || flag.load(Ordering::Acquire),
        None => true,
    };

    if !target_consumed {
        eprintln!(
            "[delivery] 目标应用在 {}ms 内未读取临时剪贴板，判定未检测到输入焦点",
            CLIPBOARD_RESTORE_DELAY.as_millis()
        );
        return DeliveryResult {
            pasted: false,
            copied: false,
            message: match restore_outcome {
                ClipboardRestoreOutcome::Restored => {
                    "未检测到输入焦点，未自动插入；原剪贴板保持不变。可重试或点击“复制”。".into()
                }
                ClipboardRestoreOutcome::ExternalChangePreserved => {
                    "未检测到输入焦点，未自动插入；已保留你刚刚复制的新内容。可重试或点击“复制”。"
                        .into()
                }
                ClipboardRestoreOutcome::Failed => {
                    "未检测到输入焦点，且恢复原剪贴板失败。请检查剪贴板，并从听写历史找回文字。"
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

/// 异步等待目标应用从延迟剪贴板提供者中读取听写文字。
///
/// 一旦检测到 `consumed` 变为 `true`，再宽限 `post_consume_grace` 后提前结束，
/// 既给多阶段读取的应用（如 Electron）留足窗口，又比死等 900ms 更快归还剪贴板。
#[cfg(target_os = "macos")]
async fn wait_for_paste_consumption(
    consumed: &Arc<AtomicBool>,
    timeout: std::time::Duration,
    post_consume_grace: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if consumed.load(Ordering::Acquire) {
            if !post_consume_grace.is_zero() {
                tokio::time::sleep(post_consume_grace).await;
            }
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return consumed.load(Ordering::Acquire);
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(remaining.min(PASTE_CONSUME_POLL_INTERVAL)).await;
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

/// 等用户把快捷键的修饰键都松开。Option 还按着时合成的 Cmd+V 会被目标
/// 应用当成 ⌥⌘V（“粘贴并匹配样式”或直接无反应），这是“有时候粘不进去”
/// 最常见的成因。
async fn wait_for_shortcut_modifiers_released(timeout: std::time::Duration) {
    #[cfg(target_os = "macos")]
    {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !macos_modifier_keys_down() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        eprintln!("[delivery] 修饰键在超时内没有松开，仍然尝试粘贴");
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = timeout;
    }
}

/// 等目标应用真正回到前台。激活是异步的，提前发 Cmd+V 会落进别的应用。
async fn wait_until_frontmost(pid: i32, timeout: std::time::Duration) -> bool {
    #[cfg(target_os = "macos")]
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if crate::overlay::frontmost_pid() == Some(pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, timeout);
        true
    }
}

#[cfg(target_os = "macos")]
fn macos_modifier_keys_down() -> bool {
    const SHORTCUT_MODIFIER_MASK: u64 = 0x0002_0000 // shift
        | 0x0004_0000 // control
        | 0x0008_0000 // option
        | 0x0010_0000 // command
        | 0x0080_0000; // fn

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// `CGEventSourceFlagsState(CGEventSourceStateID)`，0 表示合并会话状态。
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    let flags = unsafe { CGEventSourceFlagsState(0) };
    flags & SHORTCUT_MODIFIER_MASK != 0
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
mod macos_pasteboard_provider {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use objc2::rc::Retained;
    use objc2::{define_class, msg_send, AnyThread, DefinedClass};
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType,
        NSPasteboardTypeString,
    };
    use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

    #[derive(Debug)]
    pub struct ProviderIvars {
        text: String,
        consumed: Arc<AtomicBool>,
    }

    define_class!(
        /// 仅对 `NSPasteboardTypeString` 延迟供数，用来精确感知目标应用是否
        /// 真的在收到 Cmd+V 后读取了听写文字。安全标记（TransientType 等）
        /// 仍直接写入，避免剪贴板管理器查询标记时误触发回调。
        #[unsafe(super(NSObject))]
        #[name = "JVTemporaryPasteboardProvider"]
        #[ivars = ProviderIvars]
        #[derive(Debug)]
        pub struct TemporaryTextProvider;

        unsafe impl NSObjectProtocol for TemporaryTextProvider {}

        unsafe impl NSPasteboardItemDataProvider for TemporaryTextProvider {
            #[unsafe(method(pasteboard:item:provideDataForType:))]
            fn provide_data_for_type(
                &self,
                _pasteboard: Option<&NSPasteboard>,
                item: &NSPasteboardItem,
                r#type: &NSPasteboardType,
            ) {
                let string_type = unsafe { NSPasteboardTypeString };
                if r#type == string_type {
                    let text = NSString::from_str(&self.ivars().text);
                    if item.setString_forType(&text, string_type) {
                        self.ivars().consumed.store(true, Ordering::Release);
                    }
                }
            }
        }
    );

    impl TemporaryTextProvider {
        pub fn new(text: String, consumed: Arc<AtomicBool>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(ProviderIvars { text, consumed });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// 强持有延迟提供者直到事务结束（恢复原剪贴板之后）再释放。
    ///
    /// SAFETY: 仅在主线程构造、通过 AppKit 回调访问并在主线程 `finish()` 中释放；
    /// 内部 ivars 均为 `Send + Sync`，即便任务中途被取消在后台 drop，Objective-C
    /// `objc_release` 与 Rust 字段析构均线程安全。
    pub struct ProviderGuard {
        _inner: Retained<TemporaryTextProvider>,
    }

    unsafe impl Send for ProviderGuard {}

    impl ProviderGuard {
        pub fn new(provider: Retained<TemporaryTextProvider>) -> Self {
            Self { _inner: provider }
        }
    }
}

#[cfg(target_os = "macos")]
struct ClipboardTransaction {
    snapshot: ClipboardSnapshot,
    temporary_change_count: isize,
    consumed: Arc<AtomicBool>,
    _provider: macos_pasteboard_provider::ProviderGuard,
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
        let consumed = Arc::new(AtomicBool::new(false));
        let provider = match write_temporary_macos_text(
            &pasteboard,
            text,
            original_change_count,
            Arc::clone(&consumed),
        ) {
            Ok(provider) => provider,
            Err(error) => {
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
        };

        Ok(Self {
            snapshot,
            temporary_change_count: pasteboard.changeCount(),
            consumed,
            _provider: macos_pasteboard_provider::ProviderGuard::new(provider),
        })
    }

    fn consumed_flag(&self) -> Option<Arc<AtomicBool>> {
        Some(Arc::clone(&self.consumed))
    }

    fn finish<R: Runtime>(self, _app: &AppHandle<R>) -> ClipboardRestoreOutcome {
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::generalPasteboard();
        let outcome = if !clipboard_is_still_temporary(
            self.temporary_change_count,
            pasteboard.changeCount(),
            macos_clipboard_has_temporary_marker(&pasteboard),
        ) {
            // The user (or another app) copied something while JackVoice was
            // delivering. Their newest clipboard always wins.
            ClipboardRestoreOutcome::ExternalChangePreserved
        } else {
            match write_macos_snapshot(&pasteboard, &self.snapshot) {
                Ok(()) => ClipboardRestoreOutcome::Restored,
                Err(error) => {
                    eprintln!("[delivery] 恢复 macOS 剪贴板失败：{error}");
                    ClipboardRestoreOutcome::Failed
                }
            }
        };
        // 确保在剪贴板恢复完成之后才释放延迟提供者。
        drop(self._provider);
        outcome
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
    consumed: Arc<AtomicBool>,
) -> Result<
    objc2::rc::Retained<macos_pasteboard_provider::TemporaryTextProvider>,
    MacosTemporaryWriteError,
> {
    use objc2::runtime::ProtocolObject;
    use objc2::AnyThread;
    use objc2_app_kit::{NSPasteboardContentsOptions, NSPasteboardItem, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSData, NSString};

    let item = NSPasteboardItem::init(NSPasteboardItem::alloc());
    let provider =
        macos_pasteboard_provider::TemporaryTextProvider::new(text.to_string(), consumed);
    let string_type = unsafe { NSPasteboardTypeString };
    let lazy_types = NSArray::from_slice(&[string_type]);
    if !item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &lazy_types) {
        return Err(MacosTemporaryWriteError {
            message: "无法把听写文字写入临时剪贴板".into(),
            live_clipboard_changed: false,
        });
    }

    // 三个防记录标记直接写入，不走 lazy provider：
    // 剪贴板管理器查看 types 或标记值时不会误触发 consumed。
    let empty = NSData::with_bytes(&[]);
    for marker in [
        MACOS_TRANSIENT_TYPE,
        "org.nspasteboard.AutoGeneratedType",
        "org.nspasteboard.ConcealedType",
    ] {
        let marker = NSString::from_str(marker);
        if !item.setData_forType(&empty, &marker) {
            return Err(MacosTemporaryWriteError {
                message: "无法为临时剪贴板写入安全标记".into(),
                live_clipboard_changed: false,
            });
        }
    }

    let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
    if pasteboard.changeCount() != expected_change_count {
        return Err(MacosTemporaryWriteError {
            message: "写入前剪贴板发生了变化，已保留用户的新内容".into(),
            live_clipboard_changed: false,
        });
    }
    // CurrentHostOnly 阻止 Universal Clipboard / Handoff 跨设备拉取临时听写文字。
    pasteboard.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
    if pasteboard.writeObjects(&objects) {
        Ok(provider)
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

    fn consumed_flag(&self) -> Option<Arc<AtomicBool>> {
        None
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

    fn consumed_flag(&self) -> Option<Arc<AtomicBool>> {
        None
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

/// Apps with proprietary / non-standard rendering engines (such as WeChat / WXWork)
/// that do not expose accessibility UI elements (`AXFocusedUIElement` is always
/// missing value), but do support standard system Cmd+V paste when frontmost.
///
/// `app_mode_loader` is the process name of a Chrome-installed web app (PWA).
/// The shim is the frontmost process and owns the window, but the blinking
/// caret stays inside Chrome's renderer and is never reported as
/// `AXFocusedUIElement`. Cmd+V still reaches that caret.
pub fn is_unsupported_ax_app(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    matches!(
        lower.as_str(),
        "wechat"
            | "微信"
            | "wxwork"
            | "企业微信"
            | "qq"
            | "tencentmeeting"
            | "wemeet"
            | "dingtalk"
            | "钉钉"
            | "jackdsh"
            | "cursor"
            | "code"
            | "visual studio code"
            | "feishu"
            | "飞书"
            | "lark"
            | "slack"
            | "discord"
            | "obsidian"
            | "notion"
            | "chatgpt"
            | "app_mode_loader"
            | "workbuddy"
            | "workbuddy ai"
            | "electron"
    ) || lower.starts_with("wechat")
        || lower.starts_with("wxwork")
        || lower.starts_with("dingtalk")
        || lower.starts_with("jackdsh")
        || lower.starts_with("cursor")
        || lower.starts_with("feishu")
        || lower.starts_with("lark")
        || lower.starts_with("workbuddy")
        || lower.starts_with("electron")
}

/// 严格遵循原生 AppKit AX 树的苹果系统应用：只要真的有光标激活，它们
/// 必定会暴露 `AXTextField` / `AXTextArea` / `AXSearchField` / `AXComboBox`
/// 或可写属性；如果焦点停在无焦点 / `AXWindow` / `AXScrollArea` / `AXGroup`
/// 且没有任何可编辑属性，就可以 100% 确定当前没有光标，直接跳过 Cmd+V，
/// 既免去系统错误提示音，又防止向画布误粘贴。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn is_strict_native_ax_app(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    matches!(
        lower.as_str(),
        "finder"
            | "访达"
            | "notes"
            | "备忘录"
            | "preview"
            | "预览"
            | "photos"
            | "照片"
            | "system settings"
            | "system preferences"
            | "系统设置"
            | "系统偏好设置"
            | "activity monitor"
            | "活动监视器"
            | "app store"
            | "mail"
            | "邮件"
            | "textedit"
            | "文本编辑"
            | "calendar"
            | "日历"
            | "reminders"
            | "提醒事项"
            | "music"
            | "音乐"
            | "keynote"
            | "pages"
            | "numbers"
    )
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AxFocusElement {
    pub app_name: String,
    pub role: String,
    pub subrole: String,
    pub value_settable: bool,
    pub selected_text_settable: bool,
    pub selected_text_range_settable: bool,
    pub has_editable_ancestor: bool,
    pub has_selected_text: bool,
    pub has_selected_text_range: bool,
    pub insertion_point_line: Option<f64>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AxFocusProbe {
    /// 目标进程存在，但未暴露 `AXFocusedUIElement`（如 `kAXErrorNoValue`）。
    Missing { app_name: String },
    /// 探针不可用（未授权辅助功能、应用无响应/超时、PID 无效等）。
    Unavailable,
    /// 成功读到目标应用的当前焦点元素及其可编辑信号。
    Element(AxFocusElement),
}

/// 根据焦点元素的真实可编辑信号判定当前是否存在文本插入点。
///
/// 实测中，空白 Chrome 网页 (`AXWebArea`)、链接 (`AXLink`)、Qt 列表
/// (`Telegram AXList`)、达芬奇分屏 (`DaVinci Resolve AXSplitGroup`) 都会暴露
/// 只读的 `AXSelectedTextRange` 或 `AXInsertionPointLineNumber = 0`，但它们的
/// `value_settable` / `selected_text_settable` / `selected_text_range_settable`
/// / `has_editable_ancestor` 全为 false。因此不能仅凭 range 存在就判为可插入。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn classify_ax_focus(probe: &AxFocusProbe) -> InsertionProbe {
    match probe {
        AxFocusProbe::Unavailable => InsertionProbe::Unknown,
        AxFocusProbe::Missing { app_name } => {
            if is_strict_native_ax_app(app_name) {
                eprintln!(
                    "[delivery] native AX app {app_name} has no focused element; treating as NotInsertable"
                );
                InsertionProbe::NotInsertable
            } else {
                // 微信 / Electron / Chrome PWA 等自绘或跨进程应用常不暴露
                // AXFocusedUIElement，按 Unknown 放行并交由剪贴板消费确认裁决。
                let known_unsupported = is_unsupported_ax_app(Some(app_name));
                eprintln!(
                    "[delivery] target app {app_name} returned no focused element (known unsupported AX app: {known_unsupported}); treating as Unknown"
                );
                InsertionProbe::Unknown
            }
        }
        AxFocusProbe::Element(el) => {
            let role = el.role.trim();
            let is_text_role = TEXT_ROLES.contains(&role);
            let is_directly_editable =
                el.value_settable || el.selected_text_settable || el.has_editable_ancestor;
            let is_non_text_role = DEFINITELY_NON_TEXT_ROLES.contains(&role);
            let is_non_editable_structure = NON_EDITABLE_STRUCTURE_ROLES.contains(&role);
            let is_generic_container = GENERIC_CONTAINER_ROLES.contains(&role) || role.is_empty();

            let result = if is_text_role || is_directly_editable {
                InsertionProbe::Insertable
            } else if is_non_text_role {
                InsertionProbe::NotInsertable
            } else if el.selected_text_range_settable {
                // 虽未报告可写值或 AXEditableAncestor，但选区范围可写（如特殊
                // 富文本或自定义编辑器）：交由剪贴板消费确认裁决，绝不误杀。
                InsertionProbe::Unknown
            } else if is_non_editable_structure
                || (is_generic_container && is_strict_native_ax_app(&el.app_name))
            {
                InsertionProbe::NotInsertable
            } else {
                // 第三方应用的通用容器（GPU 终端、远程桌面、自绘窗口）无法仅凭
                // 角色断定没有光标：放行 Unknown，由剪贴板消费确认最终裁决。
                InsertionProbe::Unknown
            };

            eprintln!(
                "[delivery] ax-probe app={} role={} subrole={} valSet={} selSet={} rangeSet={} editableAnc={} ins={:?} -> {:?}",
                el.app_name,
                role,
                el.subrole,
                el.value_settable,
                el.selected_text_settable,
                el.selected_text_range_settable,
                el.has_editable_ancestor,
                el.insertion_point_line,
                result
            );
            result
        }
    }
}

/// Probe the target app's focused UI element directly via macOS native
/// `AXUIElement` APIs (no `osascript` subprocess, no Automation permission
/// required, bounded 500ms messaging timeout).
///
/// Prefer the remembered process PID so a development JackAICut is not
/// confused with the production install of the same product name.
pub fn probe_insertion_target(target: Option<&crate::overlay::FrontmostApp>) -> InsertionProbe {
    #[cfg(target_os = "macos")]
    {
        let Some(target) = target else {
            eprintln!("[delivery] no target app; treating as NotInsertable");
            return InsertionProbe::NotInsertable;
        };
        if target.pid <= 0 {
            eprintln!(
                "[delivery] invalid target pid={}; treating as NotInsertable",
                target.pid
            );
            return InsertionProbe::NotInsertable;
        }
        let focus = macos_ax::probe_application_focus(target.pid, &target.name);
        classify_ax_focus(&focus)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
        InsertionProbe::Unknown
    }
}

#[cfg(target_os = "macos")]
mod macos_ax {
    use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
    use core_foundation::number::{CFNumber, CFNumberRef};
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::ptr;

    use super::{AxFocusElement, AxFocusProbe};

    type AXUIElementRef = *mut c_void;
    type AXError = i32;

    const K_AX_ERROR_SUCCESS: AXError = 0;
    const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
    const K_AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;
    const K_AX_ERROR_API_DISABLED: AXError = -25211;
    const K_AX_ERROR_NO_VALUE: AXError = -25212;
    const AX_MESSAGING_TIMEOUT_SECS: f32 = 0.5;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementSetMessagingTimeout(
            element: AXUIElementRef,
            timeout_in_seconds: f32,
        ) -> AXError;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementIsAttributeSettable(
            element: AXUIElementRef,
            attribute: CFStringRef,
            settable: *mut u8,
        ) -> AXError;
    }

    struct OwnedCf(CFTypeRef);

    impl Drop for OwnedCf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn copy_attr(element: AXUIElementRef, name: &str) -> Result<OwnedCf, AXError> {
        let attr = CFString::new(name);
        let mut value: CFTypeRef = ptr::null();
        let err = unsafe {
            AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value)
        };
        if err != K_AX_ERROR_SUCCESS {
            return Err(err);
        }
        if value.is_null() {
            return Err(K_AX_ERROR_NO_VALUE);
        }
        Ok(OwnedCf(value))
    }

    fn copy_string_attr(element: AXUIElementRef, name: &str) -> Option<String> {
        let owned = copy_attr(element, name).ok()?;
        let is_string = unsafe { CFGetTypeID(owned.0) == CFString::type_id() };
        if !is_string {
            return None;
        }
        let s = unsafe { CFString::wrap_under_get_rule(owned.0 as CFStringRef) };
        Some(s.to_string())
    }

    fn copy_f64_attr(element: AXUIElementRef, name: &str) -> Option<f64> {
        let owned = copy_attr(element, name).ok()?;
        let is_number = unsafe { CFGetTypeID(owned.0) == CFNumber::type_id() };
        if !is_number {
            return None;
        }
        let n = unsafe { CFNumber::wrap_under_get_rule(owned.0 as CFNumberRef) };
        n.to_f64()
    }

    fn is_attr_settable(element: AXUIElementRef, name: &str) -> bool {
        let attr = CFString::new(name);
        let mut settable: u8 = 0;
        let err = unsafe {
            AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut settable)
        };
        err == K_AX_ERROR_SUCCESS && settable != 0
    }

    pub fn probe_application_focus(pid: i32, app_name: &str) -> AxFocusProbe {
        let app_ref = unsafe { AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            return AxFocusProbe::Unavailable;
        }
        let _app_guard = OwnedCf(app_ref as CFTypeRef);
        unsafe {
            let _ = AXUIElementSetMessagingTimeout(app_ref, AX_MESSAGING_TIMEOUT_SECS);
        }

        let focused = match copy_attr(app_ref, "AXFocusedUIElement") {
            Ok(el) => el,
            Err(
                K_AX_ERROR_NO_VALUE | K_AX_ERROR_ATTRIBUTE_UNSUPPORTED | K_AX_ERROR_NOT_IMPLEMENTED,
            ) => {
                return AxFocusProbe::Missing {
                    app_name: app_name.to_string(),
                };
            }
            Err(K_AX_ERROR_API_DISABLED) => {
                eprintln!("[delivery] AX API disabled; falling back to Unknown");
                return AxFocusProbe::Unavailable;
            }
            Err(code) => {
                eprintln!(
                    "[delivery] AXFocusedUIElement failed for pid={pid} ({app_name}) err={code}"
                );
                return AxFocusProbe::Unavailable;
            }
        };

        let el = focused.0 as AXUIElementRef;
        unsafe {
            let _ = AXUIElementSetMessagingTimeout(el, AX_MESSAGING_TIMEOUT_SECS);
        }

        let role = copy_string_attr(el, "AXRole").unwrap_or_default();
        let subrole = copy_string_attr(el, "AXSubrole").unwrap_or_default();
        let value_settable = is_attr_settable(el, "AXValue");
        let selected_text_settable = is_attr_settable(el, "AXSelectedText");
        let selected_text_range_settable = is_attr_settable(el, "AXSelectedTextRange");
        let has_editable_ancestor = copy_attr(el, "AXEditableAncestor").is_ok()
            || copy_attr(el, "AXHighestEditableAncestor").is_ok();
        let has_selected_text =
            copy_string_attr(el, "AXSelectedText").is_some_and(|text| !text.is_empty());
        let has_selected_text_range = copy_attr(el, "AXSelectedTextRange").is_ok();
        let insertion_point_line = copy_f64_attr(el, "AXInsertionPointLineNumber")
            .filter(|v| (0.0..1_000_000_000.0).contains(v));

        AxFocusProbe::Element(AxFocusElement {
            app_name: app_name.to_string(),
            role,
            subrole,
            value_settable,
            selected_text_settable,
            selected_text_range_settable,
            has_editable_ancestor,
            has_selected_text,
            has_selected_text_range,
            insertion_point_line,
        })
    }
}

/// Best-effort paste trigger used only inside a guarded clipboard transaction.
/// The caller restores the original clipboard unless newer user data has replaced it.
///
/// Runs on a blocking worker thread so the AppKit main thread stays completely
/// idle and can fulfill lazy `NSPasteboardItemDataProvider` callbacks immediately.
/// Core Graphics posting is synchronous and cannot hang; the osascript fallback
/// carries a bounded watchdog.
fn simulate_paste() -> bool {
    #[cfg(target_os = "macos")]
    {
        if post_paste_via_cgevent() {
            return true;
        }

        // Fallback: osascript can still succeed where direct posting is
        // rejected, but it must never be allowed to block the main thread.
        // A watchdog kills it instead of waiting forever.
        use std::process::{Command, Stdio};
        let mut child = match Command::new("osascript")
            .args([
                "-e",
                r#"tell application "System Events" to keystroke "v" using command down"#,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        eprintln!("[delivery] osascript 粘贴触发超时，已终止");
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
                Err(_) => return false,
            }
        }
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

/// 剪贴板事务建立失败时的兜底：只把听写文字写进去，不尝试保留原内容。
#[cfg(target_os = "macos")]
fn write_temporary_text_ignoring_previous<R: Runtime>(
    _app: &AppHandle<R>,
    text: &str,
) -> Result<(), String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();
    let string = NSString::from_str(text);
    pasteboard.clearContents();
    if pasteboard.setString_forType(&string, unsafe { NSPasteboardTypeString }) {
        Ok(())
    } else {
        Err("系统拒绝写入剪贴板".into())
    }
}

#[cfg(not(target_os = "macos"))]
fn write_temporary_text_ignoring_previous<R: Runtime>(
    app: &AppHandle<R>,
    text: &str,
) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard()
        .write_text(text.to_string())
        .map_err(|error| format!("无法写入剪贴板：{error}"))
}

/// Post Cmd+V directly through Core Graphics.
///
/// 用 `Private` 事件源：`HIDSystemState` 会把当前物理按键状态合并进事件，
/// 用户还按着 Option 时合成的就是 ⌥⌘V。Private 源只带我们显式设置的 ⌘。
#[cfg(target_os = "macos")]
fn post_paste_via_cgevent() -> bool {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, KeyCode};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let source = match CGEventSource::new(CGEventSourceStateID::Private) {
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
    // 少数应用（Electron / 远程桌面）会丢弃零间隔的按键对。
    std::thread::sleep(std::time::Duration::from_millis(12));
    key_up.post(CGEventTapLocation::HID);
    true
}

/// 停表那一刻定格投递目标。
///
/// 优先读**当前**前台：用户完全可能在录音中途换过应用，那才是他真正想
/// 写进去的地方。`remembered` 是胶囊弹出时记下的快照，听写全程不会更新，
/// 所以只能在前台读不出来时兜底（macOS 拿不到 frontmostApplication，或
/// 胶囊抢到了焦点）。用户在 JackVoice 自己的窗口里打字时，`current` 就是
/// JackVoice 本身，见 `overlay::current_input_target`。
///
/// 反过来只用 `remembered` 会退化成「粘回开始听写时那个应用」：在 Chrome
/// 开始说话、中途切到 Notes 再停表，文字会被拉回 Chrome——这正是
/// bbf1d46「修复跨应用停止听写时误插入旧窗口」修掉的场景。
///
/// 之所以要在这里定格而不是等投递时再读：收尾已经转入后台，投递可能晚
/// 几秒才发生，那时用户早就换窗口了，再读当前前台就会串台。
pub(crate) fn freeze_delivery_target(
    current: Option<crate::overlay::FrontmostApp>,
    remembered: Option<crate::overlay::FrontmostApp>,
) -> Option<crate::overlay::FrontmostApp> {
    current.or(remembered)
}

/// 投递前要不要把目标应用重新带回前台。
///
/// `target` 是录音停止那一刻定格的输入目标，投递可能要在几秒后才发生；
/// 目标本来就是前台时不能再激活一次，否则多窗口应用里带光标的窗口会被顶掉。
pub(crate) fn needs_reactivation(
    target: Option<&crate::overlay::FrontmostApp>,
    current: Option<&crate::overlay::FrontmostApp>,
) -> bool {
    target.is_some_and(|selected| current.is_none_or(|current| current.pid != selected.pid))
}

#[cfg(test)]
mod clipboard_transaction_tests {
    use super::{
        clipboard_is_still_temporary, freeze_delivery_target, is_unsupported_ax_app,
        needs_reactivation,
    };

    #[test]
    fn recognizes_unsupported_ax_apps() {
        assert!(is_unsupported_ax_app(Some("WeChat")));
        assert!(is_unsupported_ax_app(Some("wechat")));
        assert!(is_unsupported_ax_app(Some("微信")));
        assert!(is_unsupported_ax_app(Some("WXWork")));
        assert!(is_unsupported_ax_app(Some("企业微信")));
        assert!(is_unsupported_ax_app(Some("DingTalk")));
        assert!(is_unsupported_ax_app(Some("钉钉")));
        assert!(is_unsupported_ax_app(Some("QQ")));
        assert!(is_unsupported_ax_app(Some("JackDSH")));
        assert!(is_unsupported_ax_app(Some("jackdsh")));
        assert!(is_unsupported_ax_app(Some("Cursor")));
        assert!(is_unsupported_ax_app(Some("app_mode_loader")));
        assert!(is_unsupported_ax_app(Some("App_Mode_Loader")));
        assert!(is_unsupported_ax_app(Some("WorkBuddy")));
        assert!(is_unsupported_ax_app(Some("WorkBuddy AI")));
        assert!(is_unsupported_ax_app(Some("workbuddy")));
        assert!(is_unsupported_ax_app(Some("workbuddy ai")));
        assert!(is_unsupported_ax_app(Some("Electron")));
        assert!(is_unsupported_ax_app(Some("electron")));
        assert!(!is_unsupported_ax_app(Some("Google Chrome")));
        assert!(!is_unsupported_ax_app(Some("Safari")));
        assert!(!is_unsupported_ax_app(Some("Finder")));
        assert!(!is_unsupported_ax_app(Some("Notes")));
        assert!(!is_unsupported_ax_app(Some("Terminal")));
        assert!(!is_unsupported_ax_app(None));
        assert!(!is_unsupported_ax_app(Some("")));
    }

    #[test]
    fn probe_classifies_real_editable_controls_as_insertable() {
        use super::{classify_ax_focus, AxFocusElement, AxFocusProbe, InsertionProbe};

        // 原生文本控件：文本编辑 / 备忘录 / 终端（终端 valSet=false 但 role=AXTextArea）
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "文本编辑".into(),
                role: "AXTextArea".into(),
                value_settable: true,
                selected_text_settable: true,
                selected_text_range_settable: true,
                has_selected_text_range: true,
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "终端".into(),
                role: "AXTextArea".into(),
                selected_text_range_settable: true,
                has_selected_text_range: true,
                insertion_point_line: Some(8.0),
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Code".into(),
                role: "AXTextArea".into(),
                insertion_point_line: Some(0.0),
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );

        // Chrome / Electron 的 contenteditable（如 X 回复框、Notion、Slack）：
        // 即使 role 是 AXWebArea / AXGroup，只要带 AXEditableAncestor 或可写属性，
        // 就必须判为可插入。
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Google Chrome".into(),
                role: "AXWebArea".into(),
                has_editable_ancestor: true,
                has_selected_text_range: true,
                selected_text_range_settable: true,
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Google Chrome".into(),
                role: "AXGroup".into(),
                value_settable: true,
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Slack".into(),
                role: "AXGroup".into(),
                selected_text_settable: true,
                ..Default::default()
            })),
            InsertionProbe::Insertable
        );
    }

    #[test]
    fn probe_rejects_read_only_pages_lists_and_native_apps_without_caret() {
        use super::{classify_ax_focus, AxFocusElement, AxFocusProbe, InsertionProbe};

        // 确定不是输入位置的系统控件
        for (app_name, role) in [
            ("System Settings", "AXButton"),
            ("Finder", "AXMenuBarItem"),
            ("Finder", "AXStaticText"),
            ("ego lite", "AXLink"),
        ] {
            assert_eq!(
                classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                    app_name: app_name.into(),
                    role: role.into(),
                    // 实测 Chromium 链接也会暴露只读 range，绝不能因此误判可插入
                    has_selected_text_range: true,
                    ..Default::default()
                })),
                InsertionProbe::NotInsertable,
                "{app_name} {role} 不应被判为可插入"
            );
        }

        // Chrome / WKWebView 空白网页（实测带只读 range，但无任何可写/可编辑属性）
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Google Chrome".into(),
                role: "AXWebArea".into(),
                has_selected_text_range: true,
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "JackAICut".into(),
                role: "AXWebArea".into(),
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );

        // Qt 应用（Telegram 列表 / DaVinci Resolve 分屏）实测默认暴露只读 ins=0 和 range
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Telegram".into(),
                role: "AXList".into(),
                insertion_point_line: Some(0.0),
                has_selected_text_range: true,
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "DaVinci Resolve".into(),
                role: "AXSplitGroup".into(),
                insertion_point_line: Some(0.0),
                has_selected_text_range: true,
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );

        // IDE 文件树 / Finder 列表
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Code".into(),
                role: "AXOutline".into(),
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "访达".into(),
                role: "AXOutline".into(),
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );

        // 苹果原生 AppKit 应用无光标（桌面 / 窗口背景 / 照片滚动区 / 预览无焦点）
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Missing {
                app_name: "Finder".into(),
            }),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Missing {
                app_name: "访达".into(),
            }),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Missing {
                app_name: "预览".into(),
            }),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "备忘录".into(),
                role: "AXWindow".into(),
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "照片".into(),
                role: "AXScrollArea".into(),
                ..Default::default()
            })),
            InsertionProbe::NotInsertable
        );

        // 没有目标应用或无效 PID 时直接判定不可插入
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                super::probe_insertion_target(None),
                InsertionProbe::NotInsertable
            );
            assert_eq!(
                super::probe_insertion_target(Some(&app(0, "Finder"))),
                InsertionProbe::NotInsertable
            );
        }
    }

    #[test]
    fn non_native_missing_and_generic_containers_defer_to_paste_consumption() {
        use super::{classify_ax_focus, AxFocusElement, AxFocusProbe, InsertionProbe};

        // 微信 / Electron / Chrome PWA 拿不到焦点元素时必须放行 Unknown，
        // 再由延迟剪贴板消费确认精确区分「有光标」和「没光标」。
        for app_name in [
            "WeChat",
            "微信",
            "Cursor",
            "ChatGPT",
            "Feishu",
            "Obsidian",
            "app_mode_loader",
            "Firefox",
            "Some App Nobody Has Heard Of",
        ] {
            assert_eq!(
                classify_ax_focus(&AxFocusProbe::Missing {
                    app_name: app_name.into(),
                }),
                InsertionProbe::Unknown,
                "{app_name} 应放行至剪贴板消费确认"
            );
        }

        // GPU 终端 / 远程桌面 / 第三方自绘容器同样放行 Unknown
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Ghostty".into(),
                role: "AXWindow".into(),
                ..Default::default()
            })),
            InsertionProbe::Unknown
        );

        // 任何支持可写 AXSelectedTextRange 的自定义编辑器也不应被误杀
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Element(AxFocusElement {
                app_name: "Google Chrome".into(),
                role: "AXWebArea".into(),
                selected_text_range_settable: true,
                has_selected_text_range: true,
                ..Default::default()
            })),
            InsertionProbe::Unknown
        );

        // 探针超时或未授权时退回 Unknown
        assert_eq!(
            classify_ax_focus(&AxFocusProbe::Unavailable),
            InsertionProbe::Unknown
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn paste_consumption_wait_returns_early_on_read_and_false_on_timeout() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        // 无人消费：超时返回 false
        let never_consumed = Arc::new(AtomicBool::new(false));
        let ok = super::wait_for_paste_consumption(
            &never_consumed,
            Duration::from_millis(60),
            Duration::from_millis(20),
        )
        .await;
        assert!(!ok);

        // 很快消费：提前返回 true，不等满整个 timeout
        let consumed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&consumed);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            flag.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let ok = super::wait_for_paste_consumption(
            &consumed,
            Duration::from_millis(800),
            Duration::from_millis(20),
        )
        .await;
        assert!(ok);
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "消费后应提前结束等待，实际耗时 {:?}",
            started.elapsed()
        );
    }

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

    fn app(pid: i32, name: &str) -> crate::overlay::FrontmostApp {
        crate::overlay::FrontmostApp {
            pid,
            name: name.into(),
        }
    }

    #[test]
    fn already_frontmost_target_is_not_reactivated() {
        let target = app(1, "Codex");
        assert!(!needs_reactivation(Some(&target), Some(&target)));
    }

    #[test]
    fn target_is_reactivated_only_after_the_user_moved_away() {
        let target = app(1, "Codex");
        let elsewhere = app(2, "Chrome");
        assert!(needs_reactivation(Some(&target), Some(&elsewhere)));
        assert!(needs_reactivation(Some(&target), None));
        assert!(!needs_reactivation(None, Some(&elsewhere)));
    }

    /// 停表时用户在哪个应用，文字就该落进哪个应用。
    ///
    /// 这是 bbf1d46 修过的场景，这次收尾转后台时必须继续保持：在 Chrome
    /// 按快捷键开始说话，中途切到 Notes 再停表，目标必须是 Notes。
    #[test]
    fn app_switch_mid_dictation_delivers_to_the_app_frontmost_at_stop() {
        let started_in = Some(app(1, "Chrome"));
        let stopped_in = app(2, "Notes");
        assert_eq!(
            freeze_delivery_target(Some(stopped_in), started_in),
            Some(app(2, "Notes")),
            "中途换了应用时，停表那一刻的前台才是投递目标"
        );
    }

    /// 读不到当前前台时才退回胶囊弹出时的快照。
    #[test]
    fn the_capsule_snapshot_is_only_a_fallback() {
        let started_in = Some(app(1, "Chrome"));
        assert_eq!(freeze_delivery_target(None, started_in.clone()), started_in);
        assert_eq!(freeze_delivery_target(None, None), None);
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
    fn temporary_text_tracks_consumption_and_restores_original_snapshot() {
        use super::{
            capture_macos_clipboard, macos_clipboard_has_temporary_marker, write_macos_snapshot,
            write_temporary_macos_text, ClipboardRepresentation, ClipboardSnapshot,
        };
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let original = ClipboardSnapshot {
            items: vec![vec![ClipboardRepresentation {
                type_name: "public.png".into(),
                data: vec![1, 2, 3, 4, 5],
            }]],
        };
        write_macos_snapshot(&pasteboard, &original).expect("seed image clipboard");

        // 场景 1：写入并检查 Transient 标记、直接恢复原剪贴板，不应误报已消费
        let original_change_count = pasteboard.changeCount();
        let unconsumed = Arc::new(AtomicBool::new(false));
        let provider = write_temporary_macos_text(
            &pasteboard,
            "临时听写",
            original_change_count,
            Arc::clone(&unconsumed),
        )
        .expect("write temporary text");
        assert!(macos_clipboard_has_temporary_marker(&pasteboard));
        assert!(
            !unconsumed.load(Ordering::Acquire),
            "仅写入和检查标记不应触发 consumed"
        );

        write_macos_snapshot(&pasteboard, &original).expect("restore image clipboard");
        drop(provider);
        assert!(
            !unconsumed.load(Ordering::Acquire),
            "无人粘贴直接恢复原剪贴板后 consumed 必须保持为 false"
        );
        let restored = capture_macos_clipboard(&pasteboard).expect("read restored clipboard");
        assert_snapshot_preserves_original(&restored, &original);

        // 场景 2：真实读取字符串时，返回完整文本并置 consumed = true
        let change_count = pasteboard.changeCount();
        let consumed = Arc::new(AtomicBool::new(false));
        let provider = write_temporary_macos_text(
            &pasteboard,
            "你好 JackVoice",
            change_count,
            Arc::clone(&consumed),
        )
        .expect("write temporary text for read");
        let read_back = pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|s| s.to_string());
        assert_eq!(read_back.as_deref(), Some("你好 JackVoice"));
        assert!(consumed.load(Ordering::Acquire));
        write_macos_snapshot(&pasteboard, &original).expect("restore clipboard again");
        drop(provider);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn temporary_write_refuses_to_overwrite_a_newer_clipboard() {
        use super::{
            capture_macos_clipboard, write_macos_snapshot, write_temporary_macos_text,
            ClipboardRepresentation, ClipboardSnapshot,
        };
        use objc2_app_kit::NSPasteboard;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

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

        let error = write_temporary_macos_text(
            &pasteboard,
            "不得覆盖",
            stale_change_count,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("stale transaction must be rejected");
        assert!(!error.live_clipboard_changed);
        let current = capture_macos_clipboard(&pasteboard).expect("read newer clipboard");
        assert_snapshot_preserves_original(&current, &newer);
    }
}
