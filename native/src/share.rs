//! Screen capture for Nimbus streams: Windows Graphics Capture hands us BGRA
//! frames of a monitor or a window. With a hardware encoder (see hwenc.rs)
//! they become NV12 for the GPU and leave as finished H.264; otherwise they
//! become I420 and LiveKit's software encoder compresses them. Frames whose
//! size differs from the stream's (a lower quality, a resized window) are
//! scaled first. Capture runs on its own thread and stops when `stop` is set;
//! a second thread repeats the last frame while the picture stands still.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use livekit::webrtc::native::yuv_helper;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::hwenc;

/// What to show: a monitor or a window, by its raw handle.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    Monitor(usize),
    Window(usize),
}

impl Source {
    pub fn primary() -> Result<Self, String> {
        let monitor = Monitor::primary().map_err(|e| crate::i18n::tf("не нашёл монитор: {}", &[&e]))?;
        Ok(Source::Monitor(monitor.as_raw_hmonitor() as usize))
    }

    /// Whose sound goes with it: one app for a window, everything but Nimbus for a screen.
    pub fn sound_scope(self) -> crate::audio::Scope {
        match self {
            Source::Window(h) => match Window::from_raw_hwnd(h as *mut _).process_id() {
                Ok(pid) => crate::audio::Scope::App(pid),
                Err(_) => crate::audio::Scope::AllButUs,
            },
            Source::Monitor(_) => crate::audio::Scope::AllButUs,
        }
    }

    /// Current size in pixels.
    fn size(self) -> Result<(u32, u32), String> {
        match self {
            Source::Monitor(h) => {
                let m = Monitor::from_raw_hmonitor(h as *mut _);
                Ok((m.width().map_err(|e| e.to_string())?, m.height().map_err(|e| e.to_string())?))
            }
            Source::Window(h) => {
                let w = Window::from_raw_hwnd(h as *mut _);
                if !w.is_valid() {
                    return Err(crate::i18n::t("окно закрыто или свёрнуто"));
                }
                let (width, height) = (w.width().map_err(|e| e.to_string())?, w.height().map_err(|e| e.to_string())?);
                Ok((width.max(2) as u32, height.max(2) as u32))
            }
        }
    }
}

/// A row of the "what to show" menu.
pub struct SourceInfo {
    pub source: Source,
    pub name: String,
    pub detail: String,
    pub is_window: bool,
}

/// Monitors first (the primary one on top), then visible windows with a title.
pub fn sources() -> Vec<SourceInfo> {
    let mut out = Vec::new();
    let primary = Monitor::primary().map(|m| m.as_raw_hmonitor() as usize).ok();
    let mut monitors = Monitor::enumerate().unwrap_or_default();
    monitors.sort_by_key(|m| Some(m.as_raw_hmonitor() as usize) != primary);
    for (i, m) in monitors.iter().enumerate() {
        let handle = m.as_raw_hmonitor() as usize;
        let size = match (m.width(), m.height()) {
            (Ok(w), Ok(h)) => format!("{w}×{h}"),
            _ => String::new(),
        };
        let mut detail = m.name().unwrap_or_default();
        if Some(handle) == primary {
            detail = if detail.is_empty() { crate::i18n::t("основной") } else { crate::i18n::tf("основной · {}", &[&detail]) };
        }
        out.push(SourceInfo {
            source: Source::Monitor(handle),
            name: crate::i18n::tf("Экран {} · {}", &[&(i + 1), &size]),
            detail,
            is_window: false,
        });
    }
    let own = std::process::id();
    for w in Window::enumerate().unwrap_or_default() {
        let title = w.title().unwrap_or_default();
        if title.trim().is_empty() || !w.is_valid() || w.process_id().ok() == Some(own) {
            continue;
        }
        // The desktop itself and zero-size helper windows aren't worth showing.
        if title == "Program Manager" || w.width().unwrap_or(0) < 50 || w.height().unwrap_or(0) < 50 {
            continue;
        }
        let process = w.process_name().unwrap_or_default();
        out.push(SourceInfo {
            source: Source::Window(w.as_raw_hwnd() as usize),
            name: title,
            detail: process.trim_end_matches(".exe").to_string(),
            is_window: true,
        });
    }
    out
}

