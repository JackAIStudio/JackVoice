//! TOS glossary sync shared with the Android IME.
//!
//! Cloud object: `jackvoice-glossary/glossary.json` in `cn-beijing`.
//! The Android app writes the same `jackvoice-glossary-v1` schema, so IDs and
//! disabled flags survive a round trip through this module.

use crate::hotwords::{self, ReplacementRule};
use crate::storage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use ve_tos_rust_sdk::asynchronous::object::{ObjectAPI, ObjectContent};
use ve_tos_rust_sdk::asynchronous::tos;
use ve_tos_rust_sdk::asynchronous::tos::{AsyncRuntime, TosClient};
use ve_tos_rust_sdk::object::{GetObjectInput, PutObjectFromBufferInput};

const BUCKET: &str = "jackvoice-glossary";
const KEY: &str = "glossary.json";
const REGION: &str = "cn-beijing";
const ENDPOINT: &str = "https://tos-cn-beijing.volces.com";
const CREDENTIALS_FILE: &str = "tos-credentials.json";
const FORMAT: &str = "jackvoice-glossary-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TosCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TosSyncStatus {
    pub has_credentials: bool,
    pub bucket: String,
    pub key: String,
    pub hotword_count: usize,
    pub replacement_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub hotword_count: usize,
    pub replacement_count: usize,
    pub updated_at: String,
}

#[derive(Debug, Default)]
struct TokioRuntime;

#[async_trait::async_trait]
impl AsyncRuntime for TokioRuntime {
    type JoinError = tokio::task::JoinError;

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn spawn<'a, F>(&self, future: F) -> futures_util::future::BoxFuture<'a, Result<F::Output, Self::JoinError>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Box::pin(tokio::spawn(future))
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        tokio::runtime::Handle::current().block_on(future)
    }
}

fn credentials_path(dir: &Path) -> PathBuf {
    dir.join(CREDENTIALS_FILE)
}

pub fn load_credentials(dir: &Path) -> Result<Option<TosCredentials>, String> {
    let path = credentials_path(dir);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取 TOS 凭据失败：{error}"))?;
    let mut credentials: TosCredentials = serde_json::from_str(&raw)
        .map_err(|error| format!("解析 TOS 凭据失败：{error}"))?;
    credentials.access_key_id = credentials.access_key_id.trim().to_string();
    credentials.secret_access_key = credentials.secret_access_key.trim().to_string();
    if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
        return Ok(None);
    }
    Ok(Some(credentials))
}

pub fn save_credentials(
    dir: &Path,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<(), String> {
    let credentials = TosCredentials {
        access_key_id: access_key_id.trim().to_string(),
        secret_access_key: secret_access_key.trim().to_string(),
    };
    if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
        return Err("AccessKey ID 和 SecretKey 都不能为空。".into());
    }
    let raw = serde_json::to_vec_pretty(&credentials)
        .map_err(|error| format!("序列化 TOS 凭据失败：{error}"))?;
    storage::write_atomic(&credentials_path(dir), &raw, true)
}

pub fn status(dir: &Path) -> TosSyncStatus {
    TosSyncStatus {
        has_credentials: load_credentials(dir).ok().flatten().is_some(),
        bucket: BUCKET.into(),
        key: KEY.into(),
        hotword_count: hotwords::load(dir).len(),
        replacement_count: hotwords::load_replacements(dir).len(),
    }
}

fn client(credentials: &TosCredentials) -> Result<impl TosClient, String> {
    tos::builder::<TokioRuntime>()
        .connection_timeout(3_000)
        .request_timeout(10_000)
        .max_retry_count(3)
        .ak(credentials.access_key_id.clone())
        .sk(credentials.secret_access_key.clone())
        .region(REGION)
        .endpoint(ENDPOINT)
        .build()
        .map_err(|error| format!("创建 TOS 客户端失败：{error}"))
}

