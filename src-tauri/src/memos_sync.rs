//! 从 TOS 拉手机记的待办，并进本地 `memos.json`。
//!
//! ## 和词库同步的关系
//!
//! 桶和凭据是同一套（`jackvoice-glossary` 桶 + `tos-credentials.json`），
//! 但**对象和合并规则完全不同**，所以这里是独立模块，不挂在 [crate::tos_sync] 里：
//!
//! | | 词库 | 待办 |
//! |---|---|---|
//! | 对象 | `glossary.json` | `memos.json` |
//! | 谁能改 | 两端都能改 | 手机只新建，Mac 全权 |
//! | 合并 | 三方合并（base/local/remote） | 按 id 并集 |
//!
//! ## 为什么是「并集」而不是「覆盖」
//!
//! 手机推上来的是**它那一侧的全部待办**。如果 Mac 直接拿它覆盖本地，
//! 桌面上记的待办会被清空；反过来如果 Mac 只读不写，手机上又永远看不到
//! 桌面记的那些。所以两边都做并集：
//!
//! - 云端有、本地没有 → 追加（这是手机在外面记的）
//! - 本地有、云端没有 → 推上去（这是桌面记的）
//! - 两边都有 → **本地优先**。勾完成、删除都发生在 Mac，
//!   本地那份才是权威；拿云端的覆盖回来，会出现「勾了又活过来」。
//!
//! ## 删除要留墓碑
//!
//! 光「本地优先」不够：在 Mac 上删掉一条手机来的待办，下次同步云端那份
//! 又会被当成「云端有、本地没有」追加回来。所以删掉的 id 要记进
//! `memos-sync-state.json` 的 `deleted_ids`，合并时跳过。
//! 撤销删除（前端有 undo）时把 id 从墓碑里摘掉。

use crate::memos::{self, MemoItem};
use crate::storage;
use crate::tos_sync;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use ve_tos_rust_sdk::asynchronous::object::{ObjectAPI, ObjectContent};
use ve_tos_rust_sdk::asynchronous::tos;
use ve_tos_rust_sdk::asynchronous::tos::TosClient;
use ve_tos_rust_sdk::object::{GetObjectInput, PutObjectFromBufferInput};

