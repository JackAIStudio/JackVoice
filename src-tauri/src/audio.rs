use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig, StreamError};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The device the user wants JackVoice to prefer. An empty preference means
/// "follow the operating-system default". `name` is retained separately so
/// settings can still explain an offline device whose stable ID cannot be
/// resolved at the moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevicePreference {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveInputDevice {
    pub id: String,
    pub name: String,
}

/// How capture deviated from the user's chosen device. The UI turns these
/// into visible notices instead of failing (or switching) silently.
#[derive(Debug, Clone)]
pub enum AudioNotice {
    /// Every successful capture reports its actual device. This is runtime
    /// state, not a request to overwrite the user's saved preference.
    CaptureStarted {
        actual: ActiveInputDevice,
        using_fallback: bool,
    },
    /// The active device failed mid-session and no replacement could be started yet.
    DeviceLost { previous: ActiveInputDevice },
    /// Capture resumed on `actual` after a mid-session failure.
    DeviceChanged {
        previous: ActiveInputDevice,
        actual: ActiveInputDevice,
        using_fallback: bool,
    },
}

type PcmSink = Arc<Mutex<Box<dyn FnMut(Vec<u8>) + Send>>>;
type NoticeSink = Arc<Mutex<Box<dyn FnMut(AudioNotice) + Send>>>;

enum ControlMsg {
    Stop,
    StreamError,
}

/// How long to wait between reconnect attempts after the mic disappears mid-session.
const RECONNECT_RETRY_WAIT: Duration = Duration::from_millis(500);

/// Non-Send capture handle. The actual CPAL stream lives on a dedicated thread.
pub struct AudioCapture {
    stop_tx: Option<Sender<ControlMsg>>,
    join: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, AudioError> {
        let host = cpal::default_host();
        let mut items = enumerate_input_devices(&host)?
            .into_iter()
            .map(|candidate| InputDeviceInfo {
                id: candidate.info.id,
                name: candidate.info.name,
                is_default: candidate.is_default,
            })
            .collect::<Vec<_>>();

        // Keep default first for easier selection.
        items.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
        Ok(items)
    }