/// How the stream looks to viewers.
#[derive(Clone, Copy, Debug)]
pub struct Quality {
    /// Output height; 0 keeps the source's own size.
    pub height: u32,
    pub fps: u32,
    /// Send the sound too.
    pub sound: bool,
}

impl Default for Quality {
    fn default() -> Self {
        Self { height: 0, fps: 60, sound: true }
    }
}

impl Quality {
    /// The bitrate ceiling: enough for fast games at this size and rate.
    pub fn max_bitrate(self, width: u32, height: u32) -> u64 {
        let pixels = width as u64 * height as u64;
        let at_60 = if pixels > 1920 * 1080 { 12_000_000 } else if pixels > 1280 * 720 { 8_000_000 } else { 5_000_000 };
        if self.fps <= 30 { at_60 * 2 / 3 } else { at_60 }
    }

    /// Stream size for a source of this size: scaled down to `height`, never up.
    fn fit(self, w: u32, h: u32) -> (u32, u32) {
        let (w, h) = if self.height == 0 || h <= self.height {
            (w, h)
        } else {
            ((w as u64 * self.height as u64 / h as u64) as u32, self.height)
        };
        // NV12 and H.264 need even sizes.
        ((w & !1).max(2), (h & !1).max(2))
    }
}

/// A video source ready to publish, with the encoder that feeds it.
pub struct Prepared {
    pub source: NativeVideoSource,
    pub encoder: Option<hwenc::Encoder>,
    /// Human-readable encoder name, for logs and the UI.
    pub encoder_name: String,
    pub what: Source,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate: u64,
}

/// Picks the encoder: the GPU's if Windows has one, else WebRTC's software one.
pub fn prepare(what: Source, quality: Quality, high: bool) -> Result<Prepared, String> {
    let (w, h) = what.size()?;
    let (width, height) = quality.fit(w, h);
    let fps = quality.fps.max(1);
    let max_bitrate = quality.max_bitrate(width, height);
    let resolution = VideoResolution { width, height };
    crate::log_line(&format!("screen share: {what:?} {w}x{h} -> {width}x{height} @{fps} {max_bitrate}bps"));

    let encoded = NativeVideoSource::new_encoded(resolution.clone());
    let (source, encoder, encoder_name) = match hwenc::start(width, height, fps, max_bitrate, encoded.clone(), high) {
        Ok(encoder) => {
            let name = encoder.name.clone();
            crate::log_line(&format!("screen share: hardware encoder {name}"));
            (encoded, Some(encoder), name)
        }
        Err(e) => {
            crate::log_line(&format!("screen share: software encoder ({e})"));
            (NativeVideoSource::new(resolution, true), None, crate::i18n::t("программный (WebRTC)"))
        }
    };
    Ok(Prepared { source, encoder, encoder_name, what, width, height, fps, max_bitrate })
}


/// Where captured frames go, and how to stop.
pub struct Target {
    /// Pictures of what is being shared, as often as they can be made, while the flag is up (our own preview).
    pub preview: Option<(Arc<AtomicBool>, Preview)>,
    pub prepared: Prepared,
    pub stop: Arc<AtomicBool>,
    /// Frames handed to the encoder so far, for checks and stats.
    pub frames: Arc<AtomicU64>,
    /// Called once if capture ends by itself (the window closed), not via `stop`.
    pub on_end: Option<Box<dyn FnOnce() + Send>>,
}

/// Gets a small picture of the shared screen now and then.
/// The picture, the height of what is sent, and how many frames a second really go out.
pub type Preview = Arc<dyn Fn(crate::Pixels, u32, u32) + Send + Sync>;
const PREVIEW_EVERY: Duration = Duration::from_millis(33);
const PREVIEW_WIDTH: u32 = 1280;