pub async fn pull(dir: &Path) -> Result<SyncSummary, String> {
    let credentials = load_credentials(dir)?.ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let client = client(&credentials)?;
    let mut output = client
        .get_object(&GetObjectInput::new(BUCKET, KEY))
        .await
        .map_err(|error| format!("从 TOS 拉取词库失败：{error}"))?;
    let bytes = output
        .read_all()
        .await
        .map_err(|error| format!("读取 TOS 词库失败：{error}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("TOS 词库不是合法 JSON：{error}"))?;
    let (words, rules, updated_at) = parse_cloud(&value)?;
    hotwords::save(dir, &words)?;
    hotwords::save_replacements(dir, &rules)?;
    Ok(SyncSummary {
        hotword_count: words.len(),
        replacement_count: rules.len(),
        updated_at,
    })
}

pub async fn push(dir: &Path) -> Result<SyncSummary, String> {
    match push_once(dir).await {
        Ok(summary) => Ok(summary),
        Err(error)
            if error.contains("412") || error.contains("PreconditionFailed")
                || error.contains("Precondition Failed") =>
        {
            push_once(dir).await
        }
        Err(error) => Err(error),
    }
}

async fn push_once(dir: &Path) -> Result<SyncSummary, String> {
    let credentials = load_credentials(dir)?.ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let client = client(&credentials)?;
    let mut remote = client
        .get_object(&GetObjectInput::new(BUCKET, KEY))
        .await
        .map_err(|error| format!("读取 TOS 现有词库失败：{error}"))?;
    let etag = remote.etag().to_string();
    let bytes = remote
        .read_all()
        .await
        .map_err(|error| format!("读取 TOS 现有词库失败：{error}"))?;
    let remote_value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("TOS 现有词库不是合法 JSON：{error}"))?;

    let words = hotwords::load(dir);
    let rules = hotwords::load_replacements(dir);
    let body = build_cloud(&words, &rules, &remote_value)?;
    let updated_at = body
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let content = serde_json::to_vec_pretty(&body)
        .map_err(|error| format!("序列化词库失败：{error}"))?;
    let mut input = PutObjectFromBufferInput::new_with_content(BUCKET, KEY, &content);
    input.set_if_match(etag);
    client
        .put_object_from_buffer(&input)
        .await
        .map_err(|error| format!("推送到 TOS 失败：{error}"))?;
    Ok(SyncSummary {
        hotword_count: words.len(),
        replacement_count: rules.len(),
        updated_at,
    })
}

fn parse_cloud(value: &Value) -> Result<(Vec<String>, Vec<ReplacementRule>, String), String> {
    if value.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err("TOS 词库格式不是 jackvoice-glossary-v1。".into());
    }
    let words = value
        .get("hotwords")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("isEnabled").and_then(Value::as_bool).unwrap_or(true))
                .filter_map(|item| item.get("term").and_then(Value::as_str))
                .map(str::trim)
                .filter(|word| !word.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let rules = value
        .get("replacements")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("isEnabled").and_then(Value::as_bool).unwrap_or(true))
                .filter_map(|item| {
                    let from = item.get("from").and_then(Value::as_str)?.trim();
                    let to = item.get("to").and_then(Value::as_str)?.trim();
                    (!from.is_empty() && !to.is_empty()).then(|| ReplacementRule {
                        from: from.to_string(),
                        to: to.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let updated_at = value
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((words, rules, updated_at))
}

fn build_cloud(
    words: &[String],
    rules: &[ReplacementRule],
    remote: &Value,
) -> Result<Value, String> {
    let old_words = index_by(remote.get("hotwords"), "term");
    let old_rules = index_by(remote.get("replacements"), "from");
    let hotword_items = words
        .iter()
        .map(|word| {
            let old = old_words.get(&word.to_lowercase());
            json!({
                "id": old.and_then(|item| item.get("id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(new_id),
                "term": word,
                "isEnabled": old.and_then(|item| item.get("isEnabled")).and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect::<Vec<_>>();
    let replacement_items = rules
        .iter()
        .map(|rule| {
            let old = old_rules.get(&rule.from.to_lowercase());
            json!({
                "id": old.and_then(|item| item.get("id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(new_id),
                "from": rule.from,
                "to": rule.to,
                "isEnabled": old.and_then(|item| item.get("isEnabled")).and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "format": FORMAT,
        "version": 1,
        "updatedAt": chrono::Utc::now().to_rfc3339(),
        "updatedBy": "macOS",
        "hotwords": hotword_items,
        "replacements": replacement_items,
    }))
}

fn index_by(items: Option<&Value>, key: &str) -> HashMap<String, Value> {
    items
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let value = item.get(key)?.as_str()?.trim().to_lowercase();
                    (!value.is_empty()).then(|| (value, item.clone()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_pull() {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let summary = pull(&dir).await.expect("pull");
        assert!(summary.hotword_count > 0);
        assert!(summary.replacement_count > 0);
        println!(
            "pulled {} hotwords / {} replacements",
            summary.hotword_count, summary.replacement_count
        );
    }

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_push() {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let summary = push(&dir).await.expect("push");
        assert!(summary.hotword_count > 0);
        assert!(summary.replacement_count > 0);
        println!(
            "pushed {} hotwords / {} replacements",
            summary.hotword_count, summary.replacement_count
        );
    }
}
