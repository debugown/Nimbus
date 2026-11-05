//! Nimbus's own microphone and speakers, on WASAPI directly.
//!
//! WebRTC's audio module records from the Windows "communications" mic
//! whatever device is picked, and Windows treats that as a call: every other
//! sound gets turned down. So Nimbus opens the devices itself as ordinary
//! streams: the mic feeds a `NativeAudioSource`, other people's audio arrives
//! through `NativeAudioStream`s and is mixed here. Echo cancellation, noise
//! suppression and gain control come from WebRTC's audio processing module,
//! with the mixed playback as the echo reference.
//!
//! Everything runs at 48 kHz mono in 10 ms frames; WASAPI converts to and
//! from the device format.
//!
//! The sound of a screen share is here too: Windows' process loopback hands
//! us what everything else plays (never our own playback, so viewers don't
//! hear the call back), or only what one app plays when a window is shown.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::AudioSourceOptions;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::native::apm::AudioProcessingModule;
use livekit::webrtc::prelude::RtcAudioTrack;
use windows::core::{implement, Interface};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, EDataFlow, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, DEVICE_STATE_ACTIVE,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantToStringAlloc, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 10 ms frame, the unit WebRTC works in.
const FRAME: usize = (SAMPLE_RATE / 100) as usize;
/// How much of a speaker's audio may wait before the oldest is dropped,
/// so a hiccup never turns into lasting delay.
const MAX_QUEUED: usize = FRAME * 30;
/// A speaker's audio starts playing once this much has arrived (jitter cushion).
const START_QUEUED: usize = FRAME * 4;
/// Someone who keeps running dry in the middle of their sound (music has no pauses to hide it in) gets a bigger
/// cushion, a step at a time up to this. A pause in speech is longer than `UNDERRUN_WITHIN`, and changes nothing.
const MAX_CUSHION: usize = FRAME * 14;
const CUSHION_STEP: usize = FRAME * 2;
const UNDERRUN_WITHIN: Duration = Duration::from_millis(150);
/// WASAPI buffer length, in 100 ns units.
const DEVICE_BUFFER: i64 = 400_000;
/// Which of Windows' defaults is used: the ordinary one. A mic opened through
/// the communications role makes Windows duck every other sound.
const DEFAULT_ROLE: windows::Win32::Media::Audio::ERole = eConsole;
/// Rough mic-to-speaker path the echo canceller should expect.
const ECHO_DELAY_MS: i32 = 60;

/// How the mic is cleaned up and when it sends, and how loud things are:
/// the voice settings. One for the whole app; every engine reads it per frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Processing {
    /// 0 off, 1 WebRTC's standard suppression, 2 RNNoise (keeps speech, cuts the rest).
    pub noise: u8,
    /// How much of the cleaned sound is used with RNNoise, 20..100 percent (100: all of it).
    pub noise_strength: u32,
    pub echo: bool,
    pub auto_gain: bool,
    /// Percent; 100 leaves the level as it is.
    pub mic_volume: u32,
    pub speaker_volume: u32,
    /// Speech itself opens the mic; otherwise anything louder than `threshold` (dBFS).
    pub auto_threshold: bool,
    pub threshold: i32,
    /// 0 voice activity (the mic opens when it hears speech), 1 push to talk
    /// (the mic opens only while the key is held).
    pub voice_mode: u8,
    /// The push-to-talk key: a Windows virtual key code.
    pub ptt_key: i32,
    /// The modifiers that go with it: 1 Ctrl, 2 Shift, 4 Alt.
    pub ptt_mod: i32,
    /// Global keys that switch the microphone and the sound off and on (0: none).
    pub mute_key: i32,
    pub mute_mod: i32,
    pub deafen_key: i32,
    pub deafen_mod: i32,
}

impl Processing {
    pub const DEFAULT: Processing = Processing {
        noise: 2,
        noise_strength: 100,
        echo: true,
        auto_gain: true,
        mic_volume: 100,
        speaker_volume: 100,
        auto_threshold: true,
        threshold: -50,
        voice_mode: 0,
        ptt_key: VK_SPACE,
        ptt_mod: MOD_CONTROL,
        mute_key: 0,
        mute_mod: 0,
        deafen_key: 0,
        deafen_mod: 0,
    };

    pub fn from_json(v: &serde_json::Value) -> Self {
        let d = Self::DEFAULT;
        let int = |k: &str, def: i64| v[k].as_i64().unwrap_or(def);
        let flag = |k: &str, def: bool| v[k].as_bool().unwrap_or(def);
        Self {
            noise: int("noise", d.noise as i64).clamp(0, 2) as u8,
            noise_strength: int("noise_strength", 100).clamp(20, 100) as u32,
            echo: flag("echo", d.echo),
            auto_gain: flag("auto_gain", d.auto_gain),
            mic_volume: int("mic_volume", 100).clamp(0, 200) as u32,
            speaker_volume: int("speaker_volume", 100).clamp(0, 200) as u32,
            auto_threshold: flag("auto_threshold", d.auto_threshold),
            threshold: int("threshold", d.threshold as i64).clamp(-100, 0) as i32,
            voice_mode: int("voice_mode", 0).clamp(0, 1) as u8,
            ptt_key: {
                let v = int("ptt_key", VK_SPACE as i64);
                if (1..=255).contains(&v) { v as i32 } else { VK_SPACE }
            },
            ptt_mod: int("ptt_mod", MOD_CONTROL as i64).clamp(0, 7) as i32,
            mute_key: Some(int("mute_key", 0)).filter(|v| (1..=255).contains(v)).unwrap_or(0) as i32,
            mute_mod: int("mute_mod", 0).clamp(0, 7) as i32,
            deafen_key: Some(int("deafen_key", 0)).filter(|v| (1..=255).contains(v)).unwrap_or(0) as i32,
            deafen_mod: int("deafen_mod", 0).clamp(0, 7) as i32,
        }
    }

    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "noise": self.noise, "noise_strength": self.noise_strength, "echo": self.echo, "auto_gain": self.auto_gain,
            "mic_volume": self.mic_volume, "speaker_volume": self.speaker_volume,
            "auto_threshold": self.auto_threshold, "threshold": self.threshold,
            "voice_mode": self.voice_mode, "ptt_key": self.ptt_key, "ptt_mod": self.ptt_mod,
            "mute_key": self.mute_key, "mute_mod": self.mute_mod, "deafen_key": self.deafen_key, "deafen_mod": self.deafen_mod,
        })
    }
}

static PROCESSING: Mutex<Processing> = Mutex::new(Processing::DEFAULT);
/// The mic's latest loudness (dBFS × 10, before the gate) and whether it sends, for the settings meter.
static MIC_DB: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1000);
static GATE_OPEN: AtomicBool = AtomicBool::new(false);
/// Mic check: our own processed mic plays back through the speakers.
static LOOPBACK: AtomicBool = AtomicBool::new(false);

pub fn processing() -> Processing {
    *PROCESSING.lock().unwrap()
}

pub fn set_processing(p: Processing) {
    *PROCESSING.lock().unwrap() = p;
    PTT_MODE.store(p.voice_mode, Ordering::Relaxed);
    PTT_KEY.store(p.ptt_key, Ordering::Relaxed);
    PTT_MOD.store(p.ptt_mod, Ordering::Relaxed);
    HOT_KEYS[0].store(p.mute_key, Ordering::Relaxed);
    HOT_KEYS[1].store(p.mute_mod, Ordering::Relaxed);
    HOT_KEYS[2].store(p.deafen_key, Ordering::Relaxed);
    HOT_KEYS[3].store(p.deafen_mod, Ordering::Relaxed);
}

/// (dBFS, sending) of the mic right now.
pub fn meter() -> (f32, bool) {
    (MIC_DB.load(Ordering::Relaxed) as f32 / 10.0, GATE_OPEN.load(Ordering::Relaxed))
}

// ---------- Push to talk ----------