/// The capture hands raw frames to a thread of its own, which makes the small pictures, so the
/// capture and the encoder never wait for them. A frame is dropped while that thread is busy.
struct PreviewTap {
    on: Arc<AtomicBool>,
    tx: std::sync::mpsc::SyncSender<(Vec<u8>, u32, u32)>,
    /// That thread is at work: a frame isn't even copied for it.
    busy: Arc<AtomicBool>,
    /// The last frame's memory back from that thread, to copy the next one into.
    spare: Arc<Mutex<Vec<u8>>>,
}

/// A small RGBA picture of a BGRA frame.
fn preview_pixels(bgra: &[u8], w: u32, h: u32) -> crate::Pixels {
    let mut i420 = I420Buffer::new(w, h);
    let (stride_y, stride_u, stride_v) = i420.strides();
    let (y, u, v) = i420.data_mut();
    yuv_helper::argb_to_i420(bgra, w * 4, y, stride_y, u, stride_u, v, stride_v, w as i32, h as i32);
    let (pw, ph) = if w > PREVIEW_WIDTH {
        (PREVIEW_WIDTH, ((h as u64 * PREVIEW_WIDTH as u64 / w as u64) as u32 & !1).max(2))
    } else {
        (w & !1, h & !1)
    };
    let small = if (pw, ph) == (w, h) { i420 } else { i420.scale(pw as i32, ph as i32) };
    let (sw, sh) = (small.width(), small.height());
    let (stride_y, stride_u, stride_v) = small.strides();
    let (y, u, v) = small.data();
    let mut pixels = crate::Pixels::new(sw, sh);
    // libyuv "ABGR" is R,G,B,A in memory, which is what Slint holds.
    yuv_helper::i420_to_abgr(y, stride_y, u, stride_u, v, stride_v, pixels.make_mut_bytes(), sw * 4, sw as i32, sh as i32);
    pixels
}

/// Windows only delivers a frame when the picture changes. A still picture
/// still has to reach viewers who join late or lose a keyframe, so the last
/// frame goes out again at this pace while nothing moves.
const REPEAT_EVERY: Duration = Duration::from_millis(200);

/// The last frame sent, kept for repeating.
enum Held {
    Nv12(Arc<Vec<u8>>),
    I420(VideoFrame<I420Buffer>),
}

/// The sending end, shared by the capture callback and the repeat thread.
struct Output {
    prepared: Prepared,
    started: Instant,
    frames: Arc<AtomicU64>,
    /// Frames the encoder had no room for, since the last look.
    refused: AtomicU64,
    last: Mutex<(Option<Held>, Instant)>,
}

