use crate::storage;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub const AUDIO_SAMPLE_RATE_HZ: u32 = 16_000;
pub const AUDIO_CHANNELS: u16 = 1;
pub const AUDIO_BITS_PER_SAMPLE: u16 = 16;

const WAV_HEADER_BYTES: u64 = 44;
const AUDIO_DIR_NAME: &str = "audio";
const MAX_RECORDS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioArtifact {
    /// 文件名始终由应用生成且不包含目录。
    pub file_name: String,
    pub mime_type: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// 实际写入本地文件的语音时长。
    pub duration_ms: u64,
    /// 完整 WAV 文件大小（包含 44 字节头）。
    pub size_bytes: u64,
}

/// 保存一次识别所使用的参数快照，便于将来回放同一音频时复现实验基线。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecognitionContext {
    #[serde(default)]
    pub hotwords: Vec<String>,
    pub semantic_punctuation_enabled: bool,
    #[serde(default)]
    pub semantic_smoothing_enabled: bool,
    pub max_sentence_silence_ms: u32,
    pub input_gain_db: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub input_device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecord {
    pub id: String,
    pub finished_at_ms: i64,
    pub text: String,
    pub duration_ms: u64,
    pub char_count: u32,
    /// 旧版本历史记录没有音频，反序列化时保持兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recognition: Option<RecognitionContext>,
    /// completed / noSpeech / failed。旧记录默认视为已完成。
    #[serde(default = "default_recognition_status")]
    pub recognition_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recognition_error: String,
    /// 本地录音链路异常时记录原因；此时关联 WAV 可能只包含异常前的音频。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recording_error: String,
    /// 用户可能在 Finder 中自行删除 WAV；文字和其他元数据仍然保留。
    #[serde(default)]
    pub audio_missing: bool,
}

fn default_recognition_status() -> String {
    "completed".into()
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStats {
    pub sessions: u64,
    pub total_duration_ms: u64,
    pub total_chars: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryData {
    pub records: Vec<HistoryRecord>,
    pub stats: HistoryStats,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct HistoryFile {
    #[serde(default)]
    records: Vec<HistoryRecord>,
}

fn history_path(dir: &Path) -> PathBuf {
    dir.join("history.json")
}

fn audio_dir(dir: &Path) -> PathBuf {
    dir.join(AUDIO_DIR_NAME)
}

fn load_records(dir: &Path) -> Vec<HistoryRecord> {
    let mut records = fs::read_to_string(history_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<HistoryFile>(&raw).ok())
        .map(|f| f.records)
        .unwrap_or_default();
    for record in &mut records {
        record.audio_missing = record
            .audio
            .as_ref()
            .and_then(|audio| audio_file_path(dir, &audio.file_name).ok())
            .is_some_and(|path| !path.is_file());
    }
    records
}

fn compute_stats(records: &[HistoryRecord]) -> HistoryStats {
    let mut stats = HistoryStats::default();
    for record in records {
        stats.sessions += 1;
        stats.total_duration_ms += record.duration_ms;
        stats.total_chars += record.char_count as u64;
    }
    stats
}

pub fn load(dir: &Path) -> HistoryData {
    let mut records = load_records(dir);
    records.sort_by(|a, b| b.finished_at_ms.cmp(&a.finished_at_ms));
    let stats = compute_stats(&records);
    HistoryData { records, stats }
}

/// 追加一条听写记录。即使没有识别文字，只要有本地录音也必须保留记录。
/// 首页索引仍裁剪到上限，但 WAV 永远不由应用自动删除。
pub struct HistoryAppend<'a> {
    pub id: String,
    pub text: &'a str,
    pub duration_ms: u64,
    pub audio: Option<AudioArtifact>,
    pub recognition: Option<RecognitionContext>,
    pub recording_error: Option<String>,
    pub recognition_error: Option<String>,
}

pub fn append(dir: &Path, entry: HistoryAppend<'_>) -> Result<(), String> {
    let trimmed = entry.text.trim();
    if trimmed.is_empty() && entry.audio.is_none() {
        return Ok(());
    }
    let recognition_error = entry.recognition_error.unwrap_or_default();
    let recognition_status = if !recognition_error.is_empty() {
        "failed"
    } else if trimmed.is_empty() {
        "noSpeech"
    } else {
        "completed"
    };

    let mut records = load_records(dir);
    records.push(HistoryRecord {
        id: entry.id,
        finished_at_ms: chrono::Utc::now().timestamp_millis(),
        text: trimmed.to_string(),
        duration_ms: entry.duration_ms,
        char_count: trimmed.chars().filter(|c| !c.is_whitespace()).count() as u32,
        audio: entry.audio,
        recognition: entry.recognition,
        recognition_status: recognition_status.into(),
        recognition_error,
        recording_error: entry.recording_error.unwrap_or_default(),
        audio_missing: false,
    });

    if records.len() > MAX_RECORDS {
        let excess = records.len() - MAX_RECORDS;
        records.drain(0..excess);
    }

    persist_records(dir, &records)
}

fn persist_records(dir: &Path, records: &[HistoryRecord]) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(&HistoryFile {
        records: records.to_vec(),
    })
    .map_err(|e| format!("序列化历史记录失败：{e}"))?;
    storage::write_atomic(&history_path(dir), &raw, false)
}

fn remove_history_backup(dir: &Path) -> Result<(), String> {
    let backup = history_path(dir).with_extension("json.bak");
    match fs::remove_file(backup) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("清理历史记录备份失败：{error}")),
    }
}

