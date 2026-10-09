use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

const MEMOS_FILE: &str = "memos.json";
pub const MEMOS_CHANGED_EVENT: &str = "jackvoice://memos-changed";

/// 条目来源。听写置顶、面板手打、剪贴板收入各记各的：
/// 界面要能区分「我说的」和「我抄来的」，Agent 读待办时也需要这个信息。
pub const SOURCE_VOICE: &str = "voice";
pub const SOURCE_MANUAL: &str = "manual";
pub const SOURCE_CLIPBOARD: &str = "clipboard";

/// 单条待办最长能有多长。剪贴板里偶尔会是一次误操作粘来的整页内容，
/// 与其静默截断（那样用户看到的内容和复制的不一样），不如明确拒绝。
pub const MAX_MEMO_CHARS: usize = 100_000;

fn default_memo_source() -> String {
    SOURCE_VOICE.into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoItem {
    pub id: String,
    pub text: String,
    pub created_at_ms: i64,
    pub completed: bool,
    /// voice / manual / clipboard。旧版本没有这个字段，一律按语音收入处理。
    #[serde(default = "default_memo_source")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MemosData {
    pub memos: Vec<MemoItem>,
}

fn memos_path(dir: &Path) -> PathBuf {
    dir.join(MEMOS_FILE)
}

/// 未知来源一律落到 voice，磁盘上的历史数据不必迁移。
pub fn normalize_source(source: &str) -> String {
    match source.trim() {
        SOURCE_MANUAL => SOURCE_MANUAL.into(),
        SOURCE_CLIPBOARD => SOURCE_CLIPBOARD.into(),
        _ => SOURCE_VOICE.into(),
    }
}

pub fn load_memos(dir: &Path) -> Vec<MemoItem> {
    let path = memos_path(dir);
    if !path.exists() {
        return Vec::new();
    }
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<MemosData>(&content)
            .map(|d| d.memos)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_memos(dir: &Path, memos: &[MemoItem]) -> Result<(), String> {
    let path = memos_path(dir);
    let data = MemosData {
        memos: memos.to_vec(),
    };
    let json =
        serde_json::to_string_pretty(&data).map_err(|e| format!("序列化待办事项失败：{e}"))?;
    // 原子写：快捷键、托盘菜单和面板可能同时改待办，半个文件被读到就会丢数据。
    crate::storage::write_atomic(&path, json.as_bytes(), false)
}

/// 待办是读-改-写，多个入口（听写置顶 / 面板 / 剪贴板 / 快捷键）会撞车，
/// 这里把所有写入串起来。读操作也跟着排队，免得读到写了一半的文件。
static MEMOS_LOCK: Mutex<()> = Mutex::new(());

fn with_memos_lock<T>(action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _guard = MEMOS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action()
}

fn refresh_tray_badge(app: &AppHandle, memos: &[MemoItem]) {
    let count = memos.iter().filter(|m| !m.completed).count();
    crate::tray::update_tray_badge(app, count);
}

/// 校验并规范化待办文本。只去首尾空白，**内部换行原样保留**。
///
/// 不按行拆分：从网页上复制一段话，段落之间天然带换行，拆开会把一条完整的
/// 内容切成好几条待办，和用户看到的复制内容对不上。超长也不截断——宁可不收，
/// 也不要让待办里躺着一段和原文不一样的东西。
fn normalize_memo_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("待办内容不能为空".into());
    }
    if text.chars().count() > MAX_MEMO_CHARS {
        return Err(format!(
            "这段文字太长（超过 {MAX_MEMO_CHARS} 字），没有收入待办。"
        ));
    }
    Ok(text.to_string())
}

/// 收入一条待办。来源用于区分听写置顶、面板手打和剪贴板收入。
pub fn add_memo(app: &AppHandle, text: &str, source: &str) -> Result<MemoItem, String> {
    let text = normalize_memo_text(text)?;

    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let item = MemoItem {
        id: Uuid::new_v4().to_string(),
        text,
        created_at_ms: chrono::Utc::now().timestamp_millis(),
        completed: false,
        source: normalize_source(source),
    };

    // 新条目一律排在最前。
    let merged = with_memos_lock(|| {
        let mut memos = vec![item.clone()];
        memos.extend(load_memos(&dir));
        save_memos(&dir, &memos)?;
        Ok(memos)
    })?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &merged);
    refresh_tray_badge(app, &merged);
    Ok(item)
}