/// Virtual key codes we care about.
pub const VK_BACK: i32 = 0x08;
pub const VK_TAB: i32 = 0x09;
pub const VK_RETURN: i32 = 0x0D;
pub const VK_SHIFT: i32 = 0x10;
pub const VK_CONTROL: i32 = 0x11;
pub const VK_ALT: i32 = 0x12;
pub const VK_ESCAPE: i32 = 0x1B;
pub const VK_SPACE: i32 = 0x20;
pub const VK_DELETE: i32 = 0x2E;
pub const VK_LEFT: i32 = 0x25;
pub const VK_UP: i32 = 0x26;
pub const VK_RIGHT: i32 = 0x27;
pub const VK_DOWN: i32 = 0x28;
/// F1 is 0x70; F12 is 0x7B.
pub const VK_F1: i32 = 0x70;

/// The modifiers that go with the push-to-talk key.
pub const MOD_CONTROL: i32 = 1;
pub const MOD_SHIFT: i32 = 2;
pub const MOD_ALT: i32 = 4;

/// The push-to-talk key and its modifiers, as set in the voice settings.
static PTT_KEY: AtomicI32 = AtomicI32::new(VK_SPACE);
static PTT_MOD: AtomicI32 = AtomicI32::new(MOD_CONTROL);
/// 0 voice activity, 1 push to talk.
static PTT_MODE: AtomicU8 = AtomicU8::new(0);
/// Whether the key is down right now.
static PTT_HELD: AtomicBool = AtomicBool::new(false);

/// The global keys: mute key, its modifiers, deafen key, its modifiers.
static HOT_KEYS: [AtomicI32; 4] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
/// Which hotkey was pressed since the last look: 1 mute, 2 deafen.
static HOT_EVENT: AtomicU8 = AtomicU8::new(0);

/// The hotkey pressed since the last call, once (0 none, 1 mute, 2 deafen).
pub fn take_hotkey() -> u8 {
    HOT_EVENT.swap(0, Ordering::Relaxed)
}

/// Whether a key with its modifiers is held right now.
fn combo_down(key: i32, mods: i32) -> bool {
    let missing = (mods & MOD_CONTROL != 0 && !key_down(VK_CONTROL)) || (mods & MOD_SHIFT != 0 && !key_down(VK_SHIFT)) || (mods & MOD_ALT != 0 && !key_down(VK_ALT));
    !missing && key > 0 && key_down(key)
}

/// A hotkey's name for the settings: "Ctrl + M", or "Не задана".
pub fn hotkey_name(vk: i32, mods: i32) -> String {
    if vk == 0 { crate::i18n::t("Не задана") } else { key_name(vk, mods) }
}

/// Whether the mic is in push-to-talk mode.
pub fn ptt_mode() -> bool {
    PTT_MODE.load(Ordering::Relaxed) == 1
}

/// Whether the push-to-talk key is held right now.
pub fn ptt_held() -> bool {
    PTT_HELD.load(Ordering::Relaxed)
}

/// Whether the mic is open right now: in push-to-talk mode only while the
/// key is held.
pub fn mic_open() -> bool {
    !ptt_mode() || ptt_held()
}

/// Windows' own key state: it works even when another program has the focus.
fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 }
}

/// Watches the push-to-talk key in the background. Windows sends key presses
/// only to the focused window, but the key must work in any program.
pub fn watch_ptt() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            let mut was = [false, false];
            loop {
            // The mute and deafen keys: a press counts once, not for as long as it's held.
            for (i, event) in [(0usize, 1u8), (2, 2)] {
                let down = combo_down(HOT_KEYS[i].load(Ordering::Relaxed), HOT_KEYS[i + 1].load(Ordering::Relaxed));
                if down && !was[i / 2] {
                    HOT_EVENT.store(event, Ordering::Relaxed);
                }
                was[i / 2] = down;
            }
            let held = if ptt_mode() {
                let key = PTT_KEY.load(Ordering::Relaxed);
                let mods = PTT_MOD.load(Ordering::Relaxed);
                let missing = (mods & MOD_CONTROL != 0 && !key_down(VK_CONTROL))
                    || (mods & MOD_SHIFT != 0 && !key_down(VK_SHIFT))
                    || (mods & MOD_ALT != 0 && !key_down(VK_ALT));
                !missing && key > 0 && key_down(key)
            } else {
                false
            };
            PTT_HELD.store(held, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(10));
            }
        });
    });
}

/// A key as the interface reports it (a letter, a digit or a special key) as
/// a Windows virtual key code.
pub fn vk_for_key(key: &str) -> Option<i32> {
    let c = key.chars().next()?;
    let code = c as u32;
    Some(match code {
        0x08 => VK_BACK,
        0x09 => VK_TAB,
        0x0A | 0x0D => VK_RETURN,
        0x1B => VK_ESCAPE,
        0x20 => VK_SPACE,
        0x7F => VK_DELETE,
        0xF700 => VK_UP,
        0xF701 => VK_DOWN,
        0xF702 => VK_LEFT,
        0xF703 => VK_RIGHT,
        0xF704..=0xF70F => (code - 0xF704) as i32 + VK_F1,
        0x30..=0x39 | 0x41..=0x5A => code as i32,
        0x61..=0x7A => (code - 0x20) as i32,
        _ => return None,
    })
}

/// The key as it's shown in the settings: "Ctrl + Пробел".
pub fn key_name(vk: i32, mods: i32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods & MOD_CONTROL != 0 {
        parts.push("Ctrl");
    }
    if mods & MOD_SHIFT != 0 {
        parts.push("Shift");
    }
    if mods & MOD_ALT != 0 {
        parts.push("Alt");
    }
    let key = match vk {
        VK_SPACE => crate::i18n::t("Пробел"),
        VK_RETURN => "Enter".to_string(),
        VK_TAB => "Tab".to_string(),
        VK_BACK => "Backspace".to_string(),
        VK_ESCAPE => "Esc".to_string(),
        VK_DELETE => "Del".to_string(),
        VK_LEFT => crate::i18n::t("Стрелка влево"),
        VK_UP => crate::i18n::t("Стрелка вверх"),
        VK_RIGHT => crate::i18n::t("Стрелка вправо"),
        VK_DOWN => crate::i18n::t("Стрелка вниз"),
        0x30..=0x39 => char::from_u32(vk as u32).map(|c| c.to_string()).unwrap_or_default(),
        0x41..=0x5A => char::from_u32(vk as u32).map(|c| c.to_string()).unwrap_or_default(),
        0x70..=0x7B => format!("F{}", vk - 0x6F),
        _ => crate::i18n::tf("Клавиша {}", &[&vk]),
    };
    if parts.is_empty() {
        key
    } else {
        format!("{} + {}", parts.join(" + "), key)
    }
}

pub fn set_loopback(on: bool) {
    LOOPBACK.store(on, Ordering::Relaxed);
}

/// How loud each person is for us (percent, by user id; 0 = muted for us), as
/// in Discord's "user volume". Kept in `%APPDATA%\Nimbus Native\user_volumes.json`.
static USER_VOLUMES: std::sync::LazyLock<Mutex<HashMap<String, u32>>> = std::sync::LazyLock::new(|| {
    let saved = user_volumes_file().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    Mutex::new(saved.unwrap_or_default())
});

fn user_volumes_file() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Nimbus Native").join("user_volumes.json"))
}

/// Someone's volume for us, in percent (100 unless changed).
pub fn user_volume(user: &str) -> u32 {
    USER_VOLUMES.lock().unwrap().get(user).copied().unwrap_or(100)
}

pub fn set_user_volume(user: &str, percent: u32) {
    let mut volumes = USER_VOLUMES.lock().unwrap();
    if percent == 100 {
        volumes.remove(user);
    } else {
        volumes.insert(user.to_string(), percent.min(200));
    }
    if let Some(file) = user_volumes_file() {
        let _ = std::fs::write(file, serde_json::to_string(&*volumes).unwrap_or_default());
    }
}