pub fn delete_record(dir: &Path, record_id: &str) -> Result<(), String> {
    let mut records = load_records(dir);
    let index = records
        .iter()
        .position(|record| record.id == record_id)
        .ok_or_else(|| "找不到这条历史记录。".to_string())?;
    records.remove(index);
    persist_records(dir, &records)?;
    remove_history_backup(dir)?;
    Ok(())
}

pub fn clear(dir: &Path) -> Result<(), String> {
    persist_records(dir, &[])?;
    remove_history_backup(dir)
}

pub fn ensure_audio_dir(dir: &Path) -> Result<PathBuf, String> {
    let path = audio_dir(dir);
    fs::create_dir_all(&path).map_err(|e| format!("创建录音文件夹失败：{e}"))?;
    Ok(path)
}

/// 将异常退出遗留、已经包含 PCM 的临时 WAV 修复为可播放文件。
/// 没有任何音频的 `.part` 会保留供诊断，不做静默删除。
pub fn recover_partial_audio_files(dir: &Path) -> Result<usize, String> {
    let directory = ensure_audio_dir(dir)?;
    let mut recovered = 0;
    for entry in fs::read_dir(&directory).map_err(|e| format!("读取录音文件夹失败：{e}"))?
    {
        let entry = entry.map_err(|e| format!("读取临时录音失败：{e}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with('.') || !name.ends_with(".wav.part") {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|e| format!("读取临时录音大小失败：{e}"))?;
        if metadata.len() <= WAV_HEADER_BYTES {
            continue;
        }
        let pcm_bytes = metadata.len() - WAV_HEADER_BYTES;
        if pcm_bytes > (u32::MAX - 36) as u64 || pcm_bytes & 1 != 0 {
            continue;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path())
            .map_err(|e| format!("打开临时录音失败：{e}"))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| format!("定位临时录音失败：{e}"))?;
        file.write_all(&wav_header(pcm_bytes as u32))
            .map_err(|e| format!("修复临时录音文件头失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("同步修复录音失败：{e}"))?;
        drop(file);

        let final_name = name
            .strip_prefix('.')
            .and_then(|value| value.strip_suffix(".part"))
            .ok_or_else(|| "临时录音文件名无效。".to_string())?;
        let final_path = directory.join(final_name);
        if !final_path.exists() {
            fs::rename(entry.path(), final_path).map_err(|e| format!("提交修复录音失败：{e}"))?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// 根据历史记录 id 解析音频路径。调用方不能提供任意文件名。
pub fn audio_path_for_record(dir: &Path, record_id: &str) -> Result<PathBuf, String> {
    let record = load_records(dir)
        .into_iter()
        .find(|record| record.id == record_id)
        .ok_or_else(|| "找不到这条历史记录。".to_string())?;
    let audio = record
        .audio
        .ok_or_else(|| "这条旧记录没有保存音频。".to_string())?;
    let path = audio_file_path(dir, &audio.file_name)?;
    if !path.is_file() {
        return Err("这条记录的音频文件不存在。".into());
    }
    Ok(path)
}

pub fn read_audio(dir: &Path, record_id: &str) -> Result<Vec<u8>, String> {
    let path = audio_path_for_record(dir, record_id)?;
    fs::read(path).map_err(|e| format!("读取音频失败：{e}"))
}

fn audio_file_path(dir: &Path, file_name: &str) -> Result<PathBuf, String> {
    let path = Path::new(file_name);
    let mut components = path.components();
    let is_plain_file_name =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !is_plain_file_name || !file_name.ends_with(".wav") {
        return Err("音频文件名无效。".into());
    }
    Ok(audio_dir(dir).join(path))
}

/// 将实际用于识别的 PCM16 数据流式写入临时 WAV，避免长录音常驻内存。
/// 正常听写无论识别是否成功都会调用 `finish`；只有用户明确取消或本地
/// 录音本身失败时，Drop 才清理当前进程里的临时文件。
pub struct AudioRecorder {
    writer: Option<BufWriter<File>>,
    temp_path: PathBuf,
    final_path: PathBuf,
    file_name: String,
    pcm_bytes: u64,
    bytes_since_flush: u64,
    committed: bool,
    preserve_partial_on_drop: bool,
}

impl AudioRecorder {
    pub fn create(dir: &Path, record_id: &str) -> Result<Self, String> {
        let directory = ensure_audio_dir(dir)?;
        let short_id = record_id.chars().take(8).collect::<String>();
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let file_name = format!("{timestamp}_{short_id}.wav");
        let final_path = audio_file_path(dir, &file_name)?;
        let temp_path = directory.join(format!(".{file_name}.part"));
        let file = File::create(&temp_path).map_err(|e| format!("创建音频文件失败：{e}"))?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(&[0_u8; WAV_HEADER_BYTES as usize])
            .map_err(|e| format!("初始化 WAV 文件失败：{e}"))?;
        Ok(Self {
            writer: Some(writer),
            temp_path,
            final_path,
            file_name,
            pcm_bytes: 0,
            bytes_since_flush: 0,
            committed: false,
            preserve_partial_on_drop: false,
        })
    }

    pub fn write_pcm(&mut self, pcm: &[u8]) -> Result<(), String> {
        if pcm.len() & 1 != 0 {
            return Err("PCM16 音频长度不是偶数。".into());
        }
        let next_len = self
            .pcm_bytes
            .checked_add(pcm.len() as u64)
            .ok_or_else(|| "音频文件过大。".to_string())?;
        if next_len > (u32::MAX - 36) as u64 {
            return Err("单次录音超过 WAV 格式的 4 GiB 上限。".into());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| "录音文件已经关闭。".to_string())?;
        writer
            .write_all(pcm)
            .map_err(|e| format!("写入录音失败：{e}"))?;
        self.pcm_bytes = next_len;
        // 捕获到任何有效 PCM 后，异常退出也必须保留临时文件供下次恢复。
        self.preserve_partial_on_drop = true;
        self.bytes_since_flush += pcm.len() as u64;
        // 最多约 100 ms 音频留在用户态缓冲区，异常退出后也能尽量恢复完整。
        if self.bytes_since_flush >= 3_200 {
            writer
                .flush()
                .map_err(|e| format!("刷新录音缓冲失败：{e}"))?;
            self.bytes_since_flush = 0;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<AudioArtifact, String> {
        if self.pcm_bytes == 0 {
            return Err("本次录音没有有效音频。".into());
        }
        // 从这里开始任何失败都不应销毁已经捕获的 PCM。下次启动时会
        // 修复并提交这个 `.part`，供用户自行检查或删除。
        self.preserve_partial_on_drop = true;

        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| "录音文件已经关闭。".to_string())?;
        writer
            .flush()
            .map_err(|e| format!("刷新音频文件失败：{e}"))?;
        let header = wav_header(self.pcm_bytes as u32);
        let file = writer.get_mut();
        file.seek(SeekFrom::Start(0))
            .map_err(|e| format!("定位 WAV 文件头失败：{e}"))?;
        file.write_all(&header)
            .map_err(|e| format!("写入 WAV 文件头失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("同步 WAV 文件失败：{e}"))?;
        drop(writer);
        fs::rename(&self.temp_path, &self.final_path)
            .map_err(|e| format!("提交录音文件失败：{e}"))?;
        self.committed = true;

        let bytes_per_second = AUDIO_SAMPLE_RATE_HZ as u64
            * AUDIO_CHANNELS as u64
            * (AUDIO_BITS_PER_SAMPLE as u64 / 8);
        Ok(AudioArtifact {
            file_name: self.file_name.clone(),
            mime_type: "audio/wav".into(),
            sample_rate_hz: AUDIO_SAMPLE_RATE_HZ,
            channels: AUDIO_CHANNELS,
            bits_per_sample: AUDIO_BITS_PER_SAMPLE,
            duration_ms: self.pcm_bytes.saturating_mul(1000) / bytes_per_second,
            size_bytes: self.pcm_bytes + WAV_HEADER_BYTES,
        })
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        if !self.committed && !self.preserve_partial_on_drop {
            // Windows 不能删除仍被打开的文件，先关闭 BufWriter。
            self.writer.take();
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

fn wav_header(pcm_bytes: u32) -> [u8; WAV_HEADER_BYTES as usize] {
    let byte_rate = AUDIO_SAMPLE_RATE_HZ * AUDIO_CHANNELS as u32 * AUDIO_BITS_PER_SAMPLE as u32 / 8;
    let block_align = AUDIO_CHANNELS * AUDIO_BITS_PER_SAMPLE / 8;
    let mut header = [0_u8; WAV_HEADER_BYTES as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36_u32 + pcm_bytes).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&AUDIO_CHANNELS.to_le_bytes());
    header[24..28].copy_from_slice(&AUDIO_SAMPLE_RATE_HZ.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&AUDIO_BITS_PER_SAMPLE.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&pcm_bytes.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("jackvoice-history-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn writes_playable_pcm16_wav_and_reads_it_by_record_id() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let pcm = [0x34_u8, 0x12_u8].repeat(160);
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&pcm).unwrap();
        let artifact = recorder.finish().unwrap();

        append(
            &dir,
            HistoryAppend {
                id: id.clone(),
                text: "测试",
                duration_ms: 10,
                audio: Some(artifact.clone()),
                recognition: None,
                recording_error: None,
                recognition_error: None,
            },
        )
        .unwrap();
        let wav = read_audio(&dir, &id).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            pcm.len() as u32
        );
        assert_eq!(&wav[44..], pcm.as_slice());
        assert_eq!(artifact.duration_ms, 10);
        assert_eq!(artifact.size_bytes, (pcm.len() + 44) as u64);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_text_only_history_remains_compatible() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            history_path(&dir),
            r#"{"records":[{"id":"old","finishedAtMs":1,"text":"旧记录","durationMs":50,"charCount":3}]}"#,
        )
        .unwrap();

        let data = load(&dir);
        assert_eq!(data.records.len(), 1);
        assert!(data.records[0].audio.is_none());
        assert!(data.records[0].recognition.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_recognition_engine_is_ignored_and_not_reserialized() {
        let raw = r#"{
            "id":"old-context",
            "finishedAtMs":1,
            "text":"旧识别记录",
            "durationMs":50,
            "charCount":5,
            "recognition":{
                "engine":"volc-seedasr-streaming",
                "hotwords":[],
                "semanticPunctuationEnabled":true,
                "maxSentenceSilenceMs":800,
                "inputGainDb":0,
                "inputDeviceId":""
            }
        }"#;

        let record: HistoryRecord = serde_json::from_str(raw).unwrap();
        assert!(record.recognition.is_some());
        let serialized = serde_json::to_value(record).unwrap();
        assert!(serialized["recognition"].get("engine").is_none());
    }

    #[test]
    fn interrupted_recording_with_pcm_preserves_recoverable_partial_file() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let partial;
        {
            let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
            recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
            partial = recorder.temp_path.clone();
            assert!(partial.exists());
        }
        assert!(partial.exists());
        assert_eq!(recover_partial_audio_files(&dir).unwrap(), 1);
        assert!(dir
            .join(AUDIO_DIR_NAME)
            .join(
                partial
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .trim_start_matches('.')
                    .trim_end_matches(".part")
            )
            .exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_record_keeps_audio_but_removes_text_and_backup() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        let audio_path = audio_file_path(&dir, &artifact.file_name).unwrap();
        append(
            &dir,
            HistoryAppend {
                id: id.clone(),
                text: "可删除",
                duration_ms: 1,
                audio: Some(artifact),
                recognition: None,
                recording_error: None,
                recognition_error: None,
            },
        )
        .unwrap();
        fs::write(history_path(&dir).with_extension("json.bak"), "private").unwrap();

        delete_record(&dir, &id).unwrap();

        assert!(load(&dir).records.is_empty());
        assert!(audio_path.exists());
        assert!(!history_path(&dir).with_extension("json.bak").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_only_failed_recognition_is_kept() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        append(
            &dir,
            HistoryAppend {
                id,
                text: "",
                duration_ms: 1,
                audio: Some(artifact),
                recognition: None,
                recording_error: None,
                recognition_error: Some("没有网络".into()),
            },
        )
        .unwrap();

        let records = load(&dir).records;
        assert_eq!(records.len(), 1);
        assert!(records[0].text.is_empty());
        assert_eq!(records[0].recognition_status, "failed");
        assert_eq!(records[0].recognition_error, "没有网络");
        assert!(records[0].audio.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_recording_failure_is_preserved_in_history() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        append(
            &dir,
            HistoryAppend {
                id,
                text: "",
                duration_ms: 1,
                audio: Some(artifact),
                recognition: None,
                recording_error: Some("录音缓冲区已满".into()),
                recognition_error: None,
            },
        )
        .unwrap();

        let records = load(&dir).records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].recording_error, "录音缓冲区已满");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_audio_is_reported_without_removing_history() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        let audio_path = audio_file_path(&dir, &artifact.file_name).unwrap();
        append(
            &dir,
            HistoryAppend {
                id,
                text: "用户自行删除录音",
                duration_ms: 1,
                audio: Some(artifact),
                recognition: None,
                recording_error: None,
                recognition_error: None,
            },
        )
        .unwrap();
        fs::remove_file(audio_path).unwrap();

        let records = load(&dir).records;
        assert_eq!(records.len(), 1);
        assert!(records[0].audio_missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_history_keeps_all_audio() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        let audio_path = audio_file_path(&dir, &artifact.file_name).unwrap();
        append(
            &dir,
            HistoryAppend {
                id,
                text: "清空",
                duration_ms: 1,
                audio: Some(artifact),
                recognition: None,
                recording_error: None,
                recognition_error: None,
            },
        )
        .unwrap();

        clear(&dir).unwrap();

        assert!(load(&dir).records.is_empty());
        assert!(audio_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovers_pcm_from_an_interrupted_partial_wav() {
        let dir = temp_dir();
        let directory = ensure_audio_dir(&dir).unwrap();
        let partial = directory.join(".2026-08-10_12-00-00_deadbeef.wav.part");
        let pcm = [0_u8, 0, 1, 0];
        let mut bytes = vec![0_u8; WAV_HEADER_BYTES as usize];
        bytes.extend_from_slice(&pcm);
        fs::write(&partial, bytes).unwrap();

        assert_eq!(recover_partial_audio_files(&dir).unwrap(), 1);

        let recovered = directory.join("2026-08-10_12-00-00_deadbeef.wav");
        let wav = fs::read(recovered).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[44..], pcm.as_slice());
        let _ = fs::remove_dir_all(&dir);
    }
}
