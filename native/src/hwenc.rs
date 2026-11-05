//! Hardware H.264 encoding through Windows Media Foundation.
//!
//! Media Foundation picks the GPU vendor's encoder: NVENC on NVIDIA, AMF on
//! AMD, QuickSync on Intel. Frames go in as NV12, H.264 access units come out
//! and are handed to LiveKit as pre-encoded frames, so the CPU only converts
//! colours instead of compressing video.
//!
//! Hardware encoders are asynchronous MFTs: they say when they want input and
//! when output is ready through events, so the encoder runs on its own thread.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

/// Seconds between the keyframes the encoder makes on its own. Each is several times a usual frame and
/// holds up the ones behind it for a moment, so they are rare: whoever joins or loses packets asks for one.
const KEYFRAME_SECONDS: u32 = 30;

/// What a hard moment may take, for an average bitrate.
fn peak_of(bitrate: u64) -> u32 {
    (bitrate / 10 * 12).min(u32::MAX as u64) as u32
}

/// The encoder's buffer, in bits: half a second, so a frame may be well above the average.
fn buffer_of(bitrate: u64) -> u32 {
    (bitrate / 2).min(u32::MAX as u64) as u32
}

/// A frame waiting to be encoded.
pub struct Input {
    /// Shared so the capture side can keep the last frame to repeat it.
    pub nv12: Arc<Vec<u8>>,
    pub timestamp_us: i64,
}

/// Handle to the encoder thread. Dropping it ends the encoder.
pub struct Encoder {
    frames: SyncSender<Input>,
    keyframe: Arc<AtomicBool>,
    /// Target bitrate the encoder should switch to (0 = unchanged).
    bitrate: Arc<AtomicU64>,
    sinks: Sinks,
    pub name: String,
}

/// Other places the finished frames go besides LiveKit (Discord's Go Live).
type Sinks = Arc<std::sync::Mutex<Vec<crate::golive_send::FrameSink>>>;

/// A cheap copy of the encoder's controls that does not keep it alive: for a second
/// consumer of the same encoded frames.
#[derive(Clone)]
pub struct Handle {
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU64>,
    sinks: Sinks,
}

impl Handle {
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, bps: u64) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    /// Frames are handed to `sink` (Annex B, keyframe flag, µs) until it returns false.
    pub fn add_sink(&self, sink: crate::golive_send::FrameSink) {
        self.sinks.lock().unwrap().push(sink);
    }
}

impl Encoder {
    /// Queues a frame. If the encoder is busy the frame is dropped: for live
    /// streaming a skipped frame beats growing delay.
    pub fn push(&self, frame: Input) -> bool {
        match self.frames.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, bps: u64) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    pub fn handle(&self) -> Handle {
        Handle { keyframe: self.keyframe.clone(), bitrate: self.bitrate.clone(), sinks: self.sinks.clone() }
    }
}

