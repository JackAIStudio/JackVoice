use crate::normalize;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};
use uuid::Uuid;

/// 固定识别引擎：火山引擎豆包流式语音识别模型 2.0。
pub const ASR_ENGINE_ID: &str = "volc-seedasr-streaming";
/// 识别引擎展示名。
pub const ASR_ENGINE_NAME: &str = "火山引擎 豆包流式语音识别模型 2.0";

const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Error)]
pub enum AsrError {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptUpdate {
    pub text: String,
    pub is_final_sentence: bool,
}

/// 双向流式优化版接口（支持二遍识别）。
const VOLC_WS_URL: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
/// 火山引擎默认资源 ID：豆包流式语音识别模型 2.0 小时版。
const VOLC_DEFAULT_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";
/// 请求级热词直传的 token 上限（双向流式 100 tokens，按字符数保守折算）。
const VOLC_INLINE_HOTWORD_MAX_CHARS: usize = 100;

const SAUC_PROTOCOL_VERSION_HEADER_SIZE: u8 = 0b0001_0001;
const SAUC_MSG_FULL_CLIENT_REQUEST: u8 = 0b0001;
const SAUC_MSG_AUDIO_ONLY_REQUEST: u8 = 0b0010;
const SAUC_MSG_SERVER_FULL_RESPONSE: u8 = 0b1001;
const SAUC_MSG_SERVER_ERROR_RESPONSE: u8 = 0b1111;
const SAUC_FLAG_NO_SEQUENCE: u8 = 0b0000;
/// 最后一包（负包），header 后不跟 sequence number。
const SAUC_FLAG_FINAL_PACKET: u8 = 0b0010;
const SAUC_SERIALIZATION_RAW: u8 = 0b0000;
const SAUC_SERIALIZATION_JSON: u8 = 0b0001;
const SAUC_COMPRESSION_NONE: u8 = 0b0000;

/// 火山引擎连接配置（来自设置，BYOK）。
#[derive(Debug, Clone)]
pub struct VolcAsrConfig {
    pub api_key: String,
    pub resource_id: String,
    pub boosting_table_id: String,
}

/// 构造一个 SAUC 二进制帧：4 字节 header + 4 字节大端 payload 长度 + payload。
fn volc_frame(message_type: u8, flags: u8, serialization: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.push(SAUC_PROTOCOL_VERSION_HEADER_SIZE);
    frame.push((message_type << 4) | flags);
    frame.push((serialization << 4) | SAUC_COMPRESSION_NONE);
    frame.push(0x00);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// 增量解析服务端 SAUC 帧（一个 WebSocket Binary 消息内可能粘包）。
enum SaucServerFrame {
    Response {
        // 服务端帧序，解析时必须对齐读取；上层暂不依赖。
        #[allow(dead_code)]
        sequence: i32,
        /// 服务端用负序号标记最终响应（flags 0b0010 / 0b0011）。
        final_flag: bool,
        payload: Vec<u8>,
    },
    Error { code: u32, message: String },
    Ignored,
}

struct SaucDecoder {
    buf: Vec<u8>,
}

impl SaucDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn take_frames(&mut self) -> Result<Vec<SaucServerFrame>, String> {
        let mut frames = Vec::new();
        loop {
            let Some(frame) = self.try_parse_one()? else {
                break;
            };
            frames.push(frame);
        }
        Ok(frames)
    }

    fn try_parse_one(&mut self) -> Result<Option<SaucServerFrame>, String> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let header = &self.buf[..4];
        let message_type = header[1] >> 4;
        let flags = header[1] & 0x0f;
        let compression = header[2] & 0x0f;
        if compression != SAUC_COMPRESSION_NONE {
            return Err(format!(
                "火山引擎响应使用了不支持的压缩方式（{compression}）。"
            ));
        }

        // 服务端响应可能用正序号（0b0001）或负序号（0b0010 / 0b0011）
        // 标记最终结果；只要 flags 指示带 sequence 都需要对齐读取。
        let has_sequence = matches!(flags, 0b0001 | 0b0010 | 0b0011);
        let mut offset = 4usize;
        let mut sequence = 0i32;
        if has_sequence {
            if self.buf.len() < offset + 4 {
                return Ok(None);
            }
            sequence = i32::from_be_bytes(
                self.buf[offset..offset + 4]
                    .try_into()
                    .expect("sequence slice"),
            );
            offset += 4;
        }
        if self.buf.len() < offset + 4 {
            return Ok(None);
        }
        let payload_size =
            u32::from_be_bytes(self.buf[offset..offset + 4].try_into().expect("size slice"))
                as usize;
        offset += 4;
        if self.buf.len() < offset + payload_size {
            return Ok(None);
        }
        let payload = self.buf[offset..offset + payload_size].to_vec();
        self.buf.drain(..offset + payload_size);

        let frame = match message_type {
            SAUC_MSG_SERVER_FULL_RESPONSE => SaucServerFrame::Response {
                sequence,
                final_flag: matches!(flags, 0b0010 | 0b0011),
                payload,
            },
            SAUC_MSG_SERVER_ERROR_RESPONSE => {
                let (code, message) = parse_volc_error_payload(&payload);
                SaucServerFrame::Error { code, message }
            }
            _ => {
                // 未知消息类型消费掉该帧，避免阻塞后续帧解析。
                SaucServerFrame::Ignored
            }
        };
        Ok(Some(frame))
    }
}