/// Applies someone's volume to their sound; false when they're muted for us.
pub fn apply_user_volume(user: &str, samples: &mut [i16]) -> bool {
    let percent = user_volume(user);
    scale(samples, percent);
    percent > 0
}

/// Volume after RNNoise, from its speech probability: full while you talk,
/// fading to nothing within ~150 ms when you stop (keys, fans, breathing between words).
#[derive(Default)]
pub struct SpeechGain {
    gain: f32,
}

impl SpeechGain {
    /// The gain at the start and at the end of this frame (ramped, so no clicks).
    pub fn next(&mut self, voice: f32) -> (f32, f32) {
        let target = ((voice - 0.45) / 0.3).clamp(0.0, 1.0);
        let from = self.gain;
        // Opens in a few frames (about 30 ms), so a click that lasts one frame stays quiet
        // while the first syllable of a word still comes through; closes a bit slower.
        self.gain = if target > from { from + (target - from).min(0.34) } else { from + (target - from) * 0.3 };
        if self.gain < 0.01 {
            self.gain = 0.0;
        }
        (from, self.gain)
    }
}

/// RNNoise twice over, then silence between words: the strong noise suppression.
pub struct Cleaner {
    first: Box<nnnoiseless::DenoiseState<'static>>,
    second: Box<nnnoiseless::DenoiseState<'static>>,
    speech: SpeechGain,
}

impl Cleaner {
    pub fn new() -> Self {
        Self { first: nnnoiseless::DenoiseState::new(), second: nnnoiseless::DenoiseState::new(), speech: SpeechGain::default() }
    }

    /// Cleans one 10 ms frame in place; `strength` 0..1 mixes the original back in.
    /// Returns RNNoise's probability that the frame is speech.
    pub fn process(&mut self, chunk: &mut [i16], strength: f32) -> f32 {
        let mut input = [0f32; FRAME];
        let mut output = [0f32; FRAME];
        for (i, s) in input.iter_mut().zip(chunk.iter()) {
            *i = *s as f32;
        }
        // Two passes: the second takes out what the first leaves under speech.
        let probability = self.first.process_frame(&mut output, &input);
        let mut middle = output;
        self.second.process_frame(&mut output, &middle);
        middle.copy_from_slice(&input);
        // Between words everything fades out, as with Krisp.
        let (from, to) = self.speech.next(probability);
        // RNNoise's output is one frame behind its verdict: when the verdict is closing, the frame
        // being played must already be quiet from its very first sample.
        let from = from.min(to);
        for (k, (s, o)) in chunk.iter_mut().zip(&output).enumerate() {
            let g = from + (to - from) * k as f32 / FRAME as f32;
            let mixed = o * g * strength + middle[k] * (1.0 - strength);
            *s = mixed.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
        probability
    }
}

/// Frames the mic stays open after the last speech, so word ends aren't cut.
const GATE_HOLD: u32 = 30;

/// Decides, frame by frame, whether the mic sends (Discord's input sensitivity).
#[derive(Default)]
pub struct Gate {
    hold: u32,
    /// The room's noise level, learnt while no one talks (auto mode without RNNoise).
    floor: Option<f32>,
}

impl Gate {
    /// `voice`: RNNoise's speech probability, when it runs.
    pub fn update(&mut self, p: &Processing, db: f32, voice: Option<f32>) -> bool {
        let heard = if !p.auto_threshold {
            db >= p.threshold as f32
        } else if let Some(v) = voice {
            v > 0.5 && db > -65.0
        } else {
            let floor = self.floor.unwrap_or(db);
            // Falls at once, rises slowly (1 dB a second), so speech doesn't become the floor.
            let floor = if db < floor { db } else { floor + 0.01 };
            self.floor = Some(floor);
            db > (floor + 12.0).max(-60.0)
        };
        self.hold = if heard { GATE_HOLD } else { self.hold.saturating_sub(1) };
        self.hold > 0
    }
}

/// Loudness of a frame in dBFS (-100 for silence).
pub fn dbfs(samples: &[i16]) -> f32 {
    let power = samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / samples.len().max(1) as f64;
    if power < 1.0 {
        return -100.0;
    }
    (10.0 * (power / (32768.0 * 32768.0)).log10()).max(-100.0) as f32
}

fn scale(samples: &mut [i16], percent: u32) {
    if percent == 100 {
        return;
    }
    let k = percent as f32 / 100.0;
    for s in samples {
        *s = (*s as f32 * k).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

fn new_apm(p: &Processing) -> AudioProcessingModule {
    // RNNoise replaces WebRTC's own suppression rather than stacking on it.
    let mut apm = AudioProcessingModule::new(p.echo, p.auto_gain, true, p.noise == 1);
    let _ = apm.set_stream_delay_ms(ECHO_DELAY_MS);
    apm
}

/// A microphone or speaker as the device menus show it.
#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Active devices of one kind, and the id of Windows' default one.
pub fn devices(capture: bool) -> (Vec<Device>, Option<String>) {
    let flow = if capture { eCapture } else { eRender };
    std::thread::spawn(move || unsafe { list(flow) }).join().ok().and_then(|r| r.ok()).unwrap_or_default()
}

unsafe fn list(flow: EDataFlow) -> windows::core::Result<(Vec<Device>, Option<String>)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default = enumerator.GetDefaultAudioEndpoint(flow, DEFAULT_ROLE).ok().and_then(|d| device_id(&d));
        let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..collection.GetCount()? {
            let Ok(device) = collection.Item(i) else { continue };
            let Some(id) = device_id(&device) else { continue };
            let name = friendly_name(&device).unwrap_or_else(|| id.clone());
            out.push(Device { id, name });
        }
        Ok((out, default))
    }
}

unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let raw = device.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        id
    }
}

unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let raw = PropVariantToStringAlloc(&value).ok()?;
        let name = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        name
    }
}

/// 48 kHz 16-bit PCM with this many channels.
fn pcm(channels: u16) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 2 * channels as u32,
        nBlockAlign: 2 * channels,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Opens a device (by id, or Windows' default) as a 48 kHz mono 16-bit stream.
unsafe fn open(flow: EDataFlow, id: Option<&str>) -> windows::core::Result<IAudioClient> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = match id {
            Some(id) => enumerator.GetDevice(&windows::core::HSTRING::from(id))?,
            None => enumerator.GetDefaultAudioEndpoint(flow, DEFAULT_ROLE)?,
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        // An ordinary stream (not the communications category): no ducking.
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, DEVICE_BUFFER, 0, &pcm(1), None)?;
        Ok(client)
    }
}

/// Whose sound a screen share carries.
#[derive(Clone, Copy, Debug)]
pub enum Scope {
    /// Everything except Nimbus itself (a whole screen).
    AllButUs,
    /// One app and the processes it started (a window).
    App(u32),
}

/// Tells the activating thread that Windows has finished.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(std::sync::mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// An audio client on Windows' process loopback, opened as 48 kHz stereo.
unsafe fn open_loopback(scope: Scope) -> windows::core::Result<IAudioClient> {
    unsafe {
        let (pid, mode) = match scope {
            Scope::AllButUs => (std::process::id(), PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
            Scope::App(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        };
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
            },
        };
        // A blob pointing at `params`; never cleared, since Windows doesn't own that memory.
        let mut prop = ManuallyDrop::new(PROPVARIANT::default());
        let inner = &mut prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob =
            BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: &params as *const _ as *mut u8 };

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
        let operation =
            ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop as *const _), &handler)?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))?;
        let (mut result, mut unknown) = (windows::core::HRESULT(0), None);
        operation.GetActivateResult(&mut result, &mut unknown)?;
        result.ok()?;
        let client: IAudioClient = unknown.ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))?.cast()?;
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, DEVICE_BUFFER, 0, &pcm(2), None)?;
        Ok(client)
    }
}