/// Starts a hardware encoder for `width`×`height` frames whose output goes to `source`.
pub fn start(width: u32, height: u32, fps: u32, bitrate: u64, source: NativeVideoSource, high: bool) -> Result<Encoder, String> {
    if std::env::var_os("NIMBUS_SOFTWARE_ENCODER").is_some() {
        return Err("аппаратное кодирование выключено (NIMBUS_SOFTWARE_ENCODER)".into());
    }
    // Two frames of slack: enough to absorb jitter without adding latency.
    let (frames, queue) = sync_channel::<Input>(2);
    // The first frame is a keyframe anyway.
    let keyframe = Arc::new(AtomicBool::new(false));
    let target = Arc::new(AtomicU64::new(0));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();

    let sinks: Sinks = Arc::default();
    let (thread_keyframe, thread_target, thread_sinks) = (keyframe.clone(), target.clone(), sinks.clone());
    std::thread::Builder::new()
        .name("hw-encoder".into())
        .spawn(move || {
            let setup = unsafe { Mft::open(width, height, fps, bitrate, high) };
            match setup {
                Ok(mft) => {
                    let _ = ready_tx.send(Ok(mft.name.clone()));
                    let looped = unsafe { mft.run(queue, &thread_keyframe, &thread_target, &thread_sinks, &source, width, height, fps) };
                    if let Err(e) = looped {
                        crate::log_line(&format!("hardware encoder stopped: {e}"));
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let name = ready_rx.recv().map_err(|_| "поток кодировщика не запустился".to_string())??;
    Ok(Encoder { frames, keyframe, bitrate: target, sinks, name })
}

struct Mft {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    provides_samples: bool,
    output_size: u32,
    name: String,
}

fn err(context: &str, e: windows::core::Error) -> String {
    format!("{context}: {e}")
}

impl Mft {
    unsafe fn open(width: u32, height: u32, fps: u32, bitrate: u64, high: bool) -> Result<Self, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| err("MFStartup", e))?;

            // Hardware H.264 encoders that take NV12, best first.
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .map_err(|e| err("MFTEnumEx", e))?;
            if count == 0 || list.is_null() {
                return Err("аппаратный кодировщик H.264 не найден".into());
            }
            let activates = std::slice::from_raw_parts_mut(list, count as usize);
            let activate = activates[0].clone().ok_or("пустой кодировщик")?;
            let name = friendly_name(&activate).unwrap_or_else(|| "аппаратный кодировщик".into());
            let transform: IMFTransform = activate.ActivateObject().map_err(|e| err("ActivateObject", e))?;
            for slot in activates.iter_mut() {
                drop(slot.take());
            }
            CoTaskMemFree(Some(list as *const _));

            // Hardware MFTs are asynchronous and must be unlocked before use.
            let attributes = transform.GetAttributes().map_err(|e| err("GetAttributes", e))?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| err("async unlock", e))?;
            let events: IMFMediaEventGenerator = transform.cast().map_err(|e| err("event generator", e))?;

            // A keyframe every few seconds (viewers can ask for one sooner). Some
            // encoders only take this before the media types are set.
            let gop = fps * KEYFRAME_SECONDS;
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(codec) = &codec {
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
            }

            // Encoders want the output type first, then the input type.
            let out_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            let _ = out_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, gop);
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| err("out major", e))?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| err("out subtype", e))?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate.min(u32::MAX as u64) as u32).map_err(|e| err("bitrate", e))?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).map_err(|e| err("frame size", e))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| err("frame rate", e))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| err("aspect", e))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| err("interlace", e))?;
            // LiveKit's pass-through negotiates Constrained Baseline (42e01f);
            // viewers drop anything else. Baseline from these encoders has no
            // FMO/ASO, so it is constrained in practice.
            // A stream only for Discord may use High: much better for the same bitrate.
            let profile = if high { eAVEncH264VProfile_High } else { eAVEncH264VProfile_Base };
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32).map_err(|e| err("profile", e))?;
            transform.SetOutputType(0, &out_type, 0).map_err(|e| err("SetOutputType", e))?;

            let in_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| err("in major", e))?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| err("in subtype", e))?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).map_err(|e| err("in size", e))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| err("in rate", e))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| err("in aspect", e))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| err("in interlace", e))?;
            transform.SetInputType(0, &in_type, 0).map_err(|e| err("SetInputType", e))?;

            // Steady bitrate and no frame reordering: what live streaming wants.
            if let Some(codec) = &codec {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                // The average holds, and a hard moment may take more for a while: with a steady rate and the short
                // buffer of low latency, every fast scene falls apart into blocks. Encoders that don't take it stay as they were.
                let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate.min(u32::MAX as u64) as u32));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(peak_of(bitrate)));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonBufferSize, &VARIANT::from(buffer_of(bitrate)));

                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
                if std::env::var_os("NIMBUS_LOG_KEYFRAMES").is_some() {
                    let read = codec.GetValue(&CODECAPI_AVEncMPVGOPSize).map(|value| format!("{value:?}"));
                    crate::log_line(&format!("hwenc GOP asked {gop}, encoder says {read:?}"));
                }
            }

            let info = transform.GetOutputStreamInfo(0).map_err(|e| err("GetOutputStreamInfo", e))?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| err("begin streaming", e))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| err("start of stream", e))?;

            Ok(Self { transform, events, codec, provides_samples, output_size: info.cbSize.max(1 << 20), name })
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn run(
        &self,
        queue: Receiver<Input>,
        keyframe: &AtomicBool,
        target: &AtomicU64,
        sinks: &Sinks,
        source: &NativeVideoSource,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), String> {
        unsafe {
            let resolution = VideoResolution { width, height };
            let frame_duration = 10_000_000 / fps.max(1) as i64;
            // LiveKit treats timestamp 0 as "use the wall clock", so keep every
            // frame on the wall clock or the first one jumps decades ahead.
            let base_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(1);
            let mut logged = 0;
            // WebRTC drops a frame when the next one lands before it was taken
            // (and then wants a keyframe to mend the chain); the GPU can hand
            // out two at once, so deliveries are kept half a frame apart.
            let spacing = std::time::Duration::from_micros(500_000 / fps.max(1) as u64);
            let mut last_delivery: Option<std::time::Instant> = None;
            loop {
                let event = self.events.GetEvent(MF_EVENT_FLAG_NONE).map_err(|e| err("GetEvent", e))?;
                let kind = event.GetType().map_err(|e| err("GetType", e))?;
                if kind == METransformNeedInput.0 as u32 {
                    // Blocks until the capture thread hands us a frame; ends when it's gone.
                    let Ok(frame) = queue.recv() else { break };
                    if keyframe.swap(false, Ordering::Relaxed) {
                        if let Some(codec) = &self.codec {
                            let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                        }
                    }
                    let bps = target.swap(0, Ordering::Relaxed);
                    if bps > 0 {
                        if let Some(codec) = &self.codec {
                            let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps.min(u32::MAX as u64) as u32));
                            let _ = codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(peak_of(bps)));
                            let _ = codec.SetValue(&CODECAPI_AVEncCommonBufferSize, &VARIANT::from(buffer_of(bps)));
                        }
                    }
                    let sample = make_sample(&frame.nv12, frame.timestamp_us * 10, frame_duration)?;
                    self.transform.ProcessInput(0, &sample, 0).map_err(|e| err("ProcessInput", e))?;
                } else if kind == METransformHaveOutput.0 as u32 {
                    if let Some((data, key, time_us)) = self.take_output()? {
                        if let Some(wait) = last_delivery.and_then(|t| spacing.checked_sub(t.elapsed())) {
                            std::thread::sleep(wait);
                        }
                        last_delivery = Some(std::time::Instant::now());
                        let frame = EncodedVideoFrame {
                            codec: EncodedVideoCodec::H264,
                            payload: &data,
                            timestamp_us: base_us + time_us,
                            frame_type: if key { EncodedFrameType::Key } else { EncodedFrameType::Delta },
                            resolution: resolution.clone(),
                            frame_metadata: None,
                        };
                        let accepted = source.capture_encoded_frame(&frame);
                        sinks.lock().unwrap().retain(|sink| sink(&data, key, base_us + time_us));
                        if key && std::env::var_os("NIMBUS_LOG_KEYFRAMES").is_some() {
                            crate::log_line(&format!("hwenc keyframe out at {} ms, {} bytes", time_us / 1000, data.len()));
                        }
                        if logged < 2 {
                            logged += 1;
                            crate::log_line(&format!(
                                "hwenc frame: {} bytes key={key} accepted={accepted} nals={:?}",
                                data.len(),
                                nal_types(&data)
                            ));
                        }
                    }
                }
            }
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            Ok(())
        }
    }

    /// One encoded access unit: Annex-B bytes, keyframe flag, timestamp in µs.
    unsafe fn take_output(&self) -> Result<Option<(Vec<u8>, bool, i64)>, String> {
        unsafe {
            let own_sample = if self.provides_samples { None } else { Some(make_empty_sample(self.output_size)?) };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let buffer = &mut buffers[0];
            let sample = ManuallyDrop::take(&mut buffer.pSample);
            drop(ManuallyDrop::take(&mut buffer.pEvents));
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(err("ProcessOutput", e));
            }
            let Some(sample) = sample else { return Ok(None) };
            let media = sample.ConvertToContiguousBuffer().map_err(|e| err("ConvertToContiguousBuffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            media.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| err("Lock", e))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = media.Unlock();
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let time_us = sample.GetSampleTime().unwrap_or(0) / 10;
            Ok(Some((data, key, time_us)))
        }
    }
}