impl Output {
    fn send(&self, held: Held) {
        let timestamp_us = self.started.elapsed().as_micros() as i64;
        let mut last = self.last.lock().unwrap();
        let sent = self.send_now(held, timestamp_us, &mut last.0);
        last.1 = Instant::now();
        if sent {
            self.frames.fetch_add(1, Ordering::Relaxed);
        } else {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn repeat_if_still(&self) {
        let mut last = self.last.lock().unwrap();
        if last.1.elapsed() < REPEAT_EVERY {
            return;
        }
        if let Some(held) = last.0.take() {
            let timestamp_us = self.started.elapsed().as_micros() as i64;
            self.send_now(held, timestamp_us, &mut last.0);
        }
        last.1 = Instant::now();
    }

    /// Sends a frame and keeps it in `slot`.
    fn send_now(&self, held: Held, timestamp_us: i64, slot: &mut Option<Held>) -> bool {
        let source = &self.prepared.source;
        let sent = match (&self.prepared.encoder, held) {
            (Some(encoder), Held::Nv12(nv12)) => {
                // Viewers that join or lose packets ask for a keyframe; congestion lowers the bitrate.
                if source.take_keyframe_request() {
                    if std::env::var_os("NIMBUS_LOG_KEYFRAMES").is_some() {
                        crate::log_line(&format!("keyframe requested at {} ms", self.started.elapsed().as_millis()));
                    }
                    encoder.request_keyframe();
                }
                if let Some(rate) = source.take_rate_control_request() {
                    encoder.set_bitrate(rate.target_bitrate_bps.min(self.prepared.max_bitrate));
                }
                let sent = encoder.push(hwenc::Input { nv12: nv12.clone(), timestamp_us });
                *slot = Some(Held::Nv12(nv12));
                sent
            }
            (_, Held::I420(mut frame)) => {
                frame.timestamp_us = timestamp_us;
                let sent = source.capture_frame(&frame);
                *slot = Some(Held::I420(frame));
                sent
            }
            (None, Held::Nv12(_)) => false,
        };
        sent
    }
}

struct Flags {
    preview: Option<PreviewTap>,
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    on_end: Option<Box<dyn FnOnce() + Send>>,
}

/// Picks which frames Windows delivers become the stream's frames: one per interval on average,
/// whatever the screen's own rate. Dropping every frame that comes less than an interval after the
/// last kept one turns a 144 Hz screen into a 48 fps stream (every third frame); counting from the
/// time a frame was due instead keeps the average at the stream's rate.
struct Pacer {
    interval: Duration,
    due: Option<Instant>,
    last: Option<Instant>,
}

impl Pacer {
    fn new(interval: Duration) -> Self {
        Self { interval, due: None, last: None }
    }

    /// Whether the frame that came at `now` is kept.
    fn accept(&mut self, now: Instant) -> bool {
        // A frame may come a little early (a quarter of an interval); never two in quick succession.
        let early = self.interval / 4;
        if self.last.is_some_and(|t| now.duration_since(t) < self.interval / 2) {
            return false;
        }
        if self.due.is_some_and(|due| now + early < due) {
            return false;
        }
        // The next one is due an interval after this one was due; after a pause, an interval from now.
        let next = self.due.map_or(now + self.interval, |due| due + self.interval);
        self.due = Some(if next + self.interval < now { now + self.interval } else { next });
        self.last = Some(now);
        true
    }
}

struct Capture {
    flags: Flags,
    pacer: Pacer,
    /// The frame's pixels, kept between frames so nothing is allocated per frame.
    scratch: Vec<u8>,
    last_preview: Option<Instant>,
    timing: Timing,
    readback: Readback,
}

/// Copies a captured frame off the GPU into memory through one staging texture that stays around
/// between frames. The library's own `Frame::buffer()` creates a new staging texture, 15 MB at 1440p,
/// for every frame and frees it again: sixty allocations a second on the same graphics card the game
/// is drawing on, which is the kind of thing that shows up as small stutters.
struct Readback {
    staging: Option<ID3D11Texture2D>,
    /// This thread and its GPU work have been put first.
    raised: bool,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

impl Readback {
    fn new() -> Self {
        Self { staging: None, raised: false, width: 0, height: 0, format: DXGI_FORMAT::default() }
    }

    /// The frame's rows as BGRA without padding, into `out` (replacing what was there); its width and height.
    fn grab(&mut self, frame: &Frame, out: &mut Vec<u8>) -> Result<(u32, u32), windows::core::Error> {
        unsafe {
            let texture = frame.as_raw_texture();
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let device = texture.GetDevice()?;
            // A game at full load starves the capture for seconds: this thread and its GPU work go first.
            if !self.raised {
                self.raised = true;
                let _ = windows::Win32::System::Threading::SetThreadPriority(windows::Win32::System::Threading::GetCurrentThread(), windows::Win32::System::Threading::THREAD_PRIORITY_HIGHEST);
                if let Ok(dxgi) = device.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>() {
                    let _ = dxgi.SetGPUThreadPriority(7);
                }
            }
            let context = device.GetImmediateContext()?;
            if self.staging.is_none() || (self.width, self.height, self.format) != (desc.Width, desc.Height, desc.Format) {
                let staging = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut made = None;
                device.CreateTexture2D(&staging, None, Some(&mut made))?;
                self.staging = made;
                (self.width, self.height, self.format) = (desc.Width, desc.Height, desc.Format);
            }
            let Some(staging) = &self.staging else { return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))) };
            let staging: ID3D11Resource = staging.cast()?;
            let source: ID3D11Resource = texture.cast()?;
            context.CopyResource(&staging, &source);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let (w, h) = (desc.Width as usize, desc.Height as usize);
            out.clear();
            out.reserve(w * h * 4);
            for y in 0..h {
                let row = std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), w * 4);
                out.extend_from_slice(row);
            }
            context.Unmap(&staging, 0);
            Ok((desc.Width, desc.Height))
        }
    }
}

