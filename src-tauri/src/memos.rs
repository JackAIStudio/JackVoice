use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

const MEMOS_FILE: &str = "memos.json";
pub const MEMOS_CHANGED_EVENT: &str = "jackvoice://memos-changed";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoItem {
    pub id: String,
    pub text: String,
    pub created_at_ms: i64,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MemosData {
    pub memos: Vec<MemoItem>,
}

fn memos_path(dir: &Path) -> PathBuf {
    dir.join(MEMOS_FILE)
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
    let json = serde_json::to_string_pretty(&data)
        .map_err(|e| format!("序列化待办事项失败：{e}"))?;
    fs::write(&path, json).map_err(|e| format!("保存待办事项失败：{e}"))?;
    Ok(())
}

fn refresh_tray_badge(app: &AppHandle, memos: &[MemoItem]) {
    let count = memos.iter().filter(|m| !m.completed).count();
    crate::tray::update_tray_badge(app, count);
}

pub fn add_memo(app: &AppHandle, text: &str) -> Result<MemoItem, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("待办内容不能为空".into());
    }
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let mut memos = load_memos(&dir);
    let item = MemoItem {
        id: Uuid::new_v4().to_string(),
        text: text.to_string(),
        created_at_ms: chrono::Utc::now().timestamp_millis(),
        completed: false,
    };
    memos.insert(0, item.clone());
    save_memos(&dir, &memos)?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(item)
}

pub fn toggle_memo(app: &AppHandle, id: &str) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let mut memos = load_memos(&dir);
    if let Some(memo) = memos.iter_mut().find(|m| m.id == id) {
        memo.completed = !memo.completed;
    }
    save_memos(&dir, &memos)?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn delete_memo(app: &AppHandle, id: &str) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let mut memos = load_memos(&dir);
    memos.retain(|m| m.id != id);
    save_memos(&dir, &memos)?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn clear_completed_memos(app: &AppHandle) -> Result<Vec<MemoItem>, String> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    let mut memos = load_memos(&dir);
    memos.retain(|m| !m.completed);
    save_memos(&dir, &memos)?;
    let _ = app.emit(MEMOS_CHANGED_EVENT, &memos);
    refresh_tray_badge(app, &memos);
    Ok(memos)
}

pub fn list_memos(app: &AppHandle) -> Vec<MemoItem> {
    let state = app.state::<crate::session::AppState>();
    let dir = state.data_dir();
    load_memos(&dir)
}

pub fn uncompleted_count(app: &AppHandle) -> usize {
    list_memos(app).into_iter().filter(|m| !m.completed).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jackvoice-memos-test-{}", uuid::Uuid::new_v4()));
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
        };
        let memo2 = MemoItem {
            id: "m-2".into(),
            text: "第二条待办".into(),
            created_at_ms: 2000,
            completed: true,
        };

        save_memos(path, &[memo1.clone(), memo2.clone()]).unwrap();

        let loaded = load_memos(path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, "第一条待办");
        assert_eq!(loaded[1].completed, true);

        let _ = fs::remove_dir_all(&dir);
    }
}