fn parse_volc_error_payload(payload: &[u8]) -> (u32, String) {
    // 优先按二进制错误帧解析：error_code(u32) + message_size(u32) + UTF-8。
    if payload.len() >= 8 {
        let code = u32::from_be_bytes(payload[..4].try_into().expect("code slice"));
        let size =
            u32::from_be_bytes(payload[4..8].try_into().expect("size slice")) as usize;
        if payload.len() >= 8 + size {
            let raw = String::from_utf8_lossy(&payload[8..8 + size]).into_owned();
            // 部分实现把 error frame 的 payload 段当 JSON 返回（code + size + JSON），
            // 兼容解析；否则按官方布局当作纯文本错误信息。
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                let message = value
                    .pointer("/message")
                    .or_else(|| value.pointer("/error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&raw)
                    .to_string();
                return (code, message);
            }
            return (code, raw);
        }
    }
    // 兜底：部分网关错误可能直接返回 JSON。
    if let Ok(value) = serde_json::from_slice::<Value>(payload) {
        let message = value
            .pointer("/message")
            .or_else(|| value.pointer("/error"))
            .and_then(|v| v.as_str())
            .unwrap_or("火山引擎返回未知错误")
            .to_string();
        let code = value
            .pointer("/code")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        return (code, message);
    }
    (0, String::from_utf8_lossy(payload).into_owned())
}

/// 组装 corpus：优先使用平台热词表；未配置时用请求级热词直传
/// （双向流式上限 100 tokens，按累计字符数保守截断）。替换词不走云端。
fn volc_corpus(boosting_table_id: &str, hotwords: &[String]) -> Value {
    let mut corpus = serde_json::Map::new();
    let table_id = boosting_table_id.trim();
    if !table_id.is_empty() {
        corpus.insert("boosting_table_id".into(), json!(table_id));
        return Value::Object(corpus);
    }

    let mut words: Vec<Value> = Vec::new();
    let mut total_chars = 0usize;
    for word in hotwords {
        // 词典可能在旧引擎规范下保存过，直传前再归一化并按火山规范过滤。
        let word = crate::hotwords::normalize_volc_hotword(word);
        if !crate::hotwords::is_valid_volc_hotword(&word) {
            continue;
        }
        let chars = word.chars().count();
        if total_chars + chars > VOLC_INLINE_HOTWORD_MAX_CHARS {
            break;
        }
        total_chars += chars;
        words.push(json!({ "word": word }));
    }
    if !words.is_empty() {
        let context = json!({ "hotwords": words }).to_string();
        corpus.insert("context".into(), json!(context));
    }
    Value::Object(corpus)
}

/// select! 辅助：到点返回；未设置 deadline 时永久 pending。
async fn sleep_until_opt(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending::<()>().await,
    }
}