/// How long the capture callback takes, logged every few seconds: where the time goes when the
/// picture or the computer stutters (copying the frame off the GPU, converting it, handing it on).
struct Timing {
    since: Instant,
    frames: u32,
    readback: Duration,
    convert: Duration,
    total: Duration,
    slowest: Duration,
    /// The widest gap between two kept frames, and when the last one was.
    widest: Duration,
    last: Option<Instant>,
}

impl Timing {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, readback: Duration::ZERO, convert: Duration::ZERO, total: Duration::ZERO, slowest: Duration::ZERO, widest: Duration::ZERO, last: None }
    }

    /// `readback`, `converted` and `total` are measured from the start of the callback.
    fn add(&mut self, readback: Duration, converted: Duration, total: Duration, refused: &AtomicU64) {
        self.frames += 1;
        self.readback += readback;
        self.convert += converted.saturating_sub(readback);
        self.total += total;
        self.slowest = self.slowest.max(total);
        let now = Instant::now();
        if let Some(last) = self.last.replace(now) {
            self.widest = self.widest.max(now - last);
        }
        if self.since.elapsed() >= Duration::from_secs(5) {
            let n = self.frames.max(1);
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n as f64;
            crate::log_line(&format!(
                "capture: {} frames in 5 s; per frame {:.1} ms (off the GPU {:.1}, convert {:.1}); slowest {:.1} ms; widest gap {:.1} ms; encoder had no room for {}",
                self.frames,
                ms(self.total),
                ms(self.readback),
                ms(self.convert),
                self.slowest.as_secs_f64() * 1000.0,
                self.widest.as_secs_f64() * 1000.0,
                refused.swap(0, Ordering::Relaxed)
            ));
            let last = self.last;
            *self = Self::new();
            self.last = last;
        }
    }
}

impl Capture {
    /// The frame as I420 at the stream's size, scaled if needed.
    fn i420(&self, bgra: &[u8], w: u32, h: u32) -> I420Buffer {
        let mut i420 = I420Buffer::new(w, h);
        let (stride_y, stride_u, stride_v) = i420.strides();
        let (y, u, v) = i420.data_mut();
        yuv_helper::argb_to_i420(bgra, w * 4, y, stride_y, u, stride_u, v, stride_v, w as i32, h as i32);
        let p = &self.flags.out.prepared;
        if (w, h) == (p.width, p.height) { i420 } else { i420.scale(p.width as i32, p.height as i32) }
    }

    fn nv12(&self, bgra: &[u8], w: u32, h: u32) -> Vec<u8> {
        let p = &self.flags.out.prepared;
        let (ew, eh) = (p.width, p.height);
        let mut nv12 = vec![0u8; (ew * eh * 3 / 2) as usize];
        let (y, uv) = nv12.split_at_mut((ew * eh) as usize);
        if (w & !1, h & !1) == (ew, eh) {
            // Same size (an odd last row or column is dropped): straight to NV12.
            yuv_helper::argb_to_nv12(bgra, w * 4, y, ew, uv, ew, ew as i32, eh as i32);
        } else {
            let i420 = self.i420(bgra, w, h);
            let (sy, su, sv) = i420.strides();
            let (py, pu, pv) = i420.data();
            yuv_helper::i420_to_nv12(py, sy, pu, su, pv, sv, y, ew, uv, ew, ew as i32, eh as i32);
        }
        nv12
    }