/// The sound of a screen share, flowing into `source` until dropped.
pub struct SystemSound {
    _worker: Worker,
    pub frames: Arc<AtomicU64>,
    pub peak: Arc<AtomicU64>,
}

impl SystemSound {
    /// `source` must be 48 kHz stereo; `tap` gets a copy of every 10 ms chunk (for Discord). Call inside the tokio runtime.
    pub fn start(scope: Scope, source: NativeAudioSource, tap: Option<crate::golive_send::SoundTap>) -> Self {
        let (frames, peak) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (f, p) = (frames.clone(), peak.clone());
        let runtime = tokio::runtime::Handle::current();
        let worker = Worker::spawn("system-sound", move |stop| {
            if let Err(e) = unsafe { loopback(scope, &source, tap.as_ref(), &runtime, &stop, &f, &p) } {
                crate::log_line(&format!("stream sound stopped: {e}"));
            }
        });
        Self { _worker: worker, frames, peak }
    }
}

unsafe fn loopback(
    scope: Scope,
    source: &NativeAudioSource,
    tap: Option<&crate::golive_send::SoundTap>,
    runtime: &tokio::runtime::Handle,
    stop: &AtomicBool,
    frames: &AtomicU64,
    peak: &AtomicU64,
) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = open_loopback(scope)?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        crate::log_line(&format!("stream sound open: {scope:?}"));
        // Windows sends nothing while nothing plays; viewers still get a steady
        // 10 ms beat (silence), so the sound doesn't stutter when it starts.
        let stereo_frame = FRAME * 2;
        let started = Instant::now();
        let mut sent: u64 = 0;
        let mut pending: VecDeque<i16> = VecDeque::new();
        let result = (|| -> windows::core::Result<()> {
            while !stop.load(Ordering::Relaxed) {
                let _ = WaitForSingleObject(event, 10);
                while capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    capture.GetBuffer(&mut data, &mut count, &mut flags, None, None)?;
                    let samples = count as usize * 2;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        pending.extend(std::iter::repeat_n(0, samples));
                    } else {
                        pending.extend(std::slice::from_raw_parts(data as *const i16, samples));
                    }
                    capture.ReleaseBuffer(count)?;
                }
                // Never let the sound fall behind the picture.
                let extra = pending.len().saturating_sub(stereo_frame * 10);
                pending.drain(..extra);
                let due = started.elapsed().as_millis() as u64 / 10;
                while sent < due {
                    let take = stereo_frame.min(pending.len());
                    let mut chunk: Vec<i16> = pending.drain(..take).collect();
                    chunk.resize(stereo_frame, 0);
                    note_peak(peak, &chunk);
                    if let Some(tap) = tap {
                        let _ = tap.send(chunk.clone());
                    }
                    let frame = AudioFrame { data: chunk.into(), sample_rate: SAMPLE_RATE, num_channels: 2, samples_per_channel: FRAME as u32 };
                    let _ = runtime.block_on(source.capture_frame(&frame));
                    frames.fetch_add(1, Ordering::Relaxed);
                    sent += 1;
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        let _ = CloseHandle(event);
        result
    }
}

/// Other people's audio waiting to be played, one queue per track.
#[derive(Default)]
struct Queue {
    samples: VecDeque<i16>,
    playing: bool,
    /// For a stream's sound: whose stream. It plays only while that stream is watched.
    stream_of: Option<String>,
    /// How much must wait before it starts, once it has run dry mid-sound; 0: the usual.
    cushion: usize,
    /// When it last ran dry.
    dry_at: Option<Instant>,
}

impl Queue {
    /// Before a frame is mixed: starts playing when enough has come. Coming back quickly means it wasn't a pause.
    fn try_start(&mut self, now: Instant) {
        if self.playing {
            return;
        }
        let need = self.cushion.max(START_QUEUED);
        if self.samples.len() < need {
            return;
        }
        self.playing = true;
        if self.dry_at.take().is_some_and(|at| now.duration_since(at) < UNDERRUN_WITHIN) {
            self.cushion = (need + CUSHION_STEP).min(MAX_CUSHION);
        }
    }

    fn ran_dry(&mut self, now: Instant) {
        self.playing = false;
        self.dry_at = Some(now);
    }
}

/// Counters for checks.
#[derive(Default)]
pub struct Stats {
    pub mic_frames: AtomicU64,
    pub mic_peak: AtomicU64,
    /// Peak of the latest 10 ms from the mic, after processing.
    pub mic_level: AtomicU64,
    pub played_frames: AtomicU64,
    pub remote_frames: AtomicU64,
    pub remote_peak: AtomicU64,
}

/// Our processed mic sound for a second listener (Discord's voice), which
/// takes it at its own pace.
#[derive(Default)]
pub struct Tap {
    samples: Mutex<VecDeque<i16>>,
}

impl Tap {
    fn push(&self, chunk: &[i16]) {
        let mut samples = self.samples.lock().unwrap();
        samples.extend(chunk);
        // Talking needs low delay: anything older than 100 ms is dropped.
        let extra = samples.len().saturating_sub(FRAME * 10);
        samples.drain(..extra);
    }

    /// Moves up to `out.len()` samples into `out`; returns how many.
    pub fn take(&self, out: &mut [i16]) -> usize {
        let mut samples = self.samples.lock().unwrap();
        let n = out.len().min(samples.len());
        for (o, s) in out.iter_mut().zip(samples.drain(..n)) {
            *o = s;
        }
        n
    }
}

/// Short sounds for what happens in a call, like Discord's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cue {
    /// We joined / left the call.
    Join,
    Leave,
    /// Our mic (or sound) went off / on.
    Mute,
    Unmute,
    /// Someone else joined / left.
    PeerJoin,
    PeerLeave,
}

impl Cue {
    /// The file that replaces this sound, in `sounds_dir()`.
    pub const fn file_name(self) -> &'static str {
        match self {
            Cue::Join => "join.wav",
            Cue::Leave => "leave.wav",
            Cue::Mute => "mute.wav",
            Cue::Unmute => "unmute.wav",
            Cue::PeerJoin => "user_join.wav",
            Cue::PeerLeave => "user_leave.wav",
        }
    }
}

/// Where your own call sounds go: `%APPDATA%\Nimbus Native\sounds`.
pub fn sounds_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Nimbus Native").join("sounds"))
}

/// Longest call sound played; longer files are cut.
const MAX_CUE: usize = SAMPLE_RATE as usize * 3;

/// The sound for a cue: your file if there is one, else Nimbus's own.
pub fn cue_sound(cue: Cue) -> Vec<i16> {
    sounds_dir().and_then(|dir| load_wav(&std::fs::read(dir.join(cue.file_name())).ok()?)).unwrap_or_else(|| cue_samples(cue))
}

/// One soft note: a sine gliding gently from one pitch to another, fading in
/// over 18 ms and out slowly, with only a trace of its octave for warmth.
fn bloop(out: &mut Vec<i16>, from_hz: f32, to_hz: f32, ms: u32, volume: f32) {
    let n = (SAMPLE_RATE * ms / 1000) as usize;
    let attack = (SAMPLE_RATE as usize * 18 / 1000).max(1);
    let release = (SAMPLE_RATE as usize * 40 / 1000).max(1);
    let decay = n as f32 / 2.2;
    let mut phase = 0f32;
    for i in 0..n {
        let t = i as f32 / n as f32;
        let hz = from_hz * (to_hz / from_hz).powf(t.min(0.6) / 0.6);
        phase += hz * std::f32::consts::TAU / SAMPLE_RATE as f32;
        let mut envelope = (-(i as f32) / decay).exp();
        envelope *= (i as f32 / attack as f32).min(1.0);
        envelope *= ((n - 1 - i) as f32 / release as f32).min(1.0);
        let wave = phase.sin() + 0.04 * (2.0 * phase).sin();
        out.push((wave / 1.04 * envelope * volume * i16::MAX as f32) as i16);
    }
}

