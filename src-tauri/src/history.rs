use crate::storage;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
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
    /// 文件名始终是应用生成的 UUID，不包含目录。
    pub file_name: String,
    pub mime_type: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// 实际送入识别引擎的语音时长，不含服务端收尾静音。
    pub duration_ms: u64,
    /// 完整 WAV 文件大小（包含 44 字节头）。
    pub size_bytes: u64,
}

/// 保存一次识别所使用的参数快照，便于将来回放同一音频时复现实验基线。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecognitionContext {
    pub engine: String,
    #[serde(default)]
    pub hotwords: Vec<String>,
    pub semantic_punctuation_enabled: bool,
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
    fs::read_to_string(history_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<HistoryFile>(&raw).ok())
        .map(|f| f.records)
        .unwrap_or_default()
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

/// 追加一条听写记录（空文本忽略），并裁剪到上限。被裁剪记录对应的 WAV 也会一并删除。
pub fn append(
    dir: &Path,
    id: String,
    text: &str,
    duration_ms: u64,
    audio: Option<AudioArtifact>,
    recognition: Option<RecognitionContext>,
) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let mut records = load_records(dir);
    records.push(HistoryRecord {
        id,
        finished_at_ms: chrono::Utc::now().timestamp_millis(),
        text: trimmed.to_string(),
        duration_ms,
        char_count: trimmed.chars().filter(|c| !c.is_whitespace()).count() as u32,
        audio,
        recognition,
    });

    let removed = if records.len() > MAX_RECORDS {
        let excess = records.len() - MAX_RECORDS;
        records.drain(0..excess).collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    persist_records(dir, &records)?;

    // 先成功写入新索引，再删除被裁剪的旧文件；索引写失败时不冒险丢音频。
    for record in removed {
        if let Some(audio) = record.audio {
            if let Ok(path) = audio_file_path(dir, &audio.file_name) {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(())
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
    let removed = records.remove(index);
    if let Some(audio) = &removed.audio {
        if let Ok(path) = audio_file_path(dir, &audio.file_name) {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("删除历史录音失败：{error}")),
            }
        }
    }
    persist_records(dir, &records)?;
    remove_history_backup(dir)?;
    Ok(())
}

pub fn clear(dir: &Path) -> Result<(), String> {
    let audio = audio_dir(dir);
    match fs::remove_dir_all(audio) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("清空历史录音失败：{error}")),
    };
    persist_records(dir, &[])?;
    remove_history_backup(dir)
}

pub fn should_save_audio(retention: &str) -> bool {
    retention != "never"
}

pub fn apply_audio_retention(dir: &Path, retention: &str) -> Result<(), String> {
    if retention == "forever" {
        remove_partial_audio_files(dir)?;
        return remove_history_backup(dir);
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff_ms = match retention {
        "never" => i64::MAX,
        "sevenDays" => now_ms.saturating_sub(7 * 24 * 60 * 60 * 1000),
        _ => now_ms.saturating_sub(30 * 24 * 60 * 60 * 1000),
    };
    let mut records = load_records(dir);
    let mut changed = false;
    for record in &mut records {
        if record.finished_at_ms < cutoff_ms {
            changed |= record.audio.take().is_some();
        }
    }

    let retained_files = records
        .iter()
        .filter_map(|record| record.audio.as_ref().map(|audio| audio.file_name.as_str()))
        .collect::<HashSet<_>>();
    let audio_directory = audio_dir(dir);
    match fs::read_dir(&audio_directory) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| format!("读取本地录音目录失败：{error}"))?;
                if !entry
                    .file_type()
                    .map_err(|error| format!("读取本地录音类型失败：{error}"))?
                    .is_file()
                {
                    continue;
                }
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                let remove = retention == "never"
                    || file_name.ends_with(".part")
                    || !retained_files.contains(file_name.as_ref());
                if remove {
                    fs::remove_file(entry.path())
                        .map_err(|error| format!("清理本地录音失败：{error}"))?;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("读取本地录音目录失败：{error}")),
    }

    if changed {
        persist_records(dir, &records)?;
    }
    remove_history_backup(dir)
}