/// 与 Android 端 `VoiceMemosSyncClient.KEY` 一致。
const KEY: &str = "memos.json";
/// 与 Android 端 `VoiceMemosSyncClient.FORMAT` 一致。
const FORMAT: &str = "jackvoice-memos-v1";
const STATE_FILE: &str = "memos-sync-state.json";
/// 重试次数：每次冲突都重拉一遍再推。
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoSyncState {
    /// 在 Mac 上删掉的 id。云端再出现时跳过，避免「删了又回来」。
    #[serde(default)]
    deleted_ids: Vec<String>,
    #[serde(default)]
    last_synced_at: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoSyncReport {
    /// 从云端并进来的条数（手机记的）。
    pub pulled: usize,
    /// 推上去的条数（桌面记的）。
    pub pushed: usize,
    /// 因为墓碑被跳过的条数。
    pub skipped: usize,
    pub synced_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoSyncStatus {
    pub has_credentials: bool,
    pub key: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub deleted_count: usize,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

fn load_state(dir: &Path) -> MemoSyncState {
    let path = state_path(dir);
    if !path.is_file() {
        return MemoSyncState::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(dir: &Path, state: &MemoSyncState) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("序列化待办同步状态失败：{error}"))?;
    storage::write_atomic(&state_path(dir), &raw, true)
}

fn fail_state(dir: &Path, message: String) -> String {
    let mut state = load_state(dir);
    state.last_error = Some(message.clone());
    let _ = save_state(dir, &state);
    message
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn status(dir: &Path) -> MemoSyncStatus {
    let state = load_state(dir);
    let has_credentials = tos_sync::load_credentials(dir)
        .ok()
        .flatten()
        .is_some();
    MemoSyncStatus {
        has_credentials,
        key: KEY.to_string(),
        last_synced_at: state.last_synced_at,
        last_error: state.last_error,
        deleted_count: state.deleted_ids.len(),
    }
}

/// 记一条墓碑：这个 id 在 Mac 上被删了，云端再出现也不要。
///
/// 由 `delete_memo` 调用。写失败只是让墓碑丢一次，不影响删除本身。
pub fn remember_deleted(dir: &Path, id: &str) {
    let mut state = load_state(dir);
    if state.deleted_ids.iter().any(|existing| existing == id) {
        return;
    }
    state.deleted_ids.push(id.to_string());
    let _ = save_state(dir, &state);
}

/// 撤销删除：把墓碑摘掉，否则云端那条永远回不来。
pub fn forget_deleted(dir: &Path, id: &str) {
    let mut state = load_state(dir);
    let before = state.deleted_ids.len();
    state.deleted_ids.retain(|existing| existing != id);
    if state.deleted_ids.len() != before {
        let _ = save_state(dir, &state);
    }
}

fn client(credentials: &tos_sync::TosCredentials) -> Result<impl TosClient, String> {
    tos::builder::<tos_sync::TokioRuntime>()
        .connection_timeout(3_000)
        .request_timeout(10_000)
        .max_retry_count(3)
        .ak(credentials.access_key_id.clone())
        .sk(credentials.secret_access_key.clone())
        .region(tos_sync::REGION)
        .endpoint(tos_sync::ENDPOINT)
        .build()
        .map_err(|error| format!("创建 TOS 客户端失败：{error}"))
}

async fn fetch_remote(
    credentials: &tos_sync::TosCredentials,
) -> Result<Option<(Vec<MemoItem>, String)>, String> {
    let client = client(credentials)?;
    match client
        .get_object(&GetObjectInput::new(tos_sync::BUCKET, KEY))
        .await
    {
        Ok(mut output) => {
            let etag = output.etag().to_string();
            let bytes = output
                .read_all()
                .await
                .map_err(|error| format!("读取 TOS 待办失败：{error}"))?;
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                // 云端文件坏了不该卡住本地：当成「还没有」，本轮只推不拉。
                Err(error) => {
                    eprintln!("[memos-sync] 云端 memos.json 不是合法 JSON，按空处理：{error}");
                    return Ok(None);
                }
            };
            Ok(Some((parse_remote(&value), etag)))
        }
        Err(error) => {
            if error
                .as_server_error()
                .map(|server| server.status_code() == 404)
                .unwrap_or(false)
            {
                return Ok(None);
            }
            Err(format!("从 TOS 拉取待办失败：{error}"))
        }
    }
}

fn parse_remote(value: &Value) -> Vec<MemoItem> {
    value
        .get("memos")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.trim();
                    let text = item.get("text").and_then(Value::as_str)?.trim();
                    if id.is_empty() || text.is_empty() {
                        return None;
                    }
                    Some(MemoItem {
                        id: id.to_string(),
                        text: text.to_string(),
                        created_at_ms: item
                            .get("createdAtMs")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        completed: item
                            .get("completed")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        source: memos::normalize_source(
                            item.get("source").and_then(Value::as_str).unwrap_or(""),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn encode_remote(items: &[MemoItem]) -> Value {
    json!({
        "format": FORMAT,
        "version": 1,
        "updatedAt": now_rfc3339(),
        "updatedBy": "macOS",
        "memos": items.iter().map(|item| json!({
            "id": item.id,
            "text": item.text,
            "createdAtMs": item.created_at_ms,
            "completed": item.completed,
            "source": item.source,
        })).collect::<Vec<_>>(),
    })
}

enum PushError {
    Conflict,
    Other(String),
}

async fn push_remote(
    credentials: &tos_sync::TosCredentials,
    items: &[MemoItem],
    etag: Option<&str>,
) -> Result<(), PushError> {
    let client = client(credentials).map_err(PushError::Other)?;
    let body = encode_remote(items);
    let content = serde_json::to_vec_pretty(&body)
        .map_err(|error| PushError::Other(format!("序列化待办失败：{error}")))?;
    let mut input = PutObjectFromBufferInput::new_with_content(tos_sync::BUCKET, KEY, &content);
    if let Some(etag) = etag {
        input.set_if_match(etag);
    } else {
        input.set_if_none_match("*");
    }
    match client.put_object_from_buffer(&input).await {
        Ok(_) => Ok(()),
        Err(error) => {
            if error
                .as_server_error()
                .map(|server| server.status_code() == 412 || server.status_code() == 409)
                .unwrap_or(false)
            {
                return Err(PushError::Conflict);
            }
            Err(PushError::Other(format!("推送待办到 TOS 失败：{error}")))
        }
    }
}

/// 合并：本地优先，云端补新，墓碑过滤。
fn merge(
    local: &[MemoItem],
    remote: &[MemoItem],
    deleted: &HashSet<&String>,
) -> (Vec<MemoItem>, usize) {
    let mut merged: Vec<MemoItem> = local.to_vec();
    let known: HashSet<String> = local.iter().map(|item| item.id.clone()).collect();
    let mut skipped = 0usize;
    for item in remote {
        if deleted.contains(&item.id) {
            skipped += 1;
            continue;
        }
        if known.contains(&item.id) {
            // 两边都有：本地是权威，不覆盖。
            continue;
        }
        merged.push(item.clone());
    }
    merged.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    (merged, skipped)
}

/// 拉一次、并进本地、必要时把桌面的推上去。
///
/// 顺序：**先把云端并进本地，再决定要不要推**。推的内容是合并后的并集，
/// 这样手机在另一头看到的也是完整列表。
pub async fn sync(dir: &Path) -> Result<MemoSyncReport, String> {
    sync_inner(dir).await.map_err(|error| fail_state(dir, error))
}

async fn sync_inner(dir: &Path) -> Result<MemoSyncReport, String> {
    let credentials = tos_sync::load_credentials(dir)?
        .ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let mut last_conflict = None;

    for _ in 0..MAX_ATTEMPTS {
        let state = load_state(dir);
        let deleted: HashSet<&String> = state.deleted_ids.iter().collect();
        let local = memos::load_memos(dir);
        let remote = fetch_remote(&credentials).await?;
        let (merged, skipped) = merge(
            &local,
            remote.as_ref().map(|r| r.0.as_slice()).unwrap_or(&[]),
            &deleted,
        );

        // 云端有没有本地缺的：有就写进本地。
        let local_ids: HashSet<&String> = local.iter().map(|item| &item.id).collect();
        let adopted = merged.iter().filter(|item| !local_ids.contains(&item.id)).count();
        if adopted > 0 {
            memos::save_memos(dir, &merged)?;
        }

        // 本地有没有云端缺的：有就推。删除墓碑过滤掉的条目**不算缺**，
        // 否则每轮都会因为它们而反复推送。
        let remote_ids: HashSet<&String> = remote
            .as_ref()
            .map(|r| r.0.iter().map(|item| &item.id).collect())
            .unwrap_or_default();
        let missing: Vec<&MemoItem> = local
            .iter()
            .filter(|item| !remote_ids.contains(&item.id) && !deleted.contains(&item.id))
            .collect();
        let needs_push = !missing.is_empty() || remote.is_none();

        if !needs_push {
            let mut next = load_state(dir);
            next.last_synced_at = Some(now_rfc3339());
            next.last_error = None;
            save_state(dir, &next)?;
            return Ok(MemoSyncReport {
                pulled: adopted,
                pushed: 0,
                skipped,
                synced_at: now_rfc3339(),
            });
        }

        let push_items: Vec<MemoItem> = merged
            .iter()
            .filter(|item| !deleted.contains(&item.id))
            .cloned()
            .collect();
        match push_remote(&credentials, &push_items, remote.as_ref().map(|r| r.1.as_str())).await {
            Ok(()) => {
                let mut next = load_state(dir);
                next.last_synced_at = Some(now_rfc3339());
                next.last_error = None;
                save_state(dir, &next)?;
                return Ok(MemoSyncReport {
                    pulled: adopted,
                    pushed: missing.len(),
                    skipped,
                    synced_at: now_rfc3339(),
                });
            }
            Err(PushError::Conflict) => {
                last_conflict = Some("云端刚被另一台设备更新，正在重试。".to_string());
                continue;
            }
            Err(PushError::Other(error)) => return Err(error),
        }
    }

    Err(last_conflict.unwrap_or_else(|| "推送重试次数过多。".to_string()))
}

#[cfg(test)]
mod live {
    //! 真连 TOS 的验收测试。
    //!
    //! 默认不跑（`#[ignore]`）：它要真的读用户目录里的凭据、真的打网络，
    //! 在 CI 或别人机器上只会失败得很莫名其妙。本机要验收时手动跑：
    //!
    //! ```sh
    //! cargo test --manifest-path src-tauri/Cargo.toml -- --ignored memos_live
    //! ```
    //!
    //! 它验证的是「手机推上去的东西 Mac 能不能拿到」这条端到端链路——
    //! 单元测试只能证明合并函数是对的，证明不了对象名、格式、桶都对得上。

    use super::*;

    fn shared_dir() -> std::path::PathBuf {
        std::env::var_os("HOME")
            .map(|home| {
                std::path::PathBuf::from(home)
                    .join("Library/Application Support/com.jackvoice.shared")
            })
            .expect("需要 HOME")
    }

    #[tokio::test]
    #[ignore = "要真连 TOS，手动跑"]
    async fn memos_live_roundtrip() {
        let dir = shared_dir();
        assert!(
            dir.is_dir(),
            "找不到 JackVoice 共享数据目录：{}",
            dir.display()
        );
        let report = sync(&dir).await.expect("同步失败");
        println!(
            "拉回 {} 条，推上去 {} 条，跳过 {} 条，时间 {}",
            report.pulled, report.pushed, report.skipped, report.synced_at
        );
        let memos = memos::load_memos(&dir);
        let phone: Vec<_> = memos
            .iter()
            .filter(|m| m.source == memos::SOURCE_PHONE)
            .collect();
        println!("本地共 {} 条待办，其中手机来源 {} 条", memos.len(), phone.len());
        for m in phone.iter().take(5) {
            println!("  [phone] {} · {}", m.id, m.text.chars().take(24).collect::<String>());
        }
        assert!(
            !phone.is_empty(),
            "同步成功但一条手机来源的待办都没有——检查 TOS 里的 memos.json 是不是空的"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memo(id: &str, text: &str, at: i64) -> MemoItem {
        MemoItem {
            id: id.to_string(),
            text: text.to_string(),
            created_at_ms: at,
            completed: false,
            source: memos::SOURCE_PHONE.to_string(),
        }
    }

    #[test]
    fn merge_keeps_local_state_for_known_ids() {
        let mut local = memo("a", "本地勾过的", 10);
        local.completed = true;
        let remote = vec![memo("a", "云端还没勾", 10)];
        let deleted = HashSet::new();
        let (merged, skipped) = merge(&[local.clone()], &remote, &deleted);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].completed, "同 id 必须保留本地状态，否则勾了又活过来");
        assert_eq!(skipped, 0);
    }

    #[test]
    fn merge_adopts_remote_only_items() {
        let local = vec![memo("a", "桌面的", 10)];
        let remote = vec![memo("b", "手机记的", 20)];
        let (merged, _) = merge(&local, &remote, &HashSet::new());
        assert_eq!(merged.len(), 2);
        // 新的排前面
        assert_eq!(merged[0].id, "b");
    }

    #[test]
    fn merge_skips_deleted_ids() {
        let local: Vec<MemoItem> = Vec::new();
        let remote = vec![memo("gone", "删过的", 10)];
        let gone = "gone".to_string();
        let deleted: HashSet<&String> = HashSet::from([&gone]);
        let (merged, skipped) = merge(&local, &remote, &deleted);
        assert!(merged.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn parse_remote_drops_broken_rows() {
        let value = json!({
            "format": FORMAT,
            "memos": [
                {"id": "", "text": "没有 id"},
                {"id": "x", "text": "   "},
                {"id": "ok", "text": "正常的", "createdAtMs": 5, "source": "phone"}
            ]
        });
        let items = parse_remote(&value);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "ok");
        assert_eq!(items[0].source, "phone");
    }
}