/// Nimbus's own call sounds as 48 kHz mono: quiet, low, gliding notes, rising
/// for joining and switching on, falling for leaving and switching off.
pub fn cue_samples(cue: Cue) -> Vec<i16> {
    let mut out = Vec::new();
    let gap = |out: &mut Vec<i16>, ms: u32| out.extend(std::iter::repeat_n(0, (SAMPLE_RATE * ms / 1000) as usize));
    match cue {
        Cue::Join => {
            bloop(&mut out, 330.0, 392.0, 130, 0.17);
            gap(&mut out, 10);
            bloop(&mut out, 392.0, 523.0, 210, 0.17);
        }
        Cue::Leave => {
            bloop(&mut out, 523.0, 392.0, 130, 0.17);
            gap(&mut out, 10);
            bloop(&mut out, 392.0, 330.0, 210, 0.17);
        }
        Cue::Mute => bloop(&mut out, 392.0, 294.0, 140, 0.15),
        Cue::Unmute => bloop(&mut out, 294.0, 392.0, 140, 0.15),
        Cue::PeerJoin => bloop(&mut out, 440.0, 587.0, 170, 0.12),
        Cue::PeerLeave => bloop(&mut out, 587.0, 440.0, 170, 0.12),
    }
    out
}

/// A WAV file (16-bit PCM or 32-bit float, any rate, mono or stereo) as
/// 48 kHz mono samples, at most three seconds.
pub fn load_wav(bytes: &[u8]) -> Option<Vec<i16>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut format, mut channels, mut rate, mut bits, mut data) = (0u16, 0u16, 0u32, 0u16, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = bytes.get(at + 8..(at + 8 + size).min(bytes.len()))?;
        if id == b"fmt " && body.len() >= 16 {
            format = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
            bits = u16::from_le_bytes([body[14], body[15]]);
            if format == 0xfffe && body.len() >= 26 {
                format = u16::from_le_bytes([body[24], body[25]]); // WAVE_FORMAT_EXTENSIBLE: the real format
            }
        } else if id == b"data" {
            data = Some(body);
        }
        at += 8 + size + (size & 1);
    }
    let (data, channels) = (data?, channels.max(1) as usize);
    let frames: Vec<f32> = match (format, bits) {
        (1, 16) => data.chunks_exact(2 * channels).map(|f| f.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0).sum::<f32>() / channels as f32).collect(),
        (3, 32) => data.chunks_exact(4 * channels).map(|f| f.chunks_exact(4).map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]])).sum::<f32>() / channels as f32).collect(),
        _ => return None,
    };
    if frames.is_empty() || rate == 0 {
        return None;
    }
    // Linear resampling to 48 kHz is plenty for short sounds.
    let out_len = ((frames.len() as u64 * SAMPLE_RATE as u64 / rate as u64) as usize).min(MAX_CUE);
    let step = rate as f64 / SAMPLE_RATE as f64;
    Some(
        (0..out_len)
            .map(|i| {
                let pos = i as f64 * step;
                let (j, frac) = (pos as usize, (pos.fract()) as f32);
                let a = frames[j.min(frames.len() - 1)];
                let b = frames[(j + 1).min(frames.len() - 1)];
                ((a + (b - a) * frac).clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            })
            .collect(),
    )
}

/// Lets audio from elsewhere (Discord's voice) into the mix.
#[derive(Clone)]
pub struct Sink(Arc<Shared>);

impl Sink {
    pub fn push(&self, key: &str, samples: &[i16]) {
        let mut queues = self.0.queues.lock().unwrap();
        let queue = queues.entry(key.to_string()).or_default();
        queue.samples.extend(samples);
        let extra = queue.samples.len().saturating_sub(MAX_QUEUED);
        queue.samples.drain(..extra);
    }

    pub fn forget(&self, key: &str) {
        self.0.queues.lock().unwrap().remove(key);
    }

    /// A whole sound at once (call sounds, the soundboard): kept in full, up to `max` samples.
    fn push_sound(&self, key: &str, samples: &[i16], max: usize) {
        let mut queues = self.0.queues.lock().unwrap();
        let queue = queues.entry(key.to_string()).or_default();
        queue.samples.extend(samples);
        let extra = queue.samples.len().saturating_sub(max);
        queue.samples.drain(..extra);
    }
}

/// WebRTC's audio processing is single-threaded C++; the mutex around it
/// is the only way it is reached.
struct Apm(AudioProcessingModule);
unsafe impl Send for Apm {}

struct Shared {
    source: NativeAudioSource,
    apm: Mutex<Apm>,
    queues: Mutex<HashMap<String, Queue>>,
    deafened: AtomicBool,
    /// Identity of the person whose stream is open, if any.
    watched: Mutex<Option<String>>,
    tap: Mutex<Option<Arc<Tap>>>,
    runtime: tokio::runtime::Handle,
    stats: Stats,
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(name: &str, run: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || run(flag)).ok();
        Self { stop, thread }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The mic and speakers of one call.
pub struct Engine {
    shared: Arc<Shared>,
    mic: Option<Worker>,
    speaker: Option<Worker>,
    listeners: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl Engine {
    /// Must be called inside the tokio runtime (the mic thread hands frames back to it).
    pub fn start(mic: Option<String>, speaker: Option<String>) -> Self {
        // LiveKit's own processing is off: ours sees the playback, so it can cancel echo.
        let options = AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false };
        let shared = Arc::new(Shared {
            source: NativeAudioSource::new(options, SAMPLE_RATE, 1, 0),
            apm: Mutex::new(Apm(new_apm(&processing()))),
            queues: Mutex::default(),
            deafened: AtomicBool::new(false),
            watched: Mutex::new(None),
            tap: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
            stats: Stats::default(),
        });
        // The folder for your own call sounds, so it's there to find.
        if let Some(dir) = sounds_dir() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut engine = Self { shared, mic: None, speaker: None, listeners: HashMap::new() };
        engine.set_mic(mic);
        engine.set_speaker(speaker);
        engine
    }

    pub fn source(&self) -> NativeAudioSource {
        self.shared.source.clone()
    }

    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// A copy of our mic sound, for a second destination.
    pub fn tap(&self) -> Arc<Tap> {
        let tap = Arc::new(Tap::default());
        *self.shared.tap.lock().unwrap() = Some(tap.clone());
        tap
    }

    pub fn sink(&self) -> Sink {
        Sink(self.shared.clone())
    }

    /// Plays a call sound through our speakers (not sent to anyone).
    pub fn cue(&self, cue: Cue) {
        self.sink().push_sound("cue", &cue_sound(cue), MAX_CUE);
    }

    /// A soundboard sound played in the call, mixed with the voices (deafening silences it too). A few
    /// lanes take turns, so sounds played together are heard together, not one after another.
    pub fn play_sound(&self, samples: &[i16]) {
        static LANE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let lane = LANE.fetch_add(1, Ordering::Relaxed) % 4;
        self.sink().push_sound(&format!("soundboard{lane}"), samples, SAMPLE_RATE as usize * 8);
    }

    /// Whether the mic hears speech right now (for our own speaking ring,
    /// which shouldn't wait for the server's verdict).
    pub fn level_probe(&self) -> impl Fn() -> u64 + Send + 'static {
        let shared = self.shared.clone();
        move || shared.stats.mic_level.load(Ordering::Relaxed)
    }

    /// Switches the microphone (None = Windows' default), mid-call too.
    pub fn set_mic(&mut self, id: Option<String>) {
        self.mic = None; // stops the old one first
        let shared = self.shared.clone();
        self.mic = Some(Worker::spawn("mic", move |stop| {
            if let Err(e) = unsafe { record(&shared, id.as_deref(), &stop) } {
                crate::log_line(&format!("mic stopped: {e}"));
            }
        }));
    }