/// NAL unit types in an Annex-B stream (7 = SPS, 8 = PPS, 5 = IDR, 1 = slice).
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            types.push(data[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
    unsafe {
        let mut text = windows::core::PWSTR::null();
        let mut len = 0u32;
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len).ok()?;
        let name = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        name
    }
}

unsafe fn make_sample(bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample, String> {
    unsafe {
        let media = MFCreateMemoryBuffer(bytes.len() as u32).map_err(|e| err("MFCreateMemoryBuffer", e))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        media.Lock(&mut ptr, None, None).map_err(|e| err("Lock", e))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = media.Unlock();
        media.SetCurrentLength(bytes.len() as u32).map_err(|e| err("SetCurrentLength", e))?;
        let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
        sample.AddBuffer(&media).map_err(|e| err("AddBuffer", e))?;
        sample.SetSampleTime(time).map_err(|e| err("SetSampleTime", e))?;
        sample.SetSampleDuration(duration).map_err(|e| err("SetSampleDuration", e))?;
        Ok(sample)
    }
}

unsafe fn make_empty_sample(size: u32) -> Result<IMFSample, String> {
    unsafe {
        let media = MFCreateMemoryBuffer(size).map_err(|e| err("MFCreateMemoryBuffer", e))?;
        let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
        sample.AddBuffer(&media).map_err(|e| err("AddBuffer", e))?;
        Ok(sample)
    }
}