fn remove_partial_audio_files(dir: &Path) -> Result<(), String> {
    match fs::read_dir(audio_dir(dir)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| format!("读取本地录音目录失败：{error}"))?;
                if entry.file_name().to_string_lossy().ends_with(".part") {
                    fs::remove_file(entry.path())
                        .map_err(|error| format!("清理未完成录音失败：{error}"))?;
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("读取本地录音目录失败：{error}")),
    }
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

/// 将送入 ASR 的 PCM16 数据流式写入临时 WAV，避免长录音常驻内存。
/// 未调用 `finish`（取消、识别失败或进程内错误）时，Drop 会清理临时文件。
pub struct AudioRecorder {
    writer: Option<BufWriter<File>>,
    temp_path: PathBuf,
    final_path: PathBuf,
    file_name: String,
    pcm_bytes: u64,
    committed: bool,
}

impl AudioRecorder {
    pub fn create(dir: &Path, record_id: &str) -> Result<Self, String> {
        fs::create_dir_all(audio_dir(dir)).map_err(|e| format!("创建音频目录失败：{e}"))?;
        let file_name = format!("{record_id}.wav");
        let final_path = audio_file_path(dir, &file_name)?;
        let temp_path = audio_dir(dir).join(format!(".{record_id}.wav.part"));
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
            committed: false,
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
        self.writer
            .as_mut()
            .ok_or_else(|| "录音文件已经关闭。".to_string())?
            .write_all(pcm)
            .map_err(|e| format!("写入录音失败：{e}"))?;
        self.pcm_bytes = next_len;
        Ok(())
    }

    pub fn finish(mut self) -> Result<AudioArtifact, String> {
        if self.pcm_bytes == 0 {
            return Err("本次录音没有有效音频。".into());
        }

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
        if !self.committed {
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

        append(&dir, id.clone(), "测试", 10, Some(artifact.clone()), None).unwrap();
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
    fn unfinished_recording_removes_partial_file() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let partial = audio_dir(&dir).join(format!(".{id}.wav.part"));
        {
            let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
            recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
            assert!(partial.exists());
        }
        assert!(!partial.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_record_removes_text_audio_and_backup() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        let audio_path = audio_file_path(&dir, &artifact.file_name).unwrap();
        append(&dir, id.clone(), "可删除", 1, Some(artifact), None).unwrap();
        fs::write(history_path(&dir).with_extension("json.bak"), "private").unwrap();

        delete_record(&dir, &id).unwrap();

        assert!(load(&dir).records.is_empty());
        assert!(!audio_path.exists());
        assert!(!history_path(&dir).with_extension("json.bak").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_retention_removes_audio_but_keeps_transcript() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        let audio_path = audio_file_path(&dir, &artifact.file_name).unwrap();
        append(&dir, id, "只保留文字", 1, Some(artifact), None).unwrap();

        apply_audio_retention(&dir, "never").unwrap();

        let records = load(&dir).records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "只保留文字");
        assert!(records[0].audio.is_none());
        assert!(!audio_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_history_removes_all_audio() {
        let dir = temp_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let mut recorder = AudioRecorder::create(&dir, &id).unwrap();
        recorder.write_pcm(&[0, 0, 1, 0]).unwrap();
        let artifact = recorder.finish().unwrap();
        append(&dir, id, "清空", 1, Some(artifact), None).unwrap();

        clear(&dir).unwrap();

        assert!(load(&dir).records.is_empty());
        assert!(!audio_dir(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_removes_orphaned_audio_files() {
        let dir = temp_dir();
        fs::create_dir_all(audio_dir(&dir)).unwrap();
        let orphan = audio_dir(&dir).join("orphan.wav");
        fs::write(&orphan, b"private audio").unwrap();

        apply_audio_retention(&dir, "thirtyDays").unwrap();

        assert!(!orphan.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