    fn end(&mut self) {
        if !self.flags.stop.swap(true, Ordering::Relaxed) {
            if let Some(on_end) = self.flags.on_end.take() {
                on_end();
            }
        }
    }
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let pacer = Pacer::new(Duration::from_micros(1_000_000 / ctx.flags.out.prepared.fps as u64));
        Ok(Self { flags: ctx.flags, pacer, scratch: Vec::new(), last_preview: None, timing: Timing::new(), readback: Readback::new() })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        // Windows may deliver more often than we want to encode.
        let now = Instant::now();
        if !self.pacer.accept(now) {
            return Ok(());
        }

        let mut scratch = std::mem::take(&mut self.scratch);
        let (w, h) = self.readback.grab(frame, &mut scratch)?;
        let readback = now.elapsed();
        if w < 2 || h < 2 {
            self.scratch = scratch;
            return Ok(()); // a minimized window
        }
        let bgra: &[u8] = &scratch;
        if let Some(tap) = &self.flags.preview {
            if tap.on.load(Ordering::Relaxed) && !tap.busy.load(Ordering::Acquire) && self.last_preview.is_none_or(|t| now - t >= PREVIEW_EVERY) {
                self.last_preview = Some(now);
                // A full frame copied for the preview thread; skipped while that thread is still busy.
                let mut copy = std::mem::take(&mut *tap.spare.lock().unwrap());
                copy.clear();
                copy.extend_from_slice(bgra);
                tap.busy.store(true, Ordering::Release);
                if tap.tx.try_send((copy, w, h)).is_err() {
                    tap.busy.store(false, Ordering::Release);
                }
            }
        }
        // libyuv names formats by word order: "ARGB" is B,G,R,A in memory, i.e. BGRA bytes.
        let held = if self.flags.out.prepared.encoder.is_some() {
            Held::Nv12(Arc::new(self.nv12(bgra, w, h)))
        } else {
            let buffer = self.i420(bgra, w, h);
            Held::I420(VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us: 0, frame_metadata: None, buffer })
        };
        self.scratch = scratch;
        let converted = now.elapsed();
        self.flags.out.send(held);
        self.timing.add(readback, converted, now.elapsed(), &self.flags.out.refused);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        crate::log_line("screen capture: the shared window closed");
        self.end();
        Ok(())
    }
}