    /// Start capture, preferring the user's chosen device.
    ///
    /// Fault tolerance lives here:
    /// - If the preferred device is missing at start (e.g. a wireless
    ///   receiver was unplugged), capture falls back to the system default
    ///   input and reports it via `on_notice` instead of failing.
    /// - If the active device disappears mid-session, the capture thread
    ///   rebuilds the stream (preferring the original device again, then the
    ///   default) so the dictation session can keep running.
    ///
    /// Only when *no* input device exists at all does this return an error.
    pub fn start_with_device<F, N>(
        preferred_device: Option<InputDevicePreference>,
        input_gain_db: f32,
        on_pcm: F,
        on_notice: N,
    ) -> Result<Self, AudioError>
    where
        F: FnMut(Vec<u8>) + Send + 'static,
        N: FnMut(AudioNotice) + Send + 'static,
    {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (control_tx, control_rx) = mpsc::channel::<ControlMsg>();
        let preferred = preferred_device.filter(|device| !device.id.trim().is_empty());
        let pcm_sink: PcmSink = Arc::new(Mutex::new(Box::new(on_pcm)));
        let notice_sink: NoticeSink = Arc::new(Mutex::new(Box::new(on_notice)));

        // The stream error callback reports into the same channel used for Stop.
        let err_tx = control_tx.clone();

        let join = thread::Builder::new()
            .name("jackvoice-audio".into())
            .spawn(move || {
                audio_thread(
                    preferred,
                    input_gain_db,
                    pcm_sink,
                    notice_sink,
                    err_tx,
                    control_rx,
                    ready_tx,
                );
            })
            .map_err(|e| AudioError::Message(format!("无法创建录音线程：{e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx: Some(control_tx),
                join: Some(join),
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(AudioError::Message(err))
            }
            Err(_) => {
                let _ = join.join();
                Err(AudioError::Message("录音线程启动失败。".into()))
            }
        }
    }

    pub fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(ControlMsg::Stop);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Capture thread: start the stream, then keep it alive across device
/// disconnects until asked to stop.
fn audio_thread(
    preferred: Option<InputDevicePreference>,
    input_gain_db: f32,
    pcm_sink: PcmSink,
    notice_sink: NoticeSink,
    err_tx: Sender<ControlMsg>,
    control_rx: Receiver<ControlMsg>,
    ready_tx: Sender<Result<(), String>>,
) {
    let host = cpal::default_host();

    // First start: prefer the user's chosen device, otherwise the system default.
    let (first_stream, mut resolved) = match start_first_available_stream(
        &host,
        preferred.as_ref(),
        &pcm_sink,
        &err_tx,
        input_gain_db,
    ) {
        Ok(started) => started,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    emit_notice(
        &notice_sink,
        AudioNotice::CaptureStarted {
            actual: resolved.info.clone(),
            using_fallback: resolved.using_fallback,
        },
    );
    let _ = ready_tx.send(Ok(()));
    let mut stream: Option<Stream> = Some(first_stream);

    loop {
        match control_rx.recv() {
            Err(_) | Ok(ControlMsg::Stop) => break,
            Ok(ControlMsg::StreamError) => {
                // The active stream died (device unplugged, config change…).
                stream = None;
                if drain_stop(&control_rx) {
                    break;
                }

                let previous = resolved.info.clone();
                let mut rebuilt = try_rebuild_stream(
                    &host,
                    preferred.as_ref(),
                    &pcm_sink,
                    &err_tx,
                    input_gain_db,
                );
                if rebuilt.is_none() {
                    // No usable mic right now; tell the UI, then keep retrying.
                    emit_notice(
                        &notice_sink,
                        AudioNotice::DeviceLost {
                            previous: previous.clone(),
                        },
                    );
                }
                while rebuilt.is_none() {
                    match control_rx.recv_timeout(RECONNECT_RETRY_WAIT) {
                        Err(RecvTimeoutError::Disconnected) | Ok(ControlMsg::Stop) => return,
                        Ok(ControlMsg::StreamError) | Err(RecvTimeoutError::Timeout) => {}
                    }
                    rebuilt = try_rebuild_stream(
                        &host,
                        preferred.as_ref(),
                        &pcm_sink,
                        &err_tx,
                        input_gain_db,
                    );
                }
                let (new_stream, new_resolved) = rebuilt.unwrap();
                emit_notice(
                    &notice_sink,
                    AudioNotice::DeviceChanged {
                        previous,
                        actual: new_resolved.info.clone(),
                        using_fallback: new_resolved.using_fallback,
                    },
                );
                stream = Some(new_stream);
                resolved = new_resolved;
            }
        }
    }
    drop(stream);
}

/// Consume messages queued while tearing down a dead stream. Returns true if
/// a Stop was among them.
fn drain_stop(control_rx: &Receiver<ControlMsg>) -> bool {
    let mut stop = false;
    while let Ok(msg) = control_rx.try_recv() {
        if matches!(msg, ControlMsg::Stop) {
            stop = true;
        }
    }
    stop
}

fn try_rebuild_stream(
    host: &cpal::Host,
    preferred: Option<&InputDevicePreference>,
    pcm_sink: &PcmSink,
    err_tx: &Sender<ControlMsg>,
    input_gain_db: f32,
) -> Option<(Stream, ResolvedInput)> {
    start_first_available_stream(host, preferred, pcm_sink, err_tx, input_gain_db).ok()
}

fn emit_notice(sink: &NoticeSink, notice: AudioNotice) {
    let mut cb = sink.lock();
    cb(notice);
}

fn build_input_stream(
    device: &Device,
    pcm_sink: PcmSink,
    err_tx: Sender<ControlMsg>,
    input_gain_db: f32,
) -> Result<Stream, String> {
    let supported = device
        .default_input_config()
        .map_err(|e| format!("读取麦克风配置失败：{e}"))?;

    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    let resample_state = Arc::new(Mutex::new(Resampler::new(
        config.sample_rate.0,
        16_000,
        input_gain_db,
    )));
    let err_fn = move |err: StreamError| {
        eprintln!("audio stream error: {err}");
        let _ = err_tx.send(ControlMsg::StreamError);
    };

    match sample_format {
        SampleFormat::F32 => {
            let sink = pcm_sink.clone();
            let resampler = resample_state.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let mono = downmix_f32(data, channels);
                    let pcm = resampler.lock().push_f32(&mono);
                    if !pcm.is_empty() {
                        let mut cb = sink.lock();
                        cb(pcm);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let sink = pcm_sink.clone();
            let resampler = resample_state.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let mono = downmix_i16(data, channels);
                    let pcm = resampler.lock().push_i16(&mono);
                    if !pcm.is_empty() {
                        let mut cb = sink.lock();
                        cb(pcm);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let sink = pcm_sink.clone();
            let resampler = resample_state.clone();
            device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let mono: Vec<i16> = data
                        .chunks(channels)
                        .map(|frame| {
                            let sum: i32 = frame.iter().map(|s| (*s as i32) - 32768).sum();
                            (sum / channels.max(1) as i32) as i16
                        })
                        .collect();
                    let pcm = resampler.lock().push_i16(&mono);
                    if !pcm.is_empty() {
                        let mut cb = sink.lock();
                        cb(pcm);
                    }
                },
                err_fn,
                None,
            )
        }
        other => {
            return Err(format!("暂不支持的麦克风采样格式：{other:?}"));
        }
    }
    .map_err(|e| format!("创建录音流失败：{e}"))
}

struct ResolvedInput {
    device: Device,
    info: ActiveInputDevice,
    using_fallback: bool,
}

fn ordered_input_candidates(
    host: &cpal::Host,
    preferred: Option<&InputDevicePreference>,
) -> Result<Vec<ResolvedInput>, String> {
    let mut candidates = enumerate_input_devices(host).map_err(|error| error.to_string())?;
    let mut ordered = Vec::with_capacity(candidates.len());

    if let Some(preferred) = preferred {
        if let Some(index) = candidates
            .iter()
            .position(|candidate| preference_matches(preferred, &candidate.info))
        {
            let candidate = candidates.remove(index);
            ordered.push(ResolvedInput {
                device: candidate.device,
                info: candidate.info,
                using_fallback: false,
            });
        }
    }

    if let Some(default_index) = candidates.iter().position(|candidate| candidate.is_default) {
        let candidate = candidates.remove(default_index);
        ordered.push(ResolvedInput {
            device: candidate.device,
            info: candidate.info,
            using_fallback: preferred.is_some(),
        });
    }
    ordered.extend(candidates.into_iter().map(|candidate| ResolvedInput {
        device: candidate.device,
        info: candidate.info,
        using_fallback: preferred.is_some(),
    }));

    if ordered.is_empty() {
        Err(if let Some(preferred) = preferred {
            format!(
                "麦克风「{}」不可用，且没有其他可用麦克风。请检查连接或系统麦克风权限。",
                preferred_display_name(preferred)
            )
        } else {
            "未检测到可用麦克风。请检查连接或系统麦克风权限。".into()
        })
    } else {
        Ok(ordered)
    }
}

fn start_first_available_stream(
    host: &cpal::Host,
    preferred: Option<&InputDevicePreference>,
    pcm_sink: &PcmSink,
    err_tx: &Sender<ControlMsg>,
    input_gain_db: f32,
) -> Result<(Stream, ResolvedInput), String> {
    let candidates = ordered_input_candidates(host, preferred)?;
    let mut failures = Vec::new();
    for candidate in candidates {
        let stream = match build_input_stream(
            &candidate.device,
            pcm_sink.clone(),
            err_tx.clone(),
            input_gain_db,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                failures.push(format!("{}：{error}", candidate.info.name));
                continue;
            }
        };
        if let Err(error) = stream.play() {
            failures.push(format!("{}：启动失败（{error}）", candidate.info.name));
            continue;
        }
        return Ok((stream, candidate));
    }
    Err(format!("所有可用麦克风均无法启动：{}", failures.join("；")))
}

struct EnumeratedInput {
    device: Device,
    info: ActiveInputDevice,
    is_default: bool,
}

fn enumerate_input_devices(host: &cpal::Host) -> Result<Vec<EnumeratedInput>, AudioError> {
    let default_name = host
        .default_input_device()
        .and_then(|device| device.name().ok())
        .unwrap_or_default();
    let mut platform_devices = platform_input_devices();
    let default_platform_id = platform_devices
        .iter()
        .find(|device| device.is_default)
        .map(|device| device.id.clone());
    let devices = host
        .input_devices()
        .map_err(|error| AudioError::Message(format!("读取麦克风列表失败：{error}")))?;
    let mut enumerated = Vec::new();

    for device in devices {
        let name = device
            .name()
            .map_err(|error| AudioError::Message(format!("读取麦克风名称失败：{error}")))?;
        let platform_index = if name == default_name {
            default_platform_id
                .as_deref()
                .and_then(|default_id| {
                    platform_devices
                        .iter()
                        .position(|item| item.name == name && item.id == default_id)
                })
                .or_else(|| platform_devices.iter().position(|item| item.name == name))
        } else {
            platform_devices.iter().position(|item| item.name == name)
        };
        let platform = platform_index.map(|index| platform_devices.remove(index));
        let id = platform
            .as_ref()
            .map(|item| item.id.clone())
            .unwrap_or_else(|| name.clone());
        let is_default = default_platform_id
            .as_deref()
            .map(|default_id| default_id == id)
            .unwrap_or_else(|| name == default_name);
        enumerated.push(EnumeratedInput {
            device,
            info: ActiveInputDevice { id, name },
            is_default,
        });
    }
    Ok(enumerated)
}

fn preference_matches(preference: &InputDevicePreference, device: &ActiveInputDevice) -> bool {
    device.id == preference.id
        || (!preference.id.starts_with("coreaudio:") && device.name == preference.id)
}

fn preferred_display_name(preference: &InputDevicePreference) -> &str {
    if preference.name.trim().is_empty() {
        &preference.id
    } else {
        &preference.name
    }
}

#[derive(Debug)]
struct PlatformInputDevice {
    id: String,
    name: String,
    is_default: bool,
}

#[cfg(target_os = "macos")]
fn platform_input_devices() -> Vec<PlatformInputDevice> {
    macos_device_ids::input_devices().unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn platform_input_devices() -> Vec<PlatformInputDevice> {
    Vec::new()
}

#[cfg(target_os = "macos")]
mod macos_device_ids {
    use super::PlatformInputDevice;
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2_core_audio::{
        kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
        kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
        AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    };
    use std::ffi::c_void;
    use std::mem::{size_of, MaybeUninit};
    use std::ptr::{null, NonNull};

    pub(super) fn input_devices() -> Result<Vec<PlatformInputDevice>, String> {
        let devices_address = address(kAudioHardwarePropertyDevices);
        let device_ids = get_property_vec::<AudioObjectID>(
            kAudioObjectSystemObject as AudioObjectID,
            &devices_address,
        )?;
        let default_id = get_property::<AudioObjectID>(
            kAudioObjectSystemObject as AudioObjectID,
            &address(kAudioHardwarePropertyDefaultInputDevice),
        )
        .ok();

        let mut devices = Vec::new();
        for object_id in device_ids {
            if !has_input_streams(object_id) {
                continue;
            }
            let Ok(name) = string_property(object_id, kAudioObjectPropertyName) else {
                continue;
            };
            let Ok(uid) = string_property(object_id, kAudioDevicePropertyDeviceUID) else {
                continue;
            };
            devices.push(PlatformInputDevice {
                id: format!("coreaudio:{uid}"),
                name,
                is_default: default_id == Some(object_id),
            });
        }
        Ok(devices)
    }

    fn has_input_streams(object_id: AudioObjectID) -> bool {
        let streams_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreams,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut size = 0_u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object_id,
                NonNull::from(&streams_address),
                0,
                null(),
                NonNull::from(&mut size),
            )
        };
        status == 0 && size >= size_of::<AudioObjectID>() as u32
    }

    fn address(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn string_property(object_id: AudioObjectID, selector: u32) -> Result<String, String> {
        let raw = get_property::<CFStringRef>(object_id, &address(selector))?;
        if raw.is_null() {
            return Err("CoreAudio 设备属性为空。".into());
        }
        let value = unsafe { CFString::wrap_under_get_rule(raw) };
        Ok(value.to_string())
    }

    fn get_property<T: Copy>(
        object_id: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<T, String> {
        let mut value = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(address),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::new(value.as_mut_ptr().cast::<c_void>())
                    .ok_or_else(|| "CoreAudio 返回空地址。".to_string())?,
            )
        };
        if status != 0 || size != size_of::<T>() as u32 {
            return Err(format!("读取 CoreAudio 设备属性失败（{status}）。"));
        }
        Ok(unsafe { value.assume_init() })
    }

    fn get_property_vec<T: Copy + Default>(
        object_id: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<Vec<T>, String> {
        let mut size = 0_u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object_id,
                NonNull::from(address),
                0,
                null(),
                NonNull::from(&mut size),
            )
        };
        if status != 0 || !(size as usize).is_multiple_of(size_of::<T>()) {
            return Err(format!("读取 CoreAudio 设备列表大小失败（{status}）。"));
        }
        let mut values = vec![T::default(); size as usize / size_of::<T>()];
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(address),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::new(values.as_mut_ptr().cast::<c_void>())
                    .ok_or_else(|| "CoreAudio 返回空设备列表。".to_string())?,
            )
        };
        if status != 0 {
            return Err(format!("读取 CoreAudio 设备列表失败（{status}）。"));
        }
        Ok(values)
    }
}