    /// Switches the speakers (None = Windows' default), mid-call too.
    pub fn set_speaker(&mut self, id: Option<String>) {
        self.speaker = None;
        let shared = self.shared.clone();
        self.speaker = Some(Worker::spawn("speaker", move |stop| {
            if let Err(e) = unsafe { play(&shared, id.as_deref(), &stop) } {
                crate::log_line(&format!("speaker stopped: {e}"));
            }
        }));
    }

    /// Deafened: nothing is played (the mic is muted on the track itself).
    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
    }

    /// The stream whose sound plays (None: no stream sound at all).
    pub fn set_watched(&self, identity: Option<String>) {
        *self.shared.watched.lock().unwrap() = identity;
    }

    /// Starts playing someone's audio track; `stream_of` marks a stream's sound.
    /// `user`: whose voice it is, for their own volume.
    pub fn listen(&mut self, sid: String, track: RtcAudioTrack, stream_of: Option<String>, user: Option<String>) {
        let shared = self.shared.clone();
        let key = sid.clone();
        // A stream's sound has its own volume, kept under "stream:<identity>".
        let user = user.or_else(|| stream_of.as_ref().map(|s| format!("stream:{s}")));
        let task = tokio::spawn(async move {
            let mut stream = NativeAudioStream::new(track, SAMPLE_RATE as i32, 1);
            while let Some(frame) = stream.next().await {
                shared.stats.remote_frames.fetch_add(1, Ordering::Relaxed);
                note_peak(&shared.stats.remote_peak, &frame.data);
                let mut data = frame.data.to_vec();
                if let Some(user) = &user {
                    if !apply_user_volume(user, &mut data) {
                        continue;
                    }
                }
                let mut queues = shared.queues.lock().unwrap();
                let queue = queues.entry(key.clone()).or_insert_with(|| Queue { stream_of: stream_of.clone(), ..Default::default() });
                queue.samples.extend(data.iter());
                let extra = queue.samples.len().saturating_sub(MAX_QUEUED);
                queue.samples.drain(..extra);
            }
        });
        if let Some(old) = self.listeners.insert(sid, task) {
            old.abort();
        }
    }

    pub fn forget(&mut self, sid: &str) {
        if let Some(task) = self.listeners.remove(sid) {
            task.abort();
        }
        self.shared.queues.lock().unwrap().remove(sid);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        for (_, task) in self.listeners.drain() {
            task.abort();
        }
    }
}

fn note_peak(peak: &AtomicU64, samples: &[i16]) {
    let top = samples.iter().map(|s| s.unsigned_abs() as u64).max().unwrap_or(0);
    peak.fetch_max(top, Ordering::Relaxed);
}

unsafe fn record(shared: &Shared, id: Option<&str>, stop: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = open(eCapture, id)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        crate::log_line(&format!("mic open: {}", id.unwrap_or("default")));
        let mut pending: Vec<i16> = Vec::with_capacity(FRAME * 4);
        let mut applied = processing();
        let mut denoise: Option<Cleaner> = None;
        let mut gate = Gate::default();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    pending.extend(std::iter::repeat_n(0, frames as usize));
                } else {
                    pending.extend_from_slice(std::slice::from_raw_parts(data as *const i16, frames as usize));
                }
                capture.ReleaseBuffer(frames)?;
            }
            while pending.len() >= FRAME {
                let mut chunk: Vec<i16> = pending.drain(..FRAME).collect();
                let p = processing();
                if (p.echo, p.auto_gain, p.noise == 1) != (applied.echo, applied.auto_gain, applied.noise == 1) {
                    shared.apm.lock().unwrap().0 = new_apm(&p);
                }
                applied = p;
                scale(&mut chunk, p.mic_volume);
                let _ = shared.apm.lock().unwrap().0.process_stream(&mut chunk, SAMPLE_RATE as i32, 1);
                let voice = if p.noise == 2 {
                    let cleaner = denoise.get_or_insert_with(Cleaner::new);
                    Some(cleaner.process(&mut chunk, p.noise_strength as f32 / 100.0))
                } else {                    denoise = None;
                    None
                };
                let db = dbfs(&chunk);
                let mut open = gate.update(&p, db, voice);
                // Push to talk: the mic is shut unless the key is held.
                let shut = ptt_mode() && !ptt_held();
                if shut {
                    open = false;
                }
                MIC_DB.store(if shut { -1000 } else { (db * 10.0) as i64 }, Ordering::Relaxed);
                GATE_OPEN.store(open, Ordering::Relaxed);
                // Below the sensitivity: nothing is sent, as in Discord.
                if !open {
                    chunk.fill(0);
                }
                if LOOPBACK.load(Ordering::Relaxed) {
                    let mut queues = shared.queues.lock().unwrap();
                    let queue = queues.entry("mictest".into()).or_default();
                    queue.samples.extend(&chunk);
                    let extra = queue.samples.len().saturating_sub(MAX_QUEUED);
                    queue.samples.drain(..extra);
                }
                let count = shared.stats.mic_frames.fetch_add(1, Ordering::Relaxed) + 1;
                note_peak(&shared.stats.mic_peak, &chunk);
                let level = chunk.iter().map(|s| s.unsigned_abs() as u64).max().unwrap_or(0);
                shared.stats.mic_level.store(level, Ordering::Relaxed);
                if count == 500 {
                    crate::log_line(&format!("mic level after 5 s: peak {}", shared.stats.mic_peak.load(Ordering::Relaxed)));
                }
                // During the microphone check nothing leaves this PC: nobody in the call hears it.
                if LOOPBACK.load(Ordering::Relaxed) {
                    continue;
                }
                if let Some(tap) = shared.tap.lock().unwrap().as_ref() {
                    tap.push(&chunk);
                }
                let frame = AudioFrame { data: chunk.into(), sample_rate: SAMPLE_RATE, num_channels: 1, samples_per_channel: FRAME as u32 };
                let _ = shared.runtime.block_on(shared.source.capture_frame(&frame));
            }
        }
        MIC_DB.store(-1000, Ordering::Relaxed);
        GATE_OPEN.store(false, Ordering::Relaxed);
        let _ = client.Stop();
        Ok(())
    }
}