/// Starts capturing on a thread of its own, plus the thread that repeats a still picture.
pub fn start(target: Target) -> Result<(), String> {
    let Target { preview, prepared, stop, frames, on_end } = target;
    let preview = preview.map(|(on, show)| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, u32, u32)>(1);
        let (sent, sent_height) = (frames.clone(), prepared.height);
        let (busy, spare) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let (thread_busy, thread_spare) = (busy.clone(), spare.clone());
        let _ = std::thread::Builder::new().name("screen-preview".into()).spawn(move || {
            let (mut counted, mut since, mut fps) = (sent.load(Ordering::Relaxed), Instant::now(), 0u32);
            while let Ok((bgra, w, h)) = rx.recv() {
                let elapsed = since.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    let now = sent.load(Ordering::Relaxed);
                    fps = ((now - counted) as f64 / elapsed.as_secs_f64()).round() as u32;
                    (counted, since) = (now, Instant::now());
                }
                show(preview_pixels(&bgra, w, h), sent_height, fps);
                *thread_spare.lock().unwrap() = bgra;
                thread_busy.store(false, Ordering::Release);
            }
        });
        PreviewTap { on, tx, busy, spare }
    });
    let interval = Duration::from_micros(1_000_000 / prepared.fps as u64);
    let what = prepared.what;
    let out = Arc::new(Output { prepared, started: Instant::now(), frames, refused: AtomicU64::new(0), last: Mutex::new((None, Instant::now())) });

    let (repeat_out, repeat_stop) = (out.clone(), stop.clone());
    std::thread::Builder::new()
        .name("screen-repeat".into())
        .spawn(move || {
            while !repeat_stop.load(Ordering::Relaxed) {
                std::thread::sleep(REPEAT_EVERY / 2);
                repeat_out.repeat_if_still();
            }
        })
        .map_err(|e| e.to_string())?;

    let flags = Flags { preview, out, stop: stop.clone(), on_end };
    let cursor = CursorCaptureSettings::WithCursor;
    let border = DrawBorderSettings::WithoutBorder;
    // Windows is asked for somewhat more frames than needed (the pacer picks the ones to keep): asking for exactly the stream's rate lets its own timing drop frames and the rate sags.
    // Not for every frame of a fast screen: each one Windows hands over costs the GPU a copy, even when we throw it away.
    let update = MinimumUpdateIntervalSettings::Custom(interval * 3 / 4);
    // Settings is generic over the item type, so each kind builds its own.
    let run: Box<dyn FnOnce() -> Result<(), String> + Send> = match what {
        Source::Monitor(h) => {
            let item = Monitor::from_raw_hmonitor(h as *mut _);
            let settings = Settings::new(item, cursor, border, SecondaryWindowSettings::Default, update, DirtyRegionSettings::Default, ColorFormat::Bgra8, flags);
            Box::new(move || Capture::start(settings).map_err(|e| e.to_string()))
        }
        Source::Window(h) => {
            let item = Window::from_raw_hwnd(h as *mut _);
            // Menus and dialogs of the shared app show too.
            let settings = Settings::new(item, cursor, border, SecondaryWindowSettings::Include, update, DirtyRegionSettings::Default, ColorFormat::Bgra8, flags);
            Box::new(move || Capture::start(settings).map_err(|e| e.to_string()))
        }
    };
    std::thread::Builder::new()
        .name("screen-capture".into())
        .spawn(move || {
            if let Err(e) = run() {
                crate::log_line(&format!("screen capture stopped: {e}"));
                stop.store(true, Ordering::Relaxed); // and the repeat thread with it
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}
#[cfg(test)]
mod pacer_tests {
    use super::*;

    /// Frames kept in ten seconds of a screen that draws `hz` times a second, for a stream of `fps`.
    fn kept(hz: u32, fps: u32, jitter_us: i64) -> u32 {
        let start = Instant::now();
        let mut pacer = Pacer::new(Duration::from_micros(1_000_000 / fps as u64));
        let (mut n, mut seed) = (0, 12345u64);
        for i in 0..(hz * 10) {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let wobble = if jitter_us == 0 { 0 } else { ((seed >> 33) as i64 % (2 * jitter_us + 1)) - jitter_us };
            let at = (i as i64 * 1_000_000 / hz as i64 + wobble).max(0) as u64;
            if pacer.accept(start + Duration::from_micros(at)) {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn a_fast_screen_still_gives_the_full_rate() {
        // 144 and 165 Hz screens used to give 48 and 55 fps for a 60 fps stream.
        for hz in [60, 75, 120, 144, 165, 240] {
            let n = kept(hz, 60, 0);
            assert!((590..=610).contains(&n), "{hz} Hz: {n} frames in 10 s");
        }
    }

    #[test]
    fn a_wobbly_clock_does_not_lose_frames() {
        let n = kept(60, 60, 1500);
        assert!(n >= 570, "kept {n} of 600");
        let n = kept(144, 60, 1000);
        assert!((585..=615).contains(&n), "kept {n}");
    }

    #[test]
    fn a_slower_screen_keeps_everything() {
        assert_eq!(kept(30, 60, 0), 300);
        assert_eq!(kept(24, 30, 0), 240);
    }
}