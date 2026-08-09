//! 火山引擎热词表管理 API（API-Key 鉴权）。
//!
//! 文档：https://www.volcengine.com/docs/6561/1742791
//! 路由：https://openspeech.bytedance.com/api/proxy/invoke/?Action={Action}&Version=2022-08-30
//! 鉴权：Header `X-Api-Key`

use reqwest::multipart;
use serde::Deserialize;
use serde_json::json;

const VOLC_PROXY_URL: &str = "https://openspeech.bytedance.com/api/proxy/invoke/";
const VOLC_API_VERSION: &str = "2022-08-30";

#[derive(Debug, Clone)]
pub struct VolcHotwordSyncResult {
    pub table_id: String,
    pub table_name: String,
    pub word_count: usize,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    #[serde(rename = "Result")]
    result: Option<T>,
    #[serde(default, rename = "ResponseMetadata")]
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    #[serde(default, rename = "Error")]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default, rename = "Code")]
    code: String,
    #[serde(default, rename = "Message")]
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct GetResult {
    #[serde(default, rename = "BoostingTable")]
    boosting_table: Option<BoostingTableSummary>,
}

#[derive(Debug, Default, Deserialize)]
struct UpdateResult {
    #[serde(default, rename = "BoostingTable")]
    boosting_table: Option<BoostingTableSummary>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct BoostingTableSummary {
    #[serde(default, rename = "BoostingTableID")]
    boosting_table_id: String,
    #[serde(default, rename = "BoostingTableName")]
    boosting_table_name: String,
    #[serde(default, rename = "WordCount")]
    word_count: usize,
}

fn api_error_message(raw: &str, status: reqwest::StatusCode) -> String {
    if let Ok(env) = serde_json::from_str::<ApiEnvelope<serde_json::Value>>(raw) {
        if let Some(meta) = env.response_metadata {
            if let Some(err) = meta.error {
                if !err.message.is_empty() {
                    return format!("{} ({})", err.message, err.code);
                }
            }
        }
    }
    if raw.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        raw.chars().take(240).collect()
    }
}

/// 更新已有热词表内容（整表覆盖）。
/// 文件字段名必须是 `File`；内容为每行 `词|权重`，不要尾随空行。
pub async fn update_boosting_table(
    api_key: &str,
    table_id: &str,
    _table_name: Option<&str>,
    file_content: &str,
) -> Result<VolcHotwordSyncResult, String> {
    let api_key = api_key.trim();
    let table_id = table_id.trim();
    if api_key.is_empty() {
        return Err("请先配置豆包录音识别 API Key。".into());
    }
    if table_id.is_empty() {
        return Err("请先在设置里填写热词表 ID。".into());
    }
    if file_content.trim().is_empty() {
        return Err("本地词库为空，无法同步到火山热词表。".into());
    }

    let form = multipart::Form::new()
        .text("BoostingTableID", table_id.to_string())
        .part(
            "File",
            multipart::Part::text(file_content.to_string())
                .file_name("hotwords.txt")
                .mime_str("text/plain")
                .map_err(|e| format!("构造热词文件失败：{e}"))?,
        );

    let client = reqwest::Client::new();
    let url = format!("{VOLC_PROXY_URL}?Action=UpdateBoostingTable&Version={VOLC_API_VERSION}");
    let response = client
        .post(url)
        .header("X-Api-Key", api_key)
        .header("Accept", "*/*")
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("同步火山热词表失败：{e}"))?;
    let status = response.status();
    let raw = response
        .text()
        .await
        .map_err(|e| format!("读取同步响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "同步火山热词表失败：{}",
            api_error_message(&raw, status)
        ));
    }

    let env: ApiEnvelope<UpdateResult> = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "解析同步响应失败：{e}；原文：{}",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    if let Some(meta) = env.response_metadata {
        if let Some(err) = meta.error {
            if !err.message.is_empty() {
                return Err(format!(
                    "同步火山热词表失败：{} ({})",
                    err.message, err.code
                ));
            }
        }
    }
    let table = env
        .result
        .and_then(|r| r.boosting_table)
        .ok_or_else(|| "同步成功但未返回热词表信息。".to_string())?;
    Ok(VolcHotwordSyncResult {
        table_id: if table.boosting_table_id.is_empty() {
            table_id.to_string()
        } else {
            table.boosting_table_id
        },
        table_name: table.boosting_table_name,
        word_count: table.word_count,
    })
}