/// Fills one 10 ms frame with everyone's audio, mixed.
fn mix(shared: &Shared, out: &mut [i16]) {
    let mut sum = [0i32; FRAME];
    let now = Instant::now();
    let watched = shared.watched.lock().unwrap().clone();
    // During the microphone check only the check is heard, as if the headphones were off for the call.
    let testing = LOOPBACK.load(Ordering::Relaxed);
    for (key, queue) in shared.queues.lock().unwrap().iter_mut() {
        if testing && key != "mictest" {
            queue.samples.clear();
            queue.playing = false;
            continue;
        }
        // A stream's sound nobody is watching: thrown away, not saved for later.
        if queue.stream_of.is_some() && queue.stream_of != watched {
            queue.samples.clear();
            queue.playing = false;
            continue;
        }
        queue.try_start(now);
        if !queue.playing {
            continue;
        }
        for (s, v) in sum.iter_mut().zip(queue.samples.drain(..FRAME.min(queue.samples.len()))) {
            *s += v as i32;
        }
        // Ran dry: wait for a fresh cushion instead of crackling.
        if queue.samples.is_empty() {
            queue.ran_dry(now);
        }
    }
    let deafened = shared.deafened.load(Ordering::Relaxed) && !testing;
    let volume = processing().speaker_volume as i32;
    for (o, s) in out.iter_mut().zip(sum) {
        *o = if deafened { 0 } else { (s * volume / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16 };
    }
}

unsafe fn play(shared: &Shared, id: Option<&str>, stop: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = open(eRender, id)?;
        let render: IAudioRenderClient = client.GetService()?;
        let size = client.GetBufferSize()?;
        client.Start()?;
        crate::log_line(&format!("speaker open: {} (buffer {size} samples)", id.unwrap_or("default")));
        let mut frame = [0i16; FRAME];
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            let mut free = size.saturating_sub(client.GetCurrentPadding()?) as usize;
            while free >= FRAME {
                mix(shared, &mut frame);
                // The echo canceller learns what the speakers play.
                let _ = shared.apm.lock().unwrap().0.process_reverse_stream(&mut frame.clone(), SAMPLE_RATE as i32, 1);
                let data = render.GetBuffer(FRAME as u32)?;
                std::ptr::copy_nonoverlapping(frame.as_ptr(), data as *mut i16, FRAME);
                render.ReleaseBuffer(FRAME as u32, 0)?;
                shared.stats.played_frames.fetch_add(1, Ordering::Relaxed);
                free -= FRAME;
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// A sound played on its own (a voice message), outside any call. Stops when dropped.
pub struct Playback {
    _worker: Worker,
    /// Samples handed to the speakers so far, counted from the start of the sound.
    position: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}

impl Playback {
    /// Plays `samples` (48 kHz mono) from `from` on the speakers (None = Windows' default).
    pub fn start(samples: Arc<Vec<i16>>, from: usize, speaker: Option<String>) -> Self {
        let position = Arc::new(AtomicU64::new(from as u64));
        let done = Arc::new(AtomicBool::new(false));
        let (at, finished) = (position.clone(), done.clone());
        let worker = Worker::spawn("clip", move |stop| {
            let played = unsafe { play_samples(&samples, &at, speaker.as_deref(), &stop) };
            // A speaker that is gone: Windows' default instead.
            let played = match played {
                Err(_) if speaker.is_some() => unsafe { play_samples(&samples, &at, None, &stop) },
                other => other,
            };
            if let Err(e) = played {
                crate::log_line(&format!("voice message playback stopped: {e}"));
            }
            finished.store(true, Ordering::Relaxed);
        });
        Self { _worker: worker, position, done }
    }

    /// Where the sound is now, in samples.
    pub fn position(&self) -> usize {
        self.position.load(Ordering::Relaxed) as usize
    }

    /// The end was reached (or the speakers failed).
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// The position and end, for watching from another thread.
    pub fn watch(&self) -> (Arc<AtomicU64>, Arc<AtomicBool>) {
        (self.position.clone(), self.done.clone())
    }
}

/// Whether the microphone check of the voice settings is on (the mic is busy with it).
pub fn mic_testing() -> bool {
    LOOPBACK.load(Ordering::Relaxed)
}

/// A voice message being recorded from the mic, outside any call. The mic is let go when it is
/// dropped, when `finish` is called, or once `max` samples are in.
pub struct Recorder {
    worker: Option<Worker>,
    samples: Arc<Mutex<Vec<i16>>>,
    failed: Arc<AtomicBool>,
    count: Arc<AtomicU64>,
}

impl Recorder {
    /// Records 48 kHz mono from `mic` (None = Windows' default; a mic that is gone falls back to it).
    pub fn start(mic: Option<String>, max: usize) -> Self {
        let samples = Arc::new(Mutex::new(Vec::with_capacity(SAMPLE_RATE as usize * 60)));
        let failed = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicU64::new(0));
        let (into, broken, counted) = (samples.clone(), failed.clone(), count.clone());
        let worker = Worker::spawn("voice-message", move |stop| {
            let recorded = unsafe { record_samples(&into, &counted, max, mic.as_deref(), &stop) };
            let recorded = match recorded {
                Err(_) if mic.is_some() && into.lock().unwrap().is_empty() => unsafe { record_samples(&into, &counted, max, None, &stop) },
                other => other,
            };
            if let Err(e) = recorded {
                crate::log_line(&format!("voice message recording stopped: {e}"));
                broken.store(true, Ordering::Relaxed);
            }
        });
        Self { worker: Some(worker), samples, failed, count }
    }

    /// Samples recorded so far, and whether the mic failed, for watching from another thread.
    pub fn progress(&self) -> (Arc<AtomicU64>, Arc<AtomicBool>) {
        (self.count.clone(), self.failed.clone())
    }

    /// The mic could not be opened (or stopped working).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Stops recording, lets the mic go, and gives what was recorded.
    pub fn finish(mut self) -> Vec<i16> {
        self.worker = None;
        std::mem::take(&mut *self.samples.lock().unwrap())
    }
}

/// Takes the mic's sound into `into` (at the mic volume of the settings) until `stop` or `max` samples.
unsafe fn record_samples(into: &Mutex<Vec<i16>>, count: &AtomicU64, max: usize, id: Option<&str>, stop: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = open(eCapture, id)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        crate::log_line("voice message: mic open");
        let result = (|| -> windows::core::Result<()> {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                while capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let mut chunk = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        vec![0i16; frames as usize]
                    } else {
                        std::slice::from_raw_parts(data as *const i16, frames as usize).to_vec()
                    };
                    capture.ReleaseBuffer(frames)?;
                    scale(&mut chunk, processing().mic_volume);
                    let mut all = into.lock().unwrap();
                    let room = max.saturating_sub(all.len());
                    all.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    count.store(all.len() as u64, Ordering::Relaxed);
                }
                if into.lock().unwrap().len() >= max {
                    break;
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        result
    }
}

/// Feeds `samples` from `at` to a speaker until the end or `stop`, at the speaker volume of the settings.
unsafe fn play_samples(samples: &[i16], at: &AtomicU64, id: Option<&str>, stop: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = open(eRender, id)?;
        let render: IAudioRenderClient = client.GetService()?;
        let size = client.GetBufferSize()?;
        client.Start()?;
        let mut next = at.load(Ordering::Relaxed) as usize;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            let padding = client.GetCurrentPadding()? as usize;
            if next >= samples.len() {
                // Everything is handed over: wait for the speakers to play it out.
                if padding == 0 {
                    break;
                }
                continue;
            }
            let free = (size as usize).saturating_sub(padding).min(samples.len() - next);
            if free == 0 {
                continue;
            }
            let volume = processing().speaker_volume as i32;
            let data = render.GetBuffer(free as u32)? as *mut i16;
            for (i, &s) in samples[next..next + free].iter().enumerate() {
                *data.add(i) = (s as i32 * volume / 100).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            render.ReleaseBuffer(free as u32, 0)?;
            next += free;
            // What is heard now lags what was handed over by the samples still waiting in the buffer.
            at.store(next.saturating_sub(padding + free) as u64, Ordering::Relaxed);
        }
        if next >= samples.len() {
            at.store(samples.len() as u64, Ordering::Relaxed);
        }
        let _ = client.Stop();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_dry_mid_sound_grows_the_cushion_but_a_pause_does_not() {
        let start = Instant::now();
        let mut q = Queue::default();
        q.samples.extend(vec![0i16; START_QUEUED]);
        q.try_start(start);
        assert!(q.playing);
        assert_eq!(q.cushion, 0, "the first start changes nothing");

        // Dry, and back at once: music that came late.
        q.samples.clear();
        q.ran_dry(start);
        q.samples.extend(vec![0i16; START_QUEUED]);
        q.try_start(start + Duration::from_millis(40));
        assert!(q.playing);
        assert_eq!(q.cushion, START_QUEUED + CUSHION_STEP);

        // Now it waits for more before starting.
        q.samples.clear();
        q.ran_dry(start);
        q.samples.extend(vec![0i16; START_QUEUED]);
        q.try_start(start + Duration::from_millis(10));
        assert!(!q.playing);

        // A pause between words: back after a while, the cushion stays as it was.
        let before = q.cushion;
        q.samples.extend(vec![0i16; before]);
        q.try_start(start + Duration::from_secs(1));
        assert!(q.playing);
        assert_eq!(q.cushion, before);

        // Never past the limit.
        for _ in 0..50 {
            q.samples.clear();
            q.ran_dry(start);
            q.samples.extend(vec![0i16; MAX_CUSHION]);
            q.try_start(start);
        }
        assert_eq!(q.cushion, MAX_CUSHION);
        assert!(MAX_CUSHION < MAX_QUEUED);
    }
    #[test]
    fn manual_gate_opens_above_threshold_and_holds() {
        let p = Processing { auto_threshold: false, threshold: -40, ..Processing::DEFAULT };
        let mut gate = Gate::default();
        assert!(!gate.update(&p, -60.0, None));
        assert!(gate.update(&p, -30.0, None));
        // Stays open a moment after speech, then closes.
        for _ in 0..GATE_HOLD - 1 {
            assert!(gate.update(&p, -60.0, None));
        }
        assert!(!gate.update(&p, -60.0, None));
    }

    #[test]
    fn auto_gate_follows_speech_or_the_noise_floor() {
        let p = Processing::DEFAULT;
        let mut gate = Gate::default();
        assert!(!gate.update(&p, -30.0, Some(0.1)), "loud noise without speech stays out");
        assert!(gate.update(&p, -30.0, Some(0.9)));
        let mut gate = Gate::default();
        for _ in 0..100 {
            assert!(!gate.update(&p, -55.0, None), "a steady fan is the floor");
        }
        assert!(gate.update(&p, -35.0, None));
    }

    /// A fake keyboard on a quiet room: short bursts of noise, a few times a second.
    fn keyboard(frames: usize) -> Vec<i16> {
        let mut seed = 12345u32;
        let mut out = vec![0i16; frames * FRAME];
        for f in 0..frames {
            // A key every 30 frames (300 ms): a sharp click that dies away in about 5 ms.
            if f % 30 == 0 {
                for (i, s) in out[f * FRAME..(f + 1) * FRAME].iter_mut().enumerate() {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = ((seed >> 16) as i16) as f32 / 32768.0;
                    *s = (noise * (-(i as f32) / 60.0).exp() * 14000.0) as i16;
                }
            }
        }
        out
    }

    #[test]
    fn keyboard_clicks_are_suppressed() {
        let input = keyboard(300);
        let mut cleaner = Cleaner::new();
        let mut output = input.clone();
        for frame in output.chunks_mut(FRAME) {
            cleaner.process(frame, 1.0);
        }
        // After RNNoise has settled (1 s), the clicks must come out at least 10 dB quieter (measured: about 11).
        let peak = |s: &[i16]| s.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0) as f32;
        let (before, after) = (peak(&input[100 * FRAME..]), peak(&output[100 * FRAME..]));
        assert!(after < before / 3.0, "clicks went from {before} to {after}");
    }

    #[test]
    fn weaker_strength_lets_more_through() {
        let input = keyboard(300);
        let run = |strength: f32| {
            let mut cleaner = Cleaner::new();
            let mut out = input.clone();
            for frame in out.chunks_mut(FRAME) {
                cleaner.process(frame, strength);
            }
            out[100 * FRAME..].iter().map(|v| v.unsigned_abs() as u64).sum::<u64>()
        };
        assert!(run(0.3) > run(1.0), "a gentler setting should leave more of the clicks");
    }

    #[test]
    fn speech_gain_opens_fast_and_fades_out() {
        let mut g = SpeechGain::default();
        assert_eq!(g.next(0.05), (0.0, 0.0), "noise alone stays silent");
        let (_, after) = g.next(0.95);
        assert!(after > 0.3 && after < 0.4, "speech starts opening at once, not in a jump: {after}");
        for _ in 0..3 {
            g.next(0.95);
        }
        assert!(g.next(0.95).1 > 0.95);
        // About 150 ms (15 frames) after the last word, nothing is left.
        let mut last = 1.0;
        for _ in 0..15 {
            last = g.next(0.02).1;
        }
        assert_eq!(last, 0.0);
    }

    #[test]
    fn settings_round_trip_and_clamp() {
        let p = Processing { noise: 1, mic_volume: 150, threshold: -20, auto_threshold: false, ..Processing::DEFAULT };
        assert_eq!(Processing::from_json(&p.to_json()), p);
        let wild = Processing::from_json(&serde_json::json!({ "noise": 9, "mic_volume": 999, "threshold": 50 }));
        assert_eq!((wild.noise, wild.mic_volume, wild.threshold), (2, 200, 0));
        assert_eq!(dbfs(&[0; 480]), -100.0);
        assert!((dbfs(&[i16::MAX; 480])).abs() < 0.1);
    }

    #[test]
    fn push_to_talk_key_is_mapped_to_a_windows_key() {
        // What the interface reports for the keys.
        assert_eq!(vk_for_key(" "), Some(VK_SPACE));
        assert_eq!(vk_for_key("\n"), Some(VK_RETURN));
        assert_eq!(vk_for_key("\u{1b}"), Some(VK_ESCAPE));
        assert_eq!(vk_for_key("\u{7f}"), Some(VK_DELETE));
        assert_eq!(vk_for_key("\u{F700}"), Some(VK_UP));
        assert_eq!(vk_for_key("\u{F704}"), Some(VK_F1));
        assert_eq!(vk_for_key("\u{F70F}"), Some(VK_F1 + 11));
        assert_eq!(vk_for_key("a"), Some(0x41));
        assert_eq!(vk_for_key("Q"), Some(0x51));
        assert_eq!(vk_for_key("7"), Some(0x37));
        assert_eq!(vk_for_key("ю"), None, "a letter of another layout isn't a key");
        // And how it's shown in the settings.
        assert_eq!(key_name(VK_SPACE, MOD_CONTROL), "Ctrl + Пробел");
        assert_eq!(key_name(0x41, MOD_CONTROL | MOD_SHIFT), "Ctrl + Shift + A");
        assert_eq!(key_name(VK_F1, 0), "F1");
    }

    #[test]
    fn push_to_talk_settings_survive_a_restart() {
        let p = Processing { voice_mode: 1, ptt_key: VK_F1, ptt_mod: MOD_SHIFT, ..Processing::DEFAULT };
        assert_eq!(Processing::from_json(&p.to_json()), p);
        let wild = Processing::from_json(&serde_json::json!({ "voice_mode": 5, "ptt_key": 0, "ptt_mod": 99 }));
        assert_eq!((wild.voice_mode, wild.ptt_key, wild.ptt_mod), (1, VK_SPACE, 7));
    }

    #[test]
    fn cues_are_short_soft_and_click_free() {
        for cue in [Cue::Join, Cue::Leave, Cue::Mute, Cue::Unmute, Cue::PeerJoin, Cue::PeerLeave] {
            let s = cue_samples(cue);
            assert!(s.len() > SAMPLE_RATE as usize / 20 && s.len() < SAMPLE_RATE as usize / 2, "{cue:?} is {} samples", s.len());
            let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(peak > 2000 && peak < 7000, "{cue:?} peak {peak}");
            // Rises and dies away: no click at either end.
            assert!(s[0].unsigned_abs() < 300 && s[s.len() - 1].unsigned_abs() < 300, "{cue:?} clicks");
        }
    }

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut w = b"RIFF".to_vec();
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        w.extend_from_slice(&(2 * channels).to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }

    #[test]
    fn loads_a_stereo_44k_wav_as_48k_mono() {
        // Half a second of stereo at 44.1 kHz: left loud, right silent.
        let samples: Vec<i16> = (0..22050).flat_map(|_| [16000i16, 0]).collect();
        let pcm = load_wav(&wav(44100, 2, &samples)).expect("loads");
        assert!((pcm.len() as i64 - 24000).abs() <= 1, "{} samples", pcm.len());
        assert!(pcm.iter().all(|&s| (s - 8000).abs() <= 2), "mono is the average of both sides");
        assert!(load_wav(b"not a wav").is_none());
    }

    #[test]
    fn long_files_are_cut_to_three_seconds() {
        let pcm = load_wav(&wav(48000, 1, &vec![100i16; 48000 * 5])).unwrap();
        assert_eq!(pcm.len(), MAX_CUE);
    }
}