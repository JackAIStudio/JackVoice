//! TOS glossary sync shared with the Android IME.
//!
//! Cloud object: `jackvoice-glossary/glossary.json` in `cn-beijing`.
//! The Android app writes the same `jackvoice-glossary-v1` schema, so IDs and
//! disabled flags survive a round trip through this module.
//!
//! Sync is local-first: the local JSON files are the source for immediate reads,
//! and this module merges them with the cloud copy before writing anything back.

use crate::hotwords::{self, ReplacementRule};
use crate::storage;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
const STATE_FILE: &str = "glossary-sync-state.json";
const FORMAT: &str = "jackvoice-glossary-v1";
const MAX_ATTEMPTS: usize = 3;

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
    pub last_synced_at: Option<String>,
    pub local_changed_at: Option<String>,
    pub last_error: Option<String>,
    pub last_conflicts: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub hotword_count: usize,
    pub replacement_count: usize,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub hotword_count: usize,
    pub replacement_count: usize,
    pub pulled: bool,
    pub pushed: bool,
    pub conflicts: usize,
    pub hotwords_changed: bool,
    pub synced_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncState {
    base: Option<Value>,
    last_synced_at: Option<String>,
    local_changed_at: Option<String>,
    last_error: Option<String>,
    last_conflicts: usize,
    local_revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalGlossary {
    words: Vec<String>,
    rules: Vec<ReplacementRule>,
}

#[derive(Debug)]
enum PushError {
    Conflict,
    Other(String),
}

#[derive(Debug)]
struct MergeOutcome {
    glossary: LocalGlossary,
    conflicts: usize,
    changed_local: bool,
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

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
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
    let state = load_state(dir);
    TosSyncStatus {
        has_credentials: load_credentials(dir).ok().flatten().is_some(),
        bucket: BUCKET.into(),
        key: KEY.into(),
        hotword_count: hotwords::sanitize(&hotwords::load(dir)).len(),
        replacement_count: hotwords::sanitize_replacements(&hotwords::load_replacements(dir)).len(),
        last_synced_at: state.last_synced_at,
        local_changed_at: state.local_changed_at,
        last_error: state.last_error,
        last_conflicts: state.last_conflicts,
    }
}

/// 用户改了本地词库：记录“有未推送改动”，让下一次同步知道本机也动过。
pub fn mark_local_change(dir: &Path) -> Result<(), String> {
    let mut state = load_state(dir);
    state.local_changed_at = Some(now_rfc3339());
    state.local_revision = state.local_revision.saturating_add(1);
    save_state(dir, &state)
}

fn load_state(dir: &Path) -> SyncState {
    let path = state_path(dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return SyncState::default();
    };
    serde_json::from_str(&raw).unwrap_or_else(|error| {
        eprintln!("[glossary-sync] 同步状态文件损坏，按首次同步处理：{error}");
        SyncState::default()
    })
}

fn save_state(dir: &Path, state: &SyncState) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("序列化词库同步状态失败：{error}"))?;
    storage::write_atomic(&state_path(dir), &raw, true)
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

async fn fetch_remote(
    credentials: &TosCredentials,
) -> Result<Option<(Value, String)>, String> {
    let client = client(credentials)?;
    match client.get_object(&GetObjectInput::new(BUCKET, KEY)).await {
        Ok(mut output) => {
            let etag = output.etag().to_string();
            let bytes = output
                .read_all()
                .await
                .map_err(|error| format!("读取 TOS 词库失败：{error}"))?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("TOS 词库不是合法 JSON：{error}"))?;
            Ok(Some((value, etag)))
        }
        Err(error) => {
            if error
                .as_server_error()
                .map(|server| server.status_code() == 404)
                .unwrap_or(false)
            {
                return Ok(None);
            }
            Err(format!("从 TOS 拉取词库失败：{error}"))
        }
    }
}