pub struct RealtimeSession {
    audio_tx: mpsc::Sender<Vec<u8>>,
    finish_tx: Option<oneshot::Sender<()>>,
    result_rx: Option<oneshot::Receiver<Result<String, AsrError>>>,
}

impl RealtimeSession {
    pub async fn connect(
        config: VolcAsrConfig,
        semantic_punctuation_enabled: bool,
        max_sentence_silence_ms: u32,
        hotwords: Vec<String>,
        mut on_update: impl FnMut(TranscriptUpdate) + Send + 'static,
    ) -> Result<Self, AsrError> {
        let api_key = config.api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(AsrError::Message(
                "尚未配置豆包录音识别 APP Key，请先在设置里保存。".into(),
            ));
        }
        let resource_id = if config.resource_id.trim().is_empty() {
            VOLC_DEFAULT_RESOURCE_ID.to_string()
        } else {
            config.resource_id.trim().to_string()
        };

        // 组装识别参数；热词：平台热词表优先，否则请求级直传。替换词仅本地后处理。
        let corpus = volc_corpus(&config.boosting_table_id, &hotwords);
        let mut request = json!({
            "model_name": "bigmodel",
            // 流式 + 非流式二遍识别：实时上屏，最终结果更准。
            "enable_nonstream": true,
            "enable_itn": true,
            "enable_punc": semantic_punctuation_enabled,
            // 保持 JackVoice 原则：不做“语义顺滑”润色。
            "enable_ddc": false,
            "ssd_version": "200",
            "show_utterances": true,
            "result_type": "full",
            "end_window_size": max_sentence_silence_ms.max(200),
            "force_to_speech_time": 1000,
        });
        if !corpus.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            request["corpus"] = corpus;
        }

        let full_client_request = json!({
            "user": {
                "uid": "jackvoice-desktop",
                "did": "jackvoice",
                "platform": std::env::consts::OS,
                "sdk_version": "0.1",
                "app_version": "0.1.0"
            },
            "audio": {
                "format": "pcm",
                "codec": "raw",
                "rate": SAMPLE_RATE,
                "bits": 16,
                "channel": 1
            },
            "request": request
        });

        let mut ws_request = VOLC_WS_URL
            .into_client_request()
            .map_err(|e| AsrError::Message(format!("无法创建火山引擎 WebSocket 请求：{e}")))?;
        {
            let headers = ws_request.headers_mut();
            let connect_id = Uuid::new_v4().to_string().to_lowercase();
            headers.insert(
                "X-Api-Key",
                HeaderValue::from_str(&api_key)
                    .map_err(|e| AsrError::Message(format!("火山 API Key 无效：{e}")))?,
            );
            headers.insert(
                "X-Api-Resource-Id",
                HeaderValue::from_str(&resource_id)
                    .map_err(|e| AsrError::Message(format!("火山资源 ID 无效：{e}")))?,
            );
            headers.insert(
                "X-Api-Request-Id",
                HeaderValue::from_str(&Uuid::new_v4().to_string().to_lowercase())
                    .map_err(|e| AsrError::Message(format!("生成请求 ID 失败：{e}")))?,
            );
            headers.insert(
                "X-Api-Connect-Id",
                HeaderValue::from_str(&connect_id)
                    .map_err(|e| AsrError::Message(format!("生成连接 ID 失败：{e}")))?,
            );
            headers.insert(
                "X-Api-Sequence",
                HeaderValue::from_static("-1"),
            );
        }

        let (ws_stream, _) = connect_async(ws_request)
            .await
            .map_err(|e| AsrError::Message(format!("连接火山引擎实时识别失败：{e}")))?;
        let (mut write, mut read) = ws_stream.split();

        let start_frame = volc_frame(
            SAUC_MSG_FULL_CLIENT_REQUEST,
            SAUC_FLAG_NO_SEQUENCE,
            SAUC_SERIALIZATION_JSON,
            full_client_request.to_string().as_bytes(),
        );
        write
            .send(Message::Binary(start_frame.into()))
            .await
            .map_err(|e| AsrError::Message(format!("发送火山识别参数失败：{e}")))?;

        // 等待服务端第一帧响应，尽早暴露鉴权 / 参数错误。
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut decoder = SaucDecoder::new();
            while let Some(msg) = read.next().await {
                let msg = msg.map_err(|e| AsrError::Message(format!("读取火山引擎响应失败：{e}")))?;
                match msg {
                    Message::Binary(bytes) => {
                        decoder.push(&bytes);
                        for frame in decoder
                            .take_frames()
                            .map_err(|e| AsrError::Message(e))?
                        {
                            match frame {
                                SaucServerFrame::Response { .. } => return Ok(()),
                                SaucServerFrame::Error { code, message } => {
                                    return Err(AsrError::Message(format!(
                                        "火山引擎识别启动失败（{code}）：{message}"
                                    )));
                                }
                                SaucServerFrame::Ignored => {}
                            }
                        }
                    }
                    Message::Close(_) => {
                        return Err(AsrError::Message(
                            "火山引擎连接在启动前被关闭。".into(),
                        ));
                    }
                    _ => {}
                }
            }
            Err(AsrError::Message(
                "火山引擎连接在启动前意外结束。".into(),
            ))
        })
        .await
        .map_err(|_| AsrError::Message("火山引擎实时识别连接超时。".into()))??;

        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (finish_tx, mut finish_rx) = oneshot::channel::<()>();
        let (finish_audio_tx, finish_audio_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<Result<String, AsrError>>();

        // Writer task: stream audio, then send the final (negative) packet and
        // notify the reader that endpointing was requested.
        tokio::spawn(async move {
            let mut finished = false;
            let mut finish_audio_tx = Some(finish_audio_tx);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut finish_rx, if !finished => {
                        finished = true;
                        // 火山协议以“负包”表示音频结束，payload 为空。
                        let final_packet = volc_frame(
                            SAUC_MSG_AUDIO_ONLY_REQUEST,
                            SAUC_FLAG_FINAL_PACKET,
                            SAUC_SERIALIZATION_RAW,
                            &[],
                        );
                        let _ = write.send(Message::Binary(final_packet.into())).await;
                        if let Some(tx) = finish_audio_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    maybe_audio = audio_rx.recv(), if !finished => {
                        match maybe_audio {
                            Some(bytes) => {
                                let frame = volc_frame(
                                    SAUC_MSG_AUDIO_ONLY_REQUEST,
                                    SAUC_FLAG_NO_SEQUENCE,
                                    SAUC_SERIALIZATION_RAW,
                                    &bytes,
                                );
                                if write.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => {
                                // Capture side dropped without explicit finish.
                                finished = true;
                                let final_packet = volc_frame(
                                    SAUC_MSG_AUDIO_ONLY_REQUEST,
                                    SAUC_FLAG_FINAL_PACKET,
                                    SAUC_SERIALIZATION_RAW,
                                    &[],
                                );
                                let _ = write.send(Message::Binary(final_packet.into())).await;
                                if let Some(tx) = finish_audio_tx.take() {
                                    let _ = tx.send(());
                                }
                            }
                        }
                    }
                }

                if finished {
                    break;
                }
            }
        });

        // Reader task: accumulate the latest full transcript until close.
        tokio::spawn(async move {
            let mut decoder = SaucDecoder::new();
            let mut last_text = String::new();
            let mut terminal: Option<Result<String, AsrError>> = None;
            // 负包已发出：服务端会做二遍识别并返回 definite 分句，
            // 但不会主动关闭连接，需要本地判断收尾窗口。
            let mut finish_signaled = false;
            let mut definite_idle: Option<std::time::Instant> = None;
            let mut force_deadline: Option<std::time::Instant> = None;
            // oneshot Receiver 消费后不能再 poll（会 panic），置空以退出 select。
            let mut finish_audio_rx = Some(finish_audio_rx);

            'reader_loop: loop {
                tokio::select! {
                    msg = read.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            Some(Err(e)) => {
                                terminal = Some(Err(AsrError::Message(format!(
                                    "火山引擎连接中断：{e}"
                                ))));
                                break 'reader_loop;
                            }
                            None => break 'reader_loop,
                        };
                        match msg {
                            Message::Binary(bytes) => {
                                decoder.push(&bytes);
                                let frames = match decoder.take_frames() {
                                    Ok(frames) => frames,
                                    Err(e) => {
                                        terminal = Some(Err(AsrError::Message(e)));
                                        break;
                                    }
                                };
                                for frame in frames {
                                    match frame {
                                        SaucServerFrame::Response {
                                            payload, final_flag, ..
                                        } => {
                                            let value: Value =
                                                match serde_json::from_slice(&payload) {
                                                    Ok(v) => v,
                                                    Err(e) => {
                                                        terminal = Some(Err(AsrError::Message(
                                                            format!("解析火山引擎响应失败：{e}"),
                                                        )));
                                                        break 'reader_loop;
                                                    }
                                                };
                                            let text = value
                                                .pointer("/result/text")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            if !text.is_empty() {
                                                last_text = text.clone();
                                            }
                                            let has_definite = value
                                                .pointer("/result/utterances")
                                                .and_then(|v| v.as_array())
                                                .map(|utterances| {
                                                    utterances.iter().any(|u| {
                                                        u.get("definite")
                                                            .and_then(|d| d.as_bool())
                                                            .unwrap_or(false)
                                                            && u.get("text")
                                                                .and_then(|t| t.as_str())
                                                                .map(|t| !t.is_empty())
                                                                .unwrap_or(false)
                                                    })
                                                })
                                                .unwrap_or(false);
                                            on_update(TranscriptUpdate {
                                                text: normalize::normalize(&last_text),
                                                is_final_sentence: has_definite,
                                            });
                                            // 负包后的 definite 响应即二遍最终结果，
                                            // 800ms 内无新响应即可收尾；非 definite
                                            // 响应（如极短音频）用 2.5s 稳定窗口，
                                            // 避免纯静音场景挂满兜底超时。
                                            if finish_signaled {
                                                let window_ms = if has_definite { 800 } else { 2500 };
                                                definite_idle = Some(
                                                    std::time::Instant::now()
                                                        + Duration::from_millis(window_ms),
                                                );
                                            }
                                            // 服务端负序号帧即最终结果，直接收尾。
                                            if finish_signaled && final_flag {
                                                terminal =
                                                    Some(Ok(normalize::normalize(&last_text)));
                                                break 'reader_loop;
                                            }
                                        }
                                        SaucServerFrame::Error { code, message } => {
                                            terminal = Some(Err(AsrError::Message(format!(
                                                "火山引擎实时识别失败（{code}）：{message}"
                                            ))));
                                            break 'reader_loop;
                                        }
                                        SaucServerFrame::Ignored => {}
                                    }
                                }
                            }
                            Message::Close(_) => {
                                break 'reader_loop;
                            }
                            _ => {}
                        }
                    }
                    _ = sleep_until_opt(definite_idle), if definite_idle.is_some() => {
                        // 最终 definite 结果已稳定，收尾。
                        terminal = Some(Ok(normalize::normalize(&last_text)));
                        break 'reader_loop;
                    }
                    _ = sleep_until_opt(force_deadline), if force_deadline.is_some() => {
                        // 兜底：负包后始终没有 definite 响应（如纯静音），
                        // 以最后一次文本收尾，避免挂死等待连接关闭。
                        terminal = Some(Ok(normalize::normalize(&last_text)));
                        break 'reader_loop;
                    }
                    _ = async {
                        if let Some(rx) = finish_audio_rx.as_mut() {
                            let _ = rx.await;
                        }
                    }, if finish_audio_rx.is_some() => {
                        finish_signaled = true;
                        force_deadline =
                            Some(std::time::Instant::now() + Duration::from_secs(6));
                        finish_audio_rx = None;
                    }
                }
            }

            if terminal.is_none() {
                terminal = Some(Ok(normalize::normalize(&last_text)));
            }
            let _ = result_tx.send(terminal.unwrap_or_else(|| {
                Err(AsrError::Message("火山引擎会话在返回最终结果前结束。".into()))
            }));
        });

        Ok(Self {
            audio_tx,
            finish_tx: Some(finish_tx),
            result_rx: Some(result_rx),
        })
    }

    pub async fn send_audio(&self, pcm: Vec<u8>) -> Result<(), AsrError> {
        self.audio_tx
            .send(pcm)
            .await
            .map_err(|_| AsrError::Message("识别会话已关闭，无法继续发送音频。".into()))
    }

    pub async fn finish(mut self) -> Result<String, AsrError> {
        if let Some(tx) = self.finish_tx.take() {
            let _ = tx.send(());
        }
        drop(self.audio_tx);

        let rx = self
            .result_rx
            .take()
            .ok_or_else(|| AsrError::Message("最终结果接收器不可用。".into()))?;

        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AsrError::Message("等待最终结果时会话已断开。".into())),
            Err(_) => Err(AsrError::Message("火山引擎实时识别收尾超时。".into())),
        }
    }

    pub async fn cancel(mut self) {
        if let Some(tx) = self.finish_tx.take() {
            let _ = tx.send(());
        }
        drop(self.audio_tx);
        if let Some(rx) = self.result_rx.take() {
            let _ = tokio::time::timeout(Duration::from_millis(300), rx).await;
        }
    }
}