fn downmix_f32(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

fn downmix_i16(data: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| *s as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Manual gain offset in dB applied on top of the automatic gain control
    /// (0 = no offset). The AGC always runs; this is only a user override for
    /// when recognition still feels too quiet / too loud.
    gain_offset_db: f32,
    acc: f64,
    last_sample: f32,
    has_last: bool,
    /// WebRTC AudioProcessing module running automatic gain control at 16 kHz.
    agc: Option<webrtc_audio_processing::Processor>,
    /// Accumulates 16 kHz mono f32 samples until a full 10 ms frame is ready.
    agc_pending: Vec<f32>,
    /// Samples per 10 ms frame at 16 kHz.
    agc_frame_size: usize,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32, gain_db: f32) -> Self {
        let agc = webrtc_audio_processing::Processor::new(16_000)
            .ok()
            .inspect(|processor| {
                use webrtc_audio_processing::config::{
                    AdaptiveDigital, Config as WapConfig, GainController, GainController2,
                };
                let config = WapConfig {
                    // Newer adaptive-digital AGC (Gain Controller 2). Tuned for
                    // dictation: starts at +15 dB so the first syllables are
                    // already boosted, caps total gain at +24 dB, and lets the
                    // built-in limiter hold loud speech back.
                    gain_controller: Some(GainController::GainController2(GainController2 {
                        adaptive_digital: Some(AdaptiveDigital {
                            headroom_db: 5.0,
                            max_gain_db: 24.0,
                            initial_gain_db: 15.0,
                            max_gain_change_db_per_second: 6.0,
                            max_output_noise_level_dbfs: -50.0,
                        }),
                        ..Default::default()
                    })),
                    // Echo cancellation / noise suppression are intentionally
                    // off: this is a dictation-only capture path.
                    ..Default::default()
                };
                processor.set_config(config);
            });
        let agc_frame_size = agc
            .as_ref()
            .map(|p| p.num_samples_per_frame())
            .unwrap_or(16_000 / 100);
        Self {
            in_rate: in_rate.max(1),
            out_rate: out_rate.max(1),
            gain_offset_db: gain_db.max(0.0),
            acc: 0.0,
            last_sample: 0.0,
            has_last: false,
            agc,
            agc_pending: Vec::with_capacity(agc_frame_size),
            agc_frame_size,
        }
    }

    fn push_f32(&mut self, samples: &[f32]) -> Vec<u8> {
        let mut out_samples: Vec<f32> = if self.in_rate == self.out_rate {
            samples.to_vec()
        } else {
            let mut resampled = Vec::new();
            let step = self.in_rate as f64 / self.out_rate as f64;

            for &sample in samples {
                if !self.has_last {
                    self.last_sample = sample;
                    self.has_last = true;
                    continue;
                }

                self.acc += 1.0;
                while self.acc >= step {
                    let t = 1.0 - ((self.acc - step) / step).clamp(0.0, 1.0);
                    let value = self.last_sample + (sample - self.last_sample) * t as f32;
                    resampled.push(value);
                    self.acc -= step;
                }
                self.last_sample = sample;
            }
            resampled
        };

        self.process_agc(&mut out_samples)
    }

    fn push_i16(&mut self, samples: &[i16]) -> Vec<u8> {
        let as_f32: Vec<f32> = samples
            .iter()
            .map(|s| *s as f32 / i16::MAX as f32)
            .collect();
        self.push_f32(&as_f32)
    }

    /// Run the automatic gain controller on the resampled 16 kHz mono stream,
    /// then apply the manual offset and convert to PCM16. Returns an empty
    /// Vec until a full 10 ms frame has accumulated.
    fn process_agc(&mut self, samples: &mut [f32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let frame_size = self.agc_frame_size;
        self.agc_pending.extend_from_slice(samples);

        let manual_linear = 10f32.powf(self.gain_offset_db / 20.0);
        let mut frame = Vec::with_capacity(frame_size);
        while self.agc_pending.len() >= frame_size {
            frame.clear();
            frame.extend(self.agc_pending.drain(..frame_size));

            if let Some(agc) = self.agc.as_ref() {
                if let Err(err) = agc.process_capture_frame(std::iter::once(&mut frame)) {
                    eprintln!("自动增益处理失败：{err}");
                }
            }

            for sample in frame.iter_mut() {
                *sample = soft_limit(*sample * manual_linear);
            }
            bytes.extend_from_slice(&f32_to_pcm16(&frame));
        }
        bytes
    }
}

/// Soft-knee limiter: linear below the knee, asymptotically approaches 1.0
/// above it, so an occasional loud peak never hard-clips into a square wave.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.85;
    let magnitude = x.abs();
    if magnitude <= KNEE {
        return x;
    }
    let headroom = 1.0 - KNEE;
    let over = magnitude - KNEE;
    let compressed = KNEE + headroom * (over / (over + headroom));
    x.signum() * compressed
}