async fn push_body(
    credentials: &TosCredentials,
    body: &Value,
    etag: Option<&str>,
) -> Result<(), PushError> {
    let client = client(credentials).map_err(PushError::Other)?;
    let content = serde_json::to_vec_pretty(body)
        .map_err(|error| PushError::Other(format!("序列化词库失败：{error}")))?;
    let mut input = PutObjectFromBufferInput::new_with_content(BUCKET, KEY, &content);
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
            Err(PushError::Other(format!("推送到 TOS 失败：{error}")))
        }
    }
}

fn parse_cloud(value: &Value) -> Result<LocalGlossary, String> {
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
    Ok(LocalGlossary {
        words: hotwords::sanitize(&words),
        rules: hotwords::sanitize_replacements(&rules),
    })
}

fn load_local(dir: &Path) -> LocalGlossary {
    LocalGlossary {
        words: hotwords::sanitize(&hotwords::load(dir)),
        rules: hotwords::sanitize_replacements(&hotwords::load_replacements(dir)),
    }
}

fn build_cloud(words: &[String], rules: &[ReplacementRule], remote: Option<&Value>) -> Value {
    let words = hotwords::sanitize(words);
    let rules = hotwords::sanitize_replacements(rules);
    let old_words = index_items(
        remote.and_then(|value| value.get("hotwords")),
        "term",
        |raw| hotwords::normalize_volc_hotword(raw).to_lowercase(),
    );
    let old_rules = index_items(
        remote.and_then(|value| value.get("replacements")),
        "from",
        |raw| raw.trim().to_lowercase(),
    );

    let mut hotword_items = words
        .iter()
        .map(|word| {
            let old = old_words.get(&word.to_lowercase());
            json!({
                "id": old.and_then(|item| item.get("id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(new_id),
                "term": word,
                "isEnabled": true,
            })
        })
        .collect::<Vec<_>>();
    let mut seen_words = words.iter().map(|word| word.to_lowercase()).collect::<HashSet<_>>();

    let mut replacement_items = rules
        .iter()
        .map(|rule| {
            let old = old_rules.get(&rule.from.to_lowercase());
            json!({
                "id": old.and_then(|item| item.get("id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(new_id),
                "from": rule.from,
                "to": rule.to,
                "isEnabled": true,
            })
        })
        .collect::<Vec<_>>();
    let mut seen_rules = rules.iter().map(|rule| rule.from.to_lowercase()).collect::<HashSet<_>>();

    // 停用项不参与听写，但也不该因为一次推送就从云端消失。
    if let Some(items) = remote.and_then(|value| value.get("hotwords")).and_then(Value::as_array) {
        for item in items {
            if item.get("isEnabled").and_then(Value::as_bool).unwrap_or(true) {
                continue;
            }
            let term = item.get("term").and_then(Value::as_str).unwrap_or_default();
            let key = hotwords::normalize_volc_hotword(term).to_lowercase();
            if key.is_empty() || !seen_words.insert(key) {
                continue;
            }
            hotword_items.push(item.clone());
        }
    }
    if let Some(items) = remote.and_then(|value| value.get("replacements")).and_then(Value::as_array) {
        for item in items {
            if item.get("isEnabled").and_then(Value::as_bool).unwrap_or(true) {
                continue;
            }
            let from = item.get("from").and_then(Value::as_str).unwrap_or_default();
            let key = from.trim().to_lowercase();
            if key.is_empty() || !seen_rules.insert(key) {
                continue;
            }
            replacement_items.push(item.clone());
        }
    }

    json!({
        "format": FORMAT,
        "version": 1,
        "updatedAt": now_rfc3339(),
        "updatedBy": "macOS",
        "hotwords": hotword_items,
        "replacements": replacement_items,
    })
}

fn index_items(
    items: Option<&Value>,
    key: &str,
    normalize: impl Fn(&str) -> String,
) -> HashMap<String, Value> {
    items
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let value = normalize(item.get(key)?.as_str()?);
                    (!value.is_empty()).then(|| (value, item.clone()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc))
    })
}

fn word_map(glossary: &LocalGlossary) -> HashMap<String, String> {
    glossary
        .words
        .iter()
        .map(|word| (hotwords::normalize_volc_hotword(word).to_lowercase(), word.clone()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

fn rule_map(glossary: &LocalGlossary) -> HashMap<String, ReplacementRule> {
    glossary
        .rules
        .iter()
        .map(|rule| {
            (
                rule.from.trim().to_lowercase(),
                ReplacementRule {
                    from: rule.from.trim().to_string(),
                    to: rule.to.trim().to_string(),
                },
            )
        })
        .filter(|(key, rule)| !key.is_empty() && !rule.to.is_empty())
        .collect()
}

fn rule_value_map(glossary: &LocalGlossary) -> HashMap<String, String> {
    glossary
        .rules
        .iter()
        .map(|rule| (rule.from.trim().to_lowercase(), rule.to.trim().to_string()))
        .filter(|(key, to)| !key.is_empty() && !to.is_empty())
        .collect()
}

fn same_glossary(a: &LocalGlossary, b: &LocalGlossary) -> bool {
    word_map(a) == word_map(b) && rule_value_map(a) == rule_value_map(b)
}

fn same_words(a: &[String], b: &[String]) -> bool {
    hotwords::sanitize(a) == hotwords::sanitize(b)
}

fn pick<T: Clone + PartialEq>(
    has_base: bool,
    base: Option<&T>,
    local: Option<&T>,
    remote: Option<&T>,
    remote_wins: bool,
) -> (Option<T>, bool) {
    if !has_base {
        return (local.or(remote).cloned(), false);
    }
    if local == base {
        return (remote.cloned(), false);
    }
    if remote == base {
        return (local.cloned(), false);
    }
    if local == remote {
        return (local.cloned(), false);
    }
    (
        if remote_wins { remote.cloned() } else { local.cloned() },
        true,
    )
}

fn merge(
    base: Option<&LocalGlossary>,
    local: &LocalGlossary,
    remote: &LocalGlossary,
    local_changed_at: Option<DateTime<Utc>>,
    remote_updated_at: Option<DateTime<Utc>>,
) -> MergeOutcome {
    let base_words = base.map(word_map).unwrap_or_default();
    let local_words = word_map(local);
    let remote_words = word_map(remote);
    let base_rules = base.map(rule_map).unwrap_or_default();
    let local_rules = rule_map(local);
    let remote_rules = rule_map(remote);
    let remote_wins = match (remote_updated_at, local_changed_at) {
        (Some(remote), Some(local)) => remote > local,
        (Some(_), None) => true,
        _ => false,
    };

    let mut conflicts = 0usize;
    let mut picked_words: HashMap<String, Option<String>> = HashMap::new();
    let mut word_keys = Vec::new();
    let mut seen_words = HashSet::new();
    for word in local.words.iter().chain(remote.words.iter()) {
        let key = hotwords::normalize_volc_hotword(word).to_lowercase();
        if !key.is_empty() && seen_words.insert(key.clone()) {
            word_keys.push(key);
        }
    }
    for key in base_words.keys() {
        if seen_words.insert(key.clone()) {
            word_keys.push(key.clone());
        }
    }
    for key in word_keys {
        let (picked, conflict) = pick(
            base.is_some(),
            base_words.get(&key),
            local_words.get(&key),
            remote_words.get(&key),
            remote_wins,
        );
        if conflict {
            conflicts += 1;
        }
        picked_words.insert(key, picked);
    }

    let mut picked_rules: HashMap<String, Option<ReplacementRule>> = HashMap::new();
    let mut rule_keys = Vec::new();
    let mut seen_rules = HashSet::new();
    for rule in local.rules.iter().chain(remote.rules.iter()) {
        let key = rule.from.trim().to_lowercase();
        if !key.is_empty() && seen_rules.insert(key.clone()) {
            rule_keys.push(key);
        }
    }
    for key in base_rules.keys() {
        if seen_rules.insert(key.clone()) {
            rule_keys.push(key.clone());
        }
    }
    for key in rule_keys {
        let (picked, conflict) = pick(
            base.is_some(),
            base_rules.get(&key),
            local_rules.get(&key),
            remote_rules.get(&key),
            remote_wins,
        );
        if conflict {
            conflicts += 1;
        }
        picked_rules.insert(key, picked);
    }

    let mut merged_words = Vec::new();
    let mut emitted_words = HashSet::new();
    for word in local.words.iter().chain(remote.words.iter()) {
        let key = hotwords::normalize_volc_hotword(word).to_lowercase();
        if key.is_empty() || !emitted_words.insert(key.clone()) {
            continue;
        }
        if let Some(Some(value)) = picked_words.get(&key) {
            merged_words.push(value.clone());
        }
    }

    let mut merged_rules = Vec::new();
    let mut emitted_rules = HashSet::new();
    for rule in local.rules.iter().chain(remote.rules.iter()) {
        let key = rule.from.trim().to_lowercase();
        if key.is_empty() || !emitted_rules.insert(key.clone()) {
            continue;
        }
        if let Some(Some(value)) = picked_rules.get(&key) {
            merged_rules.push(value.clone());
        }
    }

    let glossary = LocalGlossary {
        words: merged_words,
        rules: merged_rules,
    };
    let changed_local = !same_glossary(&glossary, local);
    MergeOutcome {
        glossary,
        conflicts,
        changed_local,
    }
}

fn report(
    local: &LocalGlossary,
    pulled: bool,
    pushed: bool,
    conflicts: usize,
    hotwords_changed: bool,
    updated_at: String,
) -> SyncReport {
    SyncReport {
        hotword_count: local.words.len(),
        replacement_count: local.rules.len(),
        pulled,
        pushed,
        conflicts,
        hotwords_changed,
        synced_at: now_rfc3339(),
        updated_at,
    }
}

fn fail_state(dir: &Path, message: String) -> String {
    let mut state = load_state(dir);
    state.last_error = Some(message.clone());
    let _ = save_state(dir, &state);
    message
}

pub async fn sync(dir: &Path) -> Result<SyncReport, String> {
    sync_inner(dir).await.map_err(|error| fail_state(dir, error))
}

async fn sync_inner(dir: &Path) -> Result<SyncReport, String> {
    let credentials = load_credentials(dir)?.ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let mut last_conflict_error = None;

    for _ in 0..MAX_ATTEMPTS {
        let state = load_state(dir);
        let revision = state.local_revision;
        let local = load_local(dir);
        let remote = fetch_remote(&credentials).await?;
        if load_state(dir).local_revision != revision {
            continue;
        }

        let Some((remote_value, etag)) = remote else {
            let body = build_cloud(&local.words, &local.rules, None);
            match push_body(&credentials, &body, None).await {
                Ok(()) => {
                    let mut next = load_state(dir);
                    next.base = Some(body.clone());
                    next.last_synced_at = Some(now_rfc3339());
                    next.local_changed_at = None;
                    next.last_error = None;
                    next.last_conflicts = 0;
                    save_state(dir, &next)?;
                    return Ok(report(
                        &local,
                        false,
                        true,
                        0,
                        true,
                        updated_at(&body),
                    ));
                }
                Err(PushError::Conflict) => {
                    last_conflict_error = Some("云端刚被另一台设备创建，正在重试。".to_string());
                    continue;
                }
                Err(PushError::Other(error)) => return Err(fail_state(dir, error)),
            }
        };

        let remote_snapshot = parse_cloud(&remote_value)?;
        let base = state.base.as_ref().map(parse_cloud).transpose()?;
        let remote_updated_at = parse_time(remote_value.get("updatedAt").and_then(Value::as_str));
        let local_changed_at = state
            .local_changed_at
            .as_deref()
            .and_then(|raw| parse_time(Some(raw)));
        let merged = merge(
            base.as_ref(),
            &local,
            &remote_snapshot,
            local_changed_at,
            remote_updated_at,
        );

        if load_state(dir).local_revision != revision {
            continue;
        }
        if merged.changed_local {
            hotwords::save(dir, &merged.glossary.words)?;
            hotwords::save_replacements(dir, &merged.glossary.rules)?;
        }

        let pulled = !same_glossary(&local, &merged.glossary);
        let hotwords_changed = !same_words(&local.words, &merged.glossary.words);
        if !same_glossary(&merged.glossary, &remote_snapshot) {
            let body = build_cloud(
                &merged.glossary.words,
                &merged.glossary.rules,
                Some(&remote_value),
            );
            match push_body(&credentials, &body, Some(&etag)).await {
                Ok(()) => {
                    let mut next = load_state(dir);
                    next.base = Some(body.clone());
                    next.last_synced_at = Some(now_rfc3339());
                    next.local_changed_at = None;
                    next.last_error = None;
                    next.last_conflicts = merged.conflicts;
                    save_state(dir, &next)?;
                    return Ok(report(
                        &merged.glossary,
                        pulled,
                        true,
                        merged.conflicts,
                        hotwords_changed,
                        updated_at(&body),
                    ));
                }
                Err(PushError::Conflict) => {
                    last_conflict_error = Some("云端刚被另一台设备更新，正在重试。".to_string());
                    continue;
                }
                Err(PushError::Other(error)) => return Err(fail_state(dir, error)),
            }
        }

        let mut next = load_state(dir);
        next.base = Some(remote_value.clone());
        next.last_synced_at = Some(now_rfc3339());
        next.local_changed_at = None;
        next.last_error = None;
        next.last_conflicts = merged.conflicts;
        save_state(dir, &next)?;
        return Ok(report(
            &merged.glossary,
            pulled,
            false,
            merged.conflicts,
            hotwords_changed,
            updated_at(&remote_value),
        ));
    }

    Err(fail_state(
        dir,
        last_conflict_error.unwrap_or_else(|| "同步重试次数过多。".to_string()),
    ))
}

pub async fn force_pull(dir: &Path) -> Result<SyncSummary, String> {
    force_pull_inner(dir).await.map_err(|error| fail_state(dir, error))
}

async fn force_pull_inner(dir: &Path) -> Result<SyncSummary, String> {
    let credentials = load_credentials(dir)?.ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let Some((remote_value, _etag)) = fetch_remote(&credentials).await? else {
        return Err("云端还没有 glossary.json。".into());
    };
    let snapshot = parse_cloud(&remote_value)?;
    hotwords::save(dir, &snapshot.words)?;
    hotwords::save_replacements(dir, &snapshot.rules)?;
    let mut state = load_state(dir);
    state.base = Some(remote_value.clone());
    state.last_synced_at = Some(now_rfc3339());
    state.local_changed_at = None;
    state.last_error = None;
    state.last_conflicts = 0;
    save_state(dir, &state)?;
    Ok(SyncSummary {
        hotword_count: snapshot.words.len(),
        replacement_count: snapshot.rules.len(),
        updated_at: updated_at(&remote_value),
    })
}

pub async fn force_push(dir: &Path) -> Result<SyncSummary, String> {
    force_push_inner(dir).await.map_err(|error| fail_state(dir, error))
}

async fn force_push_inner(dir: &Path) -> Result<SyncSummary, String> {
    let credentials = load_credentials(dir)?.ok_or("还没有填写 TOS AccessKey / SecretKey。")?;
    let local = load_local(dir);
    let mut last_error = None;
    for _ in 0..MAX_ATTEMPTS {
        let remote = fetch_remote(&credentials).await?;
        let body = build_cloud(
            &local.words,
            &local.rules,
            remote.as_ref().map(|(value, _)| value),
        );
        let etag = remote.as_ref().map(|(_, etag)| etag.as_str());
        match push_body(&credentials, &body, etag).await {
            Ok(()) => {
                let mut state = load_state(dir);
                state.base = Some(body.clone());
                state.last_synced_at = Some(now_rfc3339());
                state.local_changed_at = None;
                state.last_error = None;
                state.last_conflicts = 0;
                save_state(dir, &state)?;
                return Ok(SyncSummary {
                    hotword_count: local.words.len(),
                    replacement_count: local.rules.len(),
                    updated_at: updated_at(&body),
                });
            }
            Err(PushError::Conflict) => {
                last_error = Some("云端刚被另一台设备更新，正在重试。".to_string());
                continue;
            }
            Err(PushError::Other(error)) => return Err(fail_state(dir, error)),
        }
    }
    Err(fail_state(
        dir,
        last_error.unwrap_or_else(|| "推送重试次数过多。".to_string()),
    ))
}

fn updated_at(value: &Value) -> String {
    value
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(words: &[&str], rules: &[(&str, &str)]) -> LocalGlossary {
        LocalGlossary {
            words: words.iter().map(|word| word.to_string()).collect(),
            rules: rules
                .iter()
                .map(|(from, to)| ReplacementRule {
                    from: from.to_string(),
                    to: to.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn local_add_is_kept_when_remote_is_unchanged() {
        let base = snap(&["A"], &[]);
        let local = snap(&["A", "B"], &[]);
        let remote = snap(&["A"], &[]);
        let out = merge(Some(&base), &local, &remote, None, None);
        assert_eq!(out.glossary.words, vec!["A", "B"]);
        assert_eq!(out.conflicts, 0);
    }

    #[test]
    fn local_deletion_propagates() {
        let base = snap(&["A", "B"], &[]);
        let local = snap(&["A"], &[]);
        let remote = snap(&["A", "B"], &[]);
        let out = merge(Some(&base), &local, &remote, None, None);
        assert_eq!(out.glossary.words, vec!["A"]);
    }

    #[test]
    fn remote_deletion_propagates() {
        let base = snap(&["A", "B"], &[]);
        let local = snap(&["A", "B"], &[]);
        let remote = snap(&["A"], &[]);
        let out = merge(Some(&base), &local, &remote, None, None);
        assert_eq!(out.glossary.words, vec!["A"]);
        assert!(out.changed_local);
    }

    #[test]
    fn same_rule_edited_on_both_sides_picks_newer() {
        let base = snap(&[], &[("Cloud", "Claude")]);
        let local = snap(&[], &[("Cloud", "云端")]);
        let remote = snap(&[], &[("Cloud", "Claude 2")]);
        let newer = merge(
            Some(&base),
            &local,
            &remote,
            Some(Utc::now() - chrono::Duration::seconds(10)),
            Some(Utc::now()),
        );
        assert_eq!(newer.glossary.rules.single().to, "Claude 2");
        assert_eq!(newer.conflicts, 1);
    }

    #[test]
    fn bootstrap_keeps_both_sides() {
        let local = snap(&["A"], &[("Cloud", "Claude")]);
        let remote = snap(&["B"], &[("Tree", "Trae")]);
        let out = merge(None, &local, &remote, None, None);
        assert_eq!(out.glossary.words, vec!["A", "B"]);
        assert_eq!(out.glossary.rules.len(), 2);
    }

    trait Single<T> {
        fn single(self) -> T;
    }

    impl<T> Single<T> for Vec<T> {
        fn single(mut self) -> T {
            assert_eq!(self.len(), 1);
            self.remove(0)
        }
    }

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_pull() {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let summary = force_pull(&dir).await.expect("pull");
        assert!(summary.hotword_count > 0);
        assert!(summary.replacement_count > 0);
        println!(
            "pulled {} hotwords / {} replacements",
            summary.hotword_count, summary.replacement_count
        );
    }

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_sync() {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let report = sync(&dir).await.expect("sync");
        assert!(report.hotword_count > 0);
        assert!(report.replacement_count > 0);
        println!(
            "synced {} hotwords / {} replacements, pulled={}, pushed={}, conflicts={}",
            report.hotword_count,
            report.replacement_count,
            report.pulled,
            report.pushed,
            report.conflicts
        );
    }

    struct Restore {
        paths: Vec<(PathBuf, Option<Vec<u8>>)>,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            for (path, bytes) in &self.paths {
                match bytes {
                    Some(bytes) => {
                        let _ = std::fs::write(path, bytes);
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }

    fn backup(path: &Path) -> (PathBuf, Option<Vec<u8>>) {
        (path.to_path_buf(), std::fs::read(path).ok())
    }

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_two_device_roundtrip() {
        let shared = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let temp = std::env::temp_dir().join(format!("jackvoice-sync-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).expect("temp dir");
        std::fs::copy(
            credentials_path(&shared),
            credentials_path(&temp),
        )
        .expect("copy credentials");

        let _restore = Restore {
            paths: vec![
                backup(&hotwords::hotwords_path(&shared)),
                backup(&hotwords::replacements_path(&shared)),
                backup(&state_path(&shared)),
            ],
        };

        let original_words = hotwords::load(&shared);
        let original_rules = hotwords::load_replacements(&shared);
        let probe_word = "SyncProbeWord2026";
        let probe_rule = ReplacementRule {
            from: "SyncProbeFrom2026".into(),
            to: "SyncProbeTo2026".into(),
        };

        let mut with_probe_words = original_words.clone();
        with_probe_words.push(probe_word.into());
        hotwords::save(&shared, &with_probe_words).expect("save probe words");
        let mut with_probe_rules = original_rules.clone();
        with_probe_rules.push(probe_rule.clone());
        hotwords::save_replacements(&shared, &with_probe_rules).expect("save probe rules");
        mark_local_change(&shared).expect("mark probe change");

        let pushed = sync(&shared).await.expect("push probe");
        assert!(pushed.pushed);
        assert_eq!(pushed.hotword_count, original_words.len() + 1);
        assert_eq!(pushed.replacement_count, original_rules.len() + 1);

        let pulled = sync(&temp).await.expect("second device pull");
        assert!(pulled.pulled);
        assert_eq!(pulled.hotword_count, original_words.len() + 1);
        assert_eq!(pulled.replacement_count, original_rules.len() + 1);
        assert!(hotwords::load(&temp).iter().any(|word| word == probe_word));
        assert!(hotwords::load_replacements(&temp)
            .iter()
            .any(|rule| rule == &probe_rule));

        hotwords::save(&shared, &original_words).expect("restore probe words");
        hotwords::save_replacements(&shared, &original_rules).expect("restore probe rules");
        mark_local_change(&shared).expect("mark probe deletion");
        let deleted = sync(&shared).await.expect("push probe deletion");
        assert!(deleted.pushed);
        assert_eq!(deleted.hotword_count, original_words.len());
        assert_eq!(deleted.replacement_count, original_rules.len());

        let synced_back = sync(&temp).await.expect("second device pull deletion");
        assert_eq!(synced_back.hotword_count, original_words.len());
        assert_eq!(synced_back.replacement_count, original_rules.len());
        assert!(!hotwords::load(&temp).iter().any(|word| word == probe_word));
        assert!(!hotwords::load_replacements(&temp)
            .iter()
            .any(|rule| rule == &probe_rule));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[tokio::test]
    #[ignore = "uses the local TOS credentials and network"]
    async fn tos_sync_live_push() {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join("Library/Application Support/com.jackvoice.shared");
        let summary = force_push(&dir).await.expect("push");
        assert!(summary.hotword_count > 0);
        assert!(summary.replacement_count > 0);
        println!(
            "pushed {} hotwords / {} replacements",
            summary.hotword_count, summary.replacement_count
        );
    }
}