/// 可选：读取热词表名称，便于状态展示。
#[allow(dead_code)]
pub async fn get_boosting_table_name(api_key: &str, table_id: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let url = format!("{VOLC_PROXY_URL}?Action=GetBoostingTable&Version={VOLC_API_VERSION}");
    let body = json!({
        "Action": "GetBoostingTable",
        "Version": VOLC_API_VERSION,
        "BoostingTableID": table_id,
    });
    let response = client
        .post(url)
        .header("X-Api-Key", api_key.trim())
        .header("Accept", "*/*")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("查询火山热词表失败：{e}"))?;
    let status = response.status();
    let raw = response
        .text()
        .await
        .map_err(|e| format!("读取热词表查询响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "查询火山热词表失败：{}",
            api_error_message(&raw, status)
        ));
    }
    let env: ApiEnvelope<GetResult> =
        serde_json::from_str(&raw).map_err(|e| format!("解析热词表查询响应失败：{e}"))?;
    Ok(env
        .result
        .and_then(|r| r.boosting_table)
        .map(|t| t.boosting_table_name)
        .unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct CorrectUpdateResult {
    #[serde(default, rename = "CorrectTable")]
    correct_table: Option<CorrectTableSummary>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct CorrectTableSummary {
    #[serde(default, rename = "CorrectTableID")]
    correct_table_id: String,
    #[serde(default, rename = "CorrectTableName")]
    correct_table_name: String,
    #[serde(default, rename = "WordCount")]
    word_count: usize,
}

/// 更新火山替换词表（整表覆盖）。字段名 `File`，内容每行 `识别形|最终写法`。
/// 注意：部分账号/环境下该接口可能返回 InternalError；调用方应把本地替换当主路径。
#[allow(dead_code)]
pub async fn update_correct_table(
    api_key: &str,
    table_id: &str,
    file_content: &str,
) -> Result<VolcHotwordSyncResult, String> {
    let api_key = api_key.trim();
    let table_id = table_id.trim();
    if api_key.is_empty() {
        return Err("请先配置豆包录音识别 API Key。".into());
    }
    if table_id.is_empty() {
        return Err("请先在设置里填写替换词表 ID。".into());
    }
    if file_content.trim().is_empty() {
        return Err("没有可同步的替换规则。".into());
    }

    let form = multipart::Form::new()
        .text("CorrectTableID", table_id.to_string())
        .part(
            "File",
            multipart::Part::text(file_content.to_string())
                .file_name("correct.txt")
                .mime_str("text/plain")
                .map_err(|e| format!("构造替换词文件失败：{e}"))?,
        );

    let client = reqwest::Client::new();
    let url = format!("{VOLC_PROXY_URL}?Action=UpdateCorrectTable&Version={VOLC_API_VERSION}");
    let response = client
        .post(url)
        .header("X-Api-Key", api_key)
        .header("Accept", "*/*")
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("同步火山替换词表失败：{e}"))?;
    let status = response.status();
    let raw = response
        .text()
        .await
        .map_err(|e| format!("读取替换词同步响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "同步火山替换词表失败：{}",
            api_error_message(&raw, status)
        ));
    }
    let env: ApiEnvelope<CorrectUpdateResult> = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "解析替换词同步响应失败：{e}；原文：{}",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    if let Some(meta) = env.response_metadata {
        if let Some(err) = meta.error {
            if !err.message.is_empty() {
                return Err(format!(
                    "同步火山替换词表失败：{} ({})",
                    err.message, err.code
                ));
            }
        }
    }
    let table = env.result.and_then(|r| r.correct_table);
    Ok(VolcHotwordSyncResult {
        table_id: table
            .as_ref()
            .map(|t| t.correct_table_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| table_id.to_string()),
        table_name: table
            .as_ref()
            .map(|t| t.correct_table_name.clone())
            .unwrap_or_default(),
        word_count: table.map(|t| t.word_count).unwrap_or(0),
    })
}