pub fn toggle_memo(app: &AppHandle, id: &str) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let memos = with_memos_lock(|| {
        let mut memos = load_memos(&dir);
        if let Some(memo) = memos.iter_mut().find(|m| m.id == id) {
            memo.completed = !memo.completed;
        }
        save_memos(&dir, &memos)?;
        Ok(memos)
    })?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn delete_memo(app: &AppHandle, id: &str) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let memos = with_memos_lock(|| {
        let mut memos = load_memos(&dir);
        memos.retain(|m| m.id != id);
        save_memos(&dir, &memos)?;
        Ok(memos)
    })?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn clear_completed_memos(app: &AppHandle) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let memos = with_memos_lock(|| {
        let mut memos = load_memos(&dir);
        memos.retain(|m| !m.completed);
        save_memos(&dir, &memos)?;
        Ok(memos)
    })?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn list_memos(app: &AppHandle) -> Vec<MemoItem> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    with_memos_lock(|| Ok(load_memos(&dir))).unwrap_or_default()
}

pub fn uncompleted_count(app: &AppHandle) -> usize {
    list_memos(app).into_iter().filter(|m| !m.completed).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jackvoice-memos-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn test_save_and_load_memos() {
        let dir = temp_dir();
        let path = dir.as_path();

        let initial = load_memos(path);
        assert!(initial.is_empty());

        let memo1 = MemoItem {
            id: "m-1".into(),
            text: "第一条待办".into(),
            created_at_ms: 1000,
            completed: false,
            source: SOURCE_VOICE.into(),
        };
        let memo2 = MemoItem {
            id: "m-2".into(),
            text: "第二条待办".into(),
            created_at_ms: 2000,
            completed: true,
            source: SOURCE_MANUAL.into(),
        };

        save_memos(path, &[memo1.clone(), memo2.clone()]).unwrap();

        let loaded = load_memos(path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, "第一条待办");
        assert!(loaded[1].completed);
        assert_eq!(loaded[1].source, SOURCE_MANUAL);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧版 memos.json 没有 source 字段，读出来必须是 voice，不需要数据迁移。
    #[test]
    fn legacy_memos_without_source_load_as_voice() {
        let dir = temp_dir();
        fs::write(
            dir.join(MEMOS_FILE),
            r#"{"memos":[{"id":"m-1","text":"旧待办","createdAtMs":1000,"completed":false}]}"#,
        )
        .unwrap();

        let loaded = load_memos(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].source, SOURCE_VOICE);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_source_falls_back_to_voice() {
        assert_eq!(normalize_source("manual"), SOURCE_MANUAL);
        assert_eq!(normalize_source(" clipboard "), SOURCE_CLIPBOARD);
        assert_eq!(normalize_source("voice"), SOURCE_VOICE);
        assert_eq!(normalize_source("别的什么"), SOURCE_VOICE);
        assert_eq!(normalize_source(""), SOURCE_VOICE);
    }

    /// 整段文本原样收下，绝不按行拆开——从网页复制一段话，
    /// 段落之间天然带换行，拆开会把一条完整的内容切成好几条待办。
    #[test]
    fn multi_line_text_stays_one_memo() {
        let text = "第一句。\n第二句。\n\n第三句。";
        let normalized = normalize_memo_text(text).unwrap();
        assert_eq!(normalized, text);
        assert!(normalized.contains('\n'));
        // 只去首尾空白，中间的结构一个字都不动。
        assert_eq!(normalize_memo_text("  两边有空格  ").unwrap(), "两边有空格");
    }

    /// 长文不截断；只有超过上限才整条拒绝，避免用户看到的内容和复制的不一致。
    #[test]
    fn overlong_text_is_rejected_instead_of_truncated() {
        let long = "字".repeat(MAX_MEMO_CHARS + 1);
        let error = normalize_memo_text(&long).unwrap_err();
        assert!(error.contains("太长"), "错误信息应说明原因：{error}");

        // 正好卡在上限的长文必须原样通过，一个字都不能少。
        let at_limit = "字".repeat(MAX_MEMO_CHARS);
        assert_eq!(
            normalize_memo_text(&at_limit).unwrap().chars().count(),
            MAX_MEMO_CHARS
        );
    }

    #[test]
    fn blank_text_is_rejected() {
        assert!(normalize_memo_text("   \n\t ").is_err());
    }

    /// source 只可能是三个已知值之一，别人传什么都落回 voice。
    #[test]
    fn serialized_source_is_one_of_three_known_values() {
        let dir = temp_dir();
        let item = MemoItem {
            id: "m-1".into(),
            text: "内容".into(),
            created_at_ms: 1000,
            completed: false,
            source: normalize_source("clipboard"),
        };
        save_memos(&dir, &[item]).unwrap();
        let raw = fs::read_to_string(dir.join(MEMOS_FILE)).unwrap();
        assert!(raw.contains("\"source\": \"clipboard\""));
        let _ = fs::remove_dir_all(&dir);
    }
}