#[cfg(test)]
mod volc_protocol_tests {
    use super::*;

    fn server_response_frame(flags: u8, sequence: i32, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(0b0001_0001);
        frame.push((SAUC_MSG_SERVER_FULL_RESPONSE << 4) | flags);
        frame.push((SAUC_SERIALIZATION_JSON << 4) | SAUC_COMPRESSION_NONE);
        frame.push(0x00);
        frame.extend_from_slice(&sequence.to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn client_frames_have_correct_headers() {
        let start = volc_frame(
            SAUC_MSG_FULL_CLIENT_REQUEST,
            SAUC_FLAG_NO_SEQUENCE,
            SAUC_SERIALIZATION_JSON,
            b"{}",
        );
        assert_eq!(start[0], 0b0001_0001);
        assert_eq!(start[1], SAUC_MSG_FULL_CLIENT_REQUEST << 4);
        assert_eq!(start[2], SAUC_SERIALIZATION_JSON << 4);
        assert_eq!(
            u32::from_be_bytes(start[4..8].try_into().unwrap()),
            2
        );
        assert_eq!(&start[8..], b"{}");

        let audio = volc_frame(
            SAUC_MSG_AUDIO_ONLY_REQUEST,
            SAUC_FLAG_NO_SEQUENCE,
            SAUC_SERIALIZATION_RAW,
            &[0u8; 3200],
        );
        assert_eq!(audio[1], SAUC_MSG_AUDIO_ONLY_REQUEST << 4);
        assert_eq!(audio[2], SAUC_SERIALIZATION_RAW << 4);
        assert_eq!(
            u32::from_be_bytes(audio[4..8].try_into().unwrap()),
            3200
        );

        let last = volc_frame(
            SAUC_MSG_AUDIO_ONLY_REQUEST,
            SAUC_FLAG_FINAL_PACKET,
            SAUC_SERIALIZATION_RAW,
            &[],
        );
        assert_eq!(last[1], (SAUC_MSG_AUDIO_ONLY_REQUEST << 4) | 0b0010);
        assert_eq!(u32::from_be_bytes(last[4..8].try_into().unwrap()), 0);
    }

    #[test]
    fn decoder_handles_positive_and_negative_sequence_responses() {
        let mut decoder = SaucDecoder::new();
        let payload = br#"{"result":{"text":"hello"}}"#;
        decoder.push(&server_response_frame(0b0001, 1, payload));

        let frames = decoder.take_frames().unwrap();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SaucServerFrame::Response {
                sequence,
                final_flag,
                payload: decoded,
            } => {
                assert_eq!(*sequence, 1);
                assert!(!final_flag);
                assert_eq!(decoded, payload);
            }
            _ => panic!("expected response frame"),
        }

        let mut decoder = SaucDecoder::new();
        decoder.push(&server_response_frame(0b0011, -1, payload));
        let frames = decoder.take_frames().unwrap();
        match &frames[0] {
            SaucServerFrame::Response { final_flag, .. } => assert!(final_flag),
            _ => panic!("expected response frame"),
        }
    }

    #[test]
    fn decoder_splits_coalesced_frames() {
        let mut decoder = SaucDecoder::new();
        let payload = br#"{"result":{"text":"first sentence"}}"#;
        let mut joined = server_response_frame(0b0001, 1, payload);
        joined.extend_from_slice(&server_response_frame(0b0011, -1, payload));
        decoder.push(&joined);

        let frames = decoder.take_frames().unwrap();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[1], SaucServerFrame::Response { final_flag: true, .. }));
    }

    #[test]
    fn decoder_handles_split_payloads() {
        let mut decoder = SaucDecoder::new();
        let payload = br#"{"result":{"text":"arrived in parts"}}"#;
        let frame = server_response_frame(0b0001, 3, payload);
        decoder.push(&frame[..6]);
        assert!(decoder.take_frames().unwrap().is_empty());
        decoder.push(&frame[6..]);
        let frames = decoder.take_frames().unwrap();
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn error_frame_follows_official_layout() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&45000001u32.to_be_bytes());
        let message = "invalid params";
        payload.extend_from_slice(&(message.len() as u32).to_be_bytes());
        payload.extend_from_slice(message.as_bytes());

        let (code, text) = parse_volc_error_payload(&payload);
        assert_eq!(code, 45000001);
        assert_eq!(text, "invalid params");
    }

    #[test]
    fn error_frame_tolerates_json_payload_layout() {
        let payload = br#"{"code":45000001,"message":"bad request"}"#;
        let (code, text) = parse_volc_error_payload(payload);
        assert_eq!(code, 45000001);
        assert_eq!(text, "bad request");
    }

    #[test]
    fn inline_hotwords_respect_char_budget() {
        let words = vec!["一二三四五".to_string(), "六七八九十".to_string()];
        let corpus = volc_corpus("", &words);
        let context = corpus["context"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(context).unwrap();
        let hotwords = parsed["hotwords"].as_array().unwrap();
        // 第一条 5 字 + 第二条 5 字 = 10 字 <= 100，两条都应包含。
        assert_eq!(hotwords.len(), 2);
    }

    #[test]
    fn inline_hotwords_drop_invalid_and_truncate() {
        let words = vec![
            "标点，词".to_string(),                    // 标点剥离后变成“标点词”
            "B roll".to_string(),                     // 空格会被吞掉
            "合法词".to_string(),
        ];
        let corpus = volc_corpus("", &words);
        let context = corpus["context"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(context).unwrap();
        let hotwords = parsed["hotwords"].as_array().unwrap();
        assert_eq!(hotwords.len(), 3);
        assert_eq!(hotwords[0]["word"], "标点词");
        assert_eq!(hotwords[1]["word"], "Broll");
        assert_eq!(hotwords[2]["word"], "合法词");
    }

    #[test]
    fn boosting_table_takes_priority_over_inline() {
        let words = vec!["热词".to_string()];
        let corpus = volc_corpus("table-123", &words);
        assert_eq!(corpus["boosting_table_id"], "table-123");
        assert!(corpus.get("context").is_none());
    }

}