/// 创建火山替换词表。注意：创建接口字段名是 `TableName`，不是 `CorrectTableName`。
/// 部分账号下 `UpdateCorrectTable` 会稳定返回 InternalError，此时应改走创建新表。
#[allow(dead_code)]
pub async fn create_correct_table(
    api_key: &str,
    table_name: &str,
    file_content: &str,
) -> Result<VolcHotwordSyncResult, String> {
    let api_key = api_key.trim();
    let table_name = table_name.trim();
    if api_key.is_empty() {
        return Err("请先配置豆包录音识别 API Key。".into());
    }
    if table_name.is_empty() {
        return Err("替换词表名称不能为空。".into());
    }
    if file_content.trim().is_empty() {
        return Err("没有可同步的替换规则。".into());
    }

    let form = multipart::Form::new()
        .text("TableName", table_name.to_string())
        .part(
            "File",
            multipart::Part::text(file_content.to_string())
                .file_name("correct.txt")
                .mime_str("text/plain")
                .map_err(|e| format!("构造替换词文件失败：{e}"))?,
        );

    let client = reqwest::Client::new();
    let url = format!("{VOLC_PROXY_URL}?Action=CreateCorrectTable&Version={VOLC_API_VERSION}");
    let response = client
        .post(url)
        .header("X-Api-Key", api_key)
        .header("Accept", "*/*")
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("创建火山替换词表失败：{e}"))?;
    let status = response.status();
    let raw = response
        .text()
        .await
        .map_err(|e| format!("读取替换词创建响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "创建火山替换词表失败：{}",
            api_error_message(&raw, status)
        ));
    }
    let env: ApiEnvelope<CorrectUpdateResult> = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "解析替换词创建响应失败：{e}；原文：{}",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    if let Some(meta) = env.response_metadata {
        if let Some(err) = meta.error {
            if !err.message.is_empty() {
                return Err(format!(
                    "创建火山替换词表失败：{} ({})",
                    err.message, err.code
                ));
            }
        }
    }
    let table = env
        .result
        .and_then(|r| r.correct_table)
        .ok_or_else(|| "创建成功但未返回替换词表信息。".to_string())?;
    Ok(VolcHotwordSyncResult {
        table_id: if table.correct_table_id.is_empty() {
            String::new()
        } else {
            table.correct_table_id
        },
        table_name: if table.correct_table_name.is_empty() {
            table_name.to_string()
        } else {
            table.correct_table_name
        },
        word_count: table.word_count,
    })
}

/// 优先更新已有替换词表；若更新接口失败，则创建新表并返回新 ID。
/// 返回的 `table_id` 始终是应当写入本地设置的当前有效 ID。
#[allow(dead_code)]
pub async fn upsert_correct_table(
    api_key: &str,
    table_id: &str,
    file_content: &str,
) -> Result<VolcHotwordSyncResult, String> {
    let table_id = table_id.trim();
    if !table_id.is_empty() {
        match update_correct_table(api_key, table_id, file_content).await {
            Ok(sync) => return Ok(sync),
            Err(update_err) => {
                // 火山部分账号 UpdateCorrectTable 会稳定 500；降级为创建新表。
                let name = format!("jvfix{}", &uuid_like_suffix());
                match create_correct_table(api_key, &name, file_content).await {
                    Ok(mut sync) => {
                        if sync.table_id.is_empty() {
                            return Err(format!(
                                "更新失败且创建后未返回新表 ID；更新错误：{update_err}"
                            ));
                        }
                        // 在名称中保留提示，便于状态展示
                        if sync.table_name.is_empty() {
                            sync.table_name = name;
                        }
                        return Ok(sync);
                    }
                    Err(create_err) => {
                        return Err(format!(
                            "更新失败：{update_err}；创建新表也失败：{create_err}"
                        ));
                    }
                }
            }
        }
    }

    let name = format!("jvfix{}", &uuid_like_suffix());
    create_correct_table(api_key, &name, file_content).await
}

fn uuid_like_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