fn f32_to_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let i = (clamped * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    bytes
}

/// Rough level in 0.0..=1.0 from PCM16 mono bytes.
pub fn pcm16_level(pcm: &[u8]) -> f32 {
    if pcm.len() < 2 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    let mut i = 0;
    while i + 1 < pcm.len() {
        let sample = i16::from_le_bytes([pcm[i], pcm[i + 1]]) as f64 / i16::MAX as f64;
        sum += sample * sample;
        count += 1.0;
        i += 2;
    }
    if count <= 0.0 {
        return 0.0;
    }
    let rms = (sum / count).sqrt();
    // Mild curve so quiet speech still moves the meter.
    (rms * 3.2).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16_peak(pcm: &[u8]) -> f64 {
        pcm.chunks_exact(2)
            .map(|s| {
                let v = i16::from_le_bytes([s[0], s[1]]) as f64 / i16::MAX as f64;
                v.abs()
            })
            .fold(0.0f64, f64::max)
    }

    /// A -40 dBFS tone should come out of the WebRTC AGC clearly above where
    /// it went in. A pure tone triggers the +15 dB initial gain (the adaptive
    /// stage only adds more for speech-like modulation), so expect ~+15 dB.
    #[test]
    fn agc_boosts_quiet_signal() {
        let mut resampler = Resampler::new(16_000, 16_000, 0.0);
        let mut quiet = Vec::new();
        for i in 0..(16_000 * 2) {
            let t = i as f32 * 2.0 * std::f32::consts::PI * 200.0 / 16_000.0;
            quiet.push(t.sin() * 0.01);
        }

        let mut out = Vec::new();
        for chunk in quiet.chunks(1600) {
            out.extend_from_slice(&resampler.push_f32(chunk));
        }

        assert!(
            out.len() >= quiet.len() * 2,
            "AGC stage should not drop samples"
        );
        let peak = pcm16_peak(&out);
        assert!(
            peak > 0.05,
            "AGC did not boost the quiet signal: peak={peak:.4}"
        );
    }

    /// Manual offset applies a deterministic boost on top of the pipeline.
    /// The AGC is disabled here so the assertion is about the offset alone.
    #[test]
    fn manual_offset_applies_on_top() {
        let make = |offset_db: f32| Resampler {
            in_rate: 16_000,
            out_rate: 16_000,
            gain_offset_db: offset_db,
            acc: 0.0,
            last_sample: 0.0,
            has_last: false,
            agc: None,
            agc_pending: Vec::new(),
            agc_frame_size: 160,
        };
        let sample = [0.2f32; 3200];

        let out = make(0.0).push_f32(&sample);
        let base_peak = pcm16_peak(&out);

        let out = make(12.0).push_f32(&sample);
        let boosted_peak = pcm16_peak(&out);
        assert!(
            boosted_peak > base_peak * 2.0,
            "manual offset did not raise the level: {base_peak:.4} -> {boosted_peak:.4}"
        );
    }

    #[test]
    fn legacy_name_preference_still_resolves() {
        let preference = InputDevicePreference {
            id: "Wireless Mic".into(),
            name: "Wireless Mic".into(),
        };
        let device = ActiveInputDevice {
            id: "coreaudio:stable-wireless-id".into(),
            name: "Wireless Mic".into(),
        };
        assert!(preference_matches(&preference, &device));
    }

    #[test]
    fn stable_id_does_not_silently_bind_to_different_hardware_with_same_name() {
        let preference = InputDevicePreference {
            id: "coreaudio:original-id".into(),
            name: "Wireless Mic".into(),
        };
        let replacement = ActiveInputDevice {
            id: "coreaudio:replacement-id".into(),
            name: "Wireless Mic".into(),
        };
        assert!(!preference_matches(&preference, &replacement));
    }
}
