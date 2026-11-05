//! Nimbus voice: a LiveKit room on the user's own server, named after the
//! Discord voice channel. The microphone, the speakers and the mixing of
//! other people's audio are ours (see audio.rs), not WebRTC's. In a guild
//! the account also joins the channel in Discord's own voice
//! (discord_voice.rs), so people without Nimbus are heard and hear us, and
//! their Go Live streams can be opened (golive.rs).
//! The same room carries screen shares: ours (see share.rs) and other people's,
//! one of which can be watched at a time.
//! People using the web Nimbus join the same rooms, so both see and hear each other.
//! Our camera (camera.rs) goes to Discord's call (discord_video.rs); other people's
//! cameras there show on their tiles.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::video_frame::{BoxVideoFrame, VideoBuffer};
use livekit::webrtc::video_source::RtcVideoSource;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use serde_json::{json, Value};
use slint::{Image, ModelRc, VecModel, Weak};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::{audio, on_ui, share, AppWindow, DeviceItem, Pixels, VoiceMember};

/// Mic peak (of 32768) above which we count as speaking, after noise suppression.
const ME_SPEAKING_LEVEL: u64 = 1500;
/// Frames of a watched stream shown per second at most.
const VIEW_FRAME_GAP: Duration = Duration::from_millis(15);

pub enum VoiceCmd {
    Join {
        /// Empty outside a guild; then Discord's own voice isn't joined, unless `dm`.
        guild_id: String,
        /// The call of a DM or group (`channel_id`): Discord's own voice with no guild, no Nimbus room.
        dm: bool,
        channel_id: String,
        channel_name: String,
        guild_name: String,
        server: String,
        identity: String,
        name: String,
    },
    Leave,
    Switches {
        muted: bool,
        deafened: bool,
    },
    /// Re-read the device lists (devices come and go: headsets, USB mics).
    RefreshDevices,
    /// Index into the microphone list last sent to the UI.
    SetMic(usize),
    /// Index into the speaker list last sent to the UI.
    SetSpeaker(usize),
    /// Re-read the monitors and windows the share menu offers.
    ListShareSources,
    /// Show the menu row at this index (of the list last sent to the UI).
    StartShare { index: usize, quality: share::Quality, to: crate::golive_send::ShareTo },
    StopShare,
    /// Watch someone's screen share (by identity), or stop watching.
    Watch(Option<String>),
    /// Stop watching one of the streams; the others stay.
    StopOne(String),
    /// The sound of our own share, switched on or off while it runs.
    ShareSound(bool),
    /// Another client of ours took the voice connection: this call ends here, without telling Discord.
    TakenElsewhere,
    /// The verification code with this person of our call: shown in the window.
    VerifyCode(String),
    /// Our own VOICE_STATE_UPDATE from the Discord gateway.
    DiscordState(Value),
    /// VOICE_SERVER_UPDATE from the Discord gateway.
    DiscordServer(Value),
    /// Go Live events from the Discord gateway, by event name.
    DiscordStream(String, Value),
    /// Someone joined or left our channel in Discord's own voice.
    DiscordPeer { user_id: String, joined: bool },
    /// A soundboard sound played in our channel (48 kHz mono, already at its volume).
    PlaySound(std::sync::Arc<Vec<i16>>),
    /// The voice settings changed (noise suppression, sensitivity, volumes).
    SetProcessing(audio::Processing),
    /// Mic check: hear yourself; outside a call the mic opens just for it.
    MicTest(bool),
    /// Our camera in the call, on or off.
    Camera(bool),
    /// Index into the camera list last sent to the UI.
    SetCamera(usize),
    /// The settings' look at our own camera, on or off.
    CameraPreview(bool),
}

/// What the device menus show, and what was picked there (None = Windows' default).
#[derive(Default)]
struct Devices {
    mics: Vec<audio::Device>,
    speakers: Vec<audio::Device>,
    default_mic: Option<String>,
    default_speaker: Option<String>,
    mic: Option<String>,
    speaker: Option<String>,
    cameras: Vec<crate::camera::Device>,
    camera: Option<String>,
}

/// Where the picked devices are remembered between launches.
fn devices_file() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Nimbus Native").join("devices.json"))
}

/// The speakers picked in the voice settings, for sounds played outside a call (voice messages).
pub fn saved_speaker() -> Option<String> {
    let saved: Value = serde_json::from_slice(&std::fs::read(devices_file()?).ok()?).ok()?;
    saved["speaker"].as_str().map(String::from)
}

/// The mic picked in the voice settings, for recording voice messages outside a call.
pub fn saved_mic() -> Option<String> {
    let saved: Value = serde_json::from_slice(&std::fs::read(devices_file()?).ok()?).ok()?;
    saved["mic"].as_str().map(String::from)
}

impl Devices {
    fn open() -> Self {
        let saved: Value = devices_file().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut devices = Self {
            mic: saved["mic"].as_str().map(String::from),
            speaker: saved["speaker"].as_str().map(String::from),
            camera: saved["camera"].as_str().map(String::from),
            ..Self::default()
        };
        if saved["processing"].is_object() {
            audio::set_processing(audio::Processing::from_json(&saved["processing"]));
        }
        devices.refresh();
        devices
    }

    fn refresh(&mut self) {
        (self.mics, self.default_mic) = audio::devices(true);
        (self.speakers, self.default_speaker) = audio::devices(false);
        // Names only: no camera is switched on to list it.
        self.cameras = crate::camera::devices();
    }

    fn save(&self) {
        let Some(file) = devices_file() else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let processing = audio::processing().to_json();
        let _ = std::fs::write(file, json!({ "mic": self.mic, "speaker": self.speaker, "camera": self.camera, "processing": processing }).to_string());
    }

    /// The picked mic if it's plugged in; otherwise Windows' default.
    fn mic_now(&self) -> Option<String> {
        self.mic.clone().filter(|id| self.mics.iter().any(|d| &d.id == id))
    }

    fn speaker_now(&self) -> Option<String> {
        self.speaker.clone().filter(|id| self.speakers.iter().any(|d| &d.id == id))
    }

    /// The picked camera if it's plugged in; otherwise the first one.
    fn camera_now(&self) -> Option<String> {
        let picked = self.camera.clone().filter(|id| self.cameras.iter().any(|d| &d.id == id));
        picked.or_else(|| self.cameras.first().map(|d| d.id.clone()))
    }

    fn pick_camera(&mut self, index: usize) -> Result<String, String> {
        let id = self.cameras.get(index).ok_or_else(|| crate::i18n::t("устройство пропало"))?.id.clone();
        self.camera = Some(id.clone());
        self.save();
        Ok(id)
    }

    fn pick_mic(&mut self, index: usize) -> Result<String, String> {
        let id = self.mics.get(index).ok_or_else(|| crate::i18n::t("устройство пропало"))?.id.clone();
        self.mic = Some(id.clone());
        self.save();
        Ok(id)
    }

    fn pick_speaker(&mut self, index: usize) -> Result<String, String> {
        let id = self.speakers.get(index).ok_or_else(|| crate::i18n::t("устройство пропало"))?.id.clone();
        self.speaker = Some(id.clone());
        self.save();
        Ok(id)
    }

    fn push(&self, ui: &Weak<AppWindow>) {
        let items = |list: &[audio::Device], picked: Option<String>, default: &Option<String>| -> Vec<DeviceItem> {
            let current = picked.as_ref().or(default.as_ref());
            list.iter().map(|d| DeviceItem { name: d.name.clone().into(), selected: current == Some(&d.id) }).collect()
        };
        let mics = items(&self.mics, self.mic_now(), &self.default_mic);
        let speakers = items(&self.speakers, self.speaker_now(), &self.default_speaker);
        let camera = self.camera_now();
        let cameras: Vec<DeviceItem> = self.cameras.iter().map(|d| DeviceItem { name: d.name.clone().into(), selected: camera.as_ref() == Some(&d.id) }).collect();
        on_ui(ui, move |ui| {
            ui.set_mic_devices(ModelRc::new(VecModel::from(mics)));
            ui.set_speaker_devices(ModelRc::new(VecModel::from(speakers)));
            ui.set_camera_devices(ModelRc::new(VecModel::from(cameras)));
        });
    }
}

/// The mic and sound switches from the user panel; they outlast any one call.
#[derive(Clone, Copy, Default)]
struct Switches {
    muted: bool,
    deafened: bool,
}

/// State the room's event task reads too.
#[derive(Default)]
struct Shared {
    switches: Mutex<Switches>,
    /// Identity of the person whose screen we're watching.
    watched: Mutex<Option<String>>,
    /// Up while we look at our own share: the capture then sends small pictures of it.
    preview_on: Arc<AtomicBool>,
    /// Up while the settings show our own camera.
    camera_preview: Arc<AtomicBool>,
    sharing: AtomicBool,
    /// Our own speaking state, from the local mic level (instant).
    me_speaking: AtomicBool,
    /// When someone's join/leave sound last played: people in both the Nimbus
    /// room and Discord's voice are announced once, not twice.
    cued: Mutex<HashMap<String, Instant>>,
}

/// Plays someone's join or leave sound, unless it just played for them.
fn peer_cue(shared: &Shared, audio: &Mutex<audio::Engine>, who: &str, joined: bool) {
    let mut cued = shared.cued.lock().unwrap();
    cued.retain(|_, at| at.elapsed() < Duration::from_secs(5));
    if cued.insert(who.to_string(), Instant::now()).is_some() {
        return;
    }
    audio.lock().unwrap().cue(if joined { audio::Cue::PeerJoin } else { audio::Cue::PeerLeave });
}

struct Sharing {
    stop: Arc<AtomicBool>,
    /// The room's track, when the share goes to the Nimbus room.
    sid: Option<TrackSid>,
    /// The stream on Discord's own servers, when the share goes there.
    discord: Option<crate::golive_send::Stream>,
    /// Where the sound is copied for Discord, kept for switching the sound on again.
    sound_tap: Option<crate::golive_send::SoundTap>,
    /// Whose sound goes with the share, to switch the sound on again while sharing.
    scope: audio::Scope,
    /// The sound track and its capture, when the share has sound.
    sound: Option<(Option<TrackSid>, audio::SystemSound)>,
}

struct Session {
    /// The Nimbus (LiveKit) room; none when the server isn't there and the call is Discord's only.
    room: Option<Arc<Room>>,
    mic: Option<LocalAudioTrack>,
    /// Who we are in the call, with or without the room.
    identity: String,
    name: String,
    /// Our mic and speakers; the event task adds and removes people's audio.
    audio: Arc<Mutex<audio::Engine>>,
    /// The same channel in Discord's own voice, for people without Nimbus.
    discord: Option<crate::discord_voice::Call>,
    /// The Discord Go Live streams being watched, by whose they are.
    golive: HashMap<String, crate::golive::Watch>,
    events: Option<JoinHandle<()>>,
    /// Reads the link's round trip and losses every few seconds.
    link: Option<JoinHandle<()>>,
    /// Frame readers for other people's screen shares, by track sid.
    viewers: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    share: Option<Sharing>,
    channel_id: String,
}

/// A participant as the UI shows it; plain data so it can cross threads.
struct Member {
    identity: String,
    name: String,
    muted: bool,
    deafened: bool,
    speaking: bool,
    streaming: bool,
}

fn set_status(ui: &Weak<AppWindow>, channel: Option<(String, String)>, status: &str, connected: bool) {
    let status = status.to_string();
    on_ui(ui, move |ui| {
        match channel {
            Some((id, name)) => {
                ui.set_voice_channel(id.into());
                ui.set_voice_channel_name(name.into());
            }
            None => {
                ui.set_voice_channel("".into());
                ui.set_voice_members(ModelRc::new(VecModel::<VoiceMember>::default()));
                ui.set_me_speaking(false);
                crate::set_sharing(&ui, false);
            }
        }
        ui.set_voice_status(status.into());
        ui.set_voice_connected(connected);
    });
}

/// The round trip to the Nimbus (LiveKit) server in ms and the share of packets lost so far
/// in tenths of a percent, from WebRTC's own statistics; 0 when unknown.
pub static LINK_RTT_MS: AtomicU32 = AtomicU32::new(0);
pub static LINK_LOSS_PERMILLE: AtomicU32 = AtomicU32::new(0);

/// Lost packets as tenths of a percent of all that should have come.
fn loss_permille(lost: i64, received: u64) -> u32 {
    let total = lost.max(0) as u64 + received;
    if total == 0 { 0 } else { (lost.max(0) as u64 * 1000 / total) as u32 }
}

/// Watches the link to the server while we're in the room.
fn watch_link(room: Arc<Room>) -> JoinHandle<()> {
    use livekit::webrtc::stats::RtcStats;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let Ok(stats) = room.get_stats().await else { continue };
            let (mut rtt, mut lost, mut received) = (0.0f64, 0i64, 0u64);
            for s in stats.publisher_stats.iter().chain(&stats.subscriber_stats) {
                match s {
                    RtcStats::CandidatePair(p) if p.candidate_pair.nominated => rtt = rtt.max(p.candidate_pair.current_round_trip_time),
                    RtcStats::InboundRtp(i) => {
                        lost += i.received.packets_lost;
                        received += i.received.packets_received;
                    }
                    _ => {}
                }
            }
            LINK_RTT_MS.store((rtt * 1000.0) as u32, Ordering::Relaxed);
            LINK_LOSS_PERMILLE.store(loss_permille(lost, received), Ordering::Relaxed);
        }
    })
}

/// Shows a voice problem in the voice panel, where it's hard to miss.
fn report(ui: &Weak<AppWindow>, text: String) {
    on_ui(ui, move |ui| ui.set_voice_error(text.into()));
}

/// The saved voice settings into the settings window.
fn push_processing(ui: &Weak<AppWindow>) {
    let p = audio::processing();
    let key = audio::key_name(p.ptt_key, p.ptt_mod);
    let (mute_key, deafen_key) = (audio::hotkey_name(p.mute_key, p.mute_mod), audio::hotkey_name(p.deafen_key, p.deafen_mod));
    on_ui(ui, move |ui| {
        ui.set_mute_key_name(mute_key.into());
        ui.set_deafen_key_name(deafen_key.into());
        ui.set_voice_settings(crate::VoiceSettings {
            noise: p.noise as i32,
            noise_strength: p.noise_strength as i32,
            echo: p.echo,
            auto_gain: p.auto_gain,
            mic_volume: p.mic_volume as f32,
            speaker_volume: p.speaker_volume as f32,
            auto_threshold: p.auto_threshold,
            threshold: p.threshold as f32,
            voice_mode: p.voice_mode as i32,
        });
        ui.set_ptt_key_name(key.into());
    });
}

/// How many Discord streams are received at once: each one is decoded on this computer.
const MAX_STREAMS: usize = 4;

/// Starts receiving a Discord stream; its pictures go to its tile, and to the stage while it is up there.
fn watch_stream(ui: &Weak<AppWindow>, gateway: &UnboundedSender<Value>, s: &Session, id: &str) -> Option<crate::golive::Watch> {
    let (guild, channel, session_id) = s.discord.as_ref()?.place()?;
    let status: crate::golive::Status = Arc::new(|text: String| crate::log_line(&format!("go live status: {text}")));
    let (frame_ui, who) = (ui.clone(), id.to_string());
    let last_shown: Mutex<Option<Instant>> = Mutex::new(None);
    let meter = Mutex::new(QualityMeter::new(ui, id));
    let latest = Arc::new(Latest::default());
    let display: crate::golive::Display = Arc::new(move |pixels: Pixels| {
        meter.lock().unwrap().frame(pixels.height());
        let mut last = last_shown.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < VIEW_FRAME_GAP) {
            return;
        }
        *last = Some(Instant::now());
        show_latest(&frame_ui, &latest, &who, fit_to_shown(pixels, latest.shown_width.load(Ordering::Relaxed)));
    });
    let sound = s.audio.lock().unwrap().sink();
    Some(crate::golive::Watch::start(gateway.clone(), guild, channel, id, &s.identity, session_id, status, display, sound))
}

/// Measures a watched stream's picture height and frame rate for the
/// quality badge ("1440P 60 КАДРОВ В СЕКУНДУ"), updated once a second.
struct QualityMeter {
    ui: Weak<AppWindow>,
    who: String,
    frames: u32,
    since: Instant,
    shown: String,
}

impl QualityMeter {
    fn new(ui: &Weak<AppWindow>, who: &str) -> Self {
        Self { ui: ui.clone(), who: who.to_string(), frames: 0, since: Instant::now(), shown: String::new() }
    }

    /// Every received picture, before any are skipped for the screen.
    fn frame(&mut self, height: u32) {
        self.frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let fps = (self.frames as f64 / elapsed.as_secs_f64()).round() as u32;
        let text = crate::i18n::tf("{}P {} КАДРОВ В СЕКУНДУ", &[&height, &fps]);
        (self.frames, self.since) = (0, Instant::now());
        if text != self.shown {
            self.shown = text.clone();
            let who = self.who.clone();
            on_ui(&self.ui, move |ui| crate::show_stream_quality(&ui, &who, text));
        }
    }
}

pub async fn run(ui: Weak<AppWindow>, http: reqwest::Client, mut cmds: UnboundedReceiver<VoiceCmd>, gateway: UnboundedSender<Value>) {
    let shared = Arc::new(Shared::default());
    let mut devices = Devices::open();
    if devices.mics.is_empty() && devices.speakers.is_empty() {
        report(&ui, crate::i18n::t("Нет доступа к микрофону и динамикам"));
    }
    devices.push(&ui);
    push_processing(&ui);
    let mut session: Option<Session> = None;
    // The mic and speakers opened for a mic check outside a call.
    let mut test_engine: Option<audio::Engine> = None;
    // What the share menu shows, in the same order.
    let mut share_sources: Vec<share::Source> = Vec::new();
    // Our camera, while the call or the settings' preview wants it.
    let mut rig: Option<Rig> = None;

    while let Some(cmd) = cmds.recv().await {
        match cmd {
            VoiceCmd::Join { guild_id, dm, channel_id, channel_name, guild_name, server, identity, name } => {
                if session.as_ref().is_some_and(|s| s.channel_id == channel_id) {
                    continue;
                }
                if let Some(mut old) = session.take() {
                    camera_off(&ui, Some(&mut old), &mut rig, &shared);
                    leave(old, &shared, &ui).await;
                }
                // The call's own engine takes over the devices from a mic check.
                test_engine = None;
                let label = if guild_name.is_empty() { channel_name } else { format!("{channel_name} / {guild_name}") };
                report(&ui, String::new());
                set_status(&ui, Some((channel_id.clone(), label)), &crate::i18n::t("Подключение…"), false);
                let place = Place { gateway: &gateway, guild_id: &guild_id, dm, channel_id: &channel_id, server: &server };
                match join(&ui, &http, &devices, &shared, place, &identity, &name).await {
                    Ok(s) => {
                        session = Some(s);
                        on_ui(&ui, |ui| {
                            ui.set_voice_status(crate::i18n::t("Голос подключён").into());
                            ui.set_voice_connected(true);
                        });
                    }
                    Err(e) => {
                        set_status(&ui, None, "", false);
                        report(&ui, e);
                    }
                }
            }
            VoiceCmd::VerifyCode(user) => {
                let code = session.as_ref().and_then(|s| s.discord.as_ref()).zip(user.parse::<u64>().ok()).map(|(call, id)| call.verification_code(id));
                let shown = ui.clone();
                tokio::spawn(async move {
                    let code = match code {
                        Some(answer) => answer.await.ok().flatten(),
                        None => None,
                    };
                    on_ui(&shown, move |ui| match code {
                        Some(code) => {
                            ui.set_info_title(crate::i18n::t("Код подтверждения").into());
                            ui.set_info_text(code.into());
                            ui.set_info_timeouts(false);
                            ui.set_info_open(true);
                        }
                        None => ui.set_notice(crate::i18n::t("Код подтверждения пока недоступен").into()),
                    });
                });
            }
            VoiceCmd::TakenElsewhere => {
                if let Some(mut old) = session.take() {
                    if let Some(call) = old.discord.as_mut() {
                        call.leave_silently();
                    }
                    camera_off(&ui, Some(&mut old), &mut rig, &shared);
                    leave(old, &shared, &ui).await;
                }
                set_status(&ui, None, "", false);
                report(&ui, String::new());
            }
            VoiceCmd::Leave => {
                if let Some(mut old) = session.take() {
                    camera_off(&ui, Some(&mut old), &mut rig, &shared);
                    leave(old, &shared, &ui).await;
                }
                set_status(&ui, None, "", false);
                report(&ui, String::new());
            }
            VoiceCmd::RefreshDevices => {
                devices.refresh();
                devices.push(&ui);
            }
            VoiceCmd::SetMic(index) => {
                match devices.pick_mic(index) {
                    Ok(id) => {
                        if let Some(s) = &session {
                            s.audio.lock().unwrap().set_mic(Some(id));
                        }
                    }
                    Err(e) => report(&ui, crate::i18n::tf("Не удалось сменить микрофон: {}", &[&e])),
                }
                devices.push(&ui);
            }
            VoiceCmd::SetSpeaker(index) => {
                match devices.pick_speaker(index) {
                    Ok(id) => {
                        if let Some(s) = &session {
                            s.audio.lock().unwrap().set_speaker(Some(id));
                        }
                    }
                    Err(e) => report(&ui, crate::i18n::tf("Не удалось сменить устройство вывода: {}", &[&e])),
                }
                devices.push(&ui);
            }
            VoiceCmd::DiscordState(data) => {
                if let Some(call) = session.as_mut().and_then(|s| s.discord.as_mut()) {
                    call.on_state(&data);
                }
            }
            VoiceCmd::DiscordServer(data) => {
                if let Some(call) = session.as_mut().and_then(|s| s.discord.as_mut()) {
                    call.on_server(&data);
                }
            }
            VoiceCmd::Switches { muted, deafened } => {
                let before = std::mem::replace(&mut *shared.switches.lock().unwrap(), Switches { muted, deafened });
                if let Some(s) = &mut session {
                    let (was_off, now_off) = (before.muted || before.deafened, muted || deafened);
                    if was_off != now_off {
                        s.audio.lock().unwrap().cue(if now_off { audio::Cue::Mute } else { audio::Cue::Unmute });
                    }
                    apply_switches(s, Switches { muted, deafened });
                    push_members(&ui, s.room.as_deref(), &s.identity, &s.name, &HashSet::new(), &shared);
                }
            }
            VoiceCmd::ListShareSources => {
                let list = tokio::task::spawn_blocking(share::sources).await.unwrap_or_default();
                share_sources = list.iter().map(|s| s.source).collect();
                // (name, detail, is a window); the list's own rows are made on the UI thread, they hold pictures.
                let rows: Vec<(String, String, bool)> = list.into_iter().map(|s| (s.name, s.detail, s.is_window)).collect();
                let monitors = rows.iter().filter(|r| !r.2).count() as i32;
                let windows = rows.len() as i32 - monitors;
                on_ui(&ui, move |ui| {
                    let items: Vec<crate::ShareSourceItem> = rows
                        .into_iter()
                        .map(|(name, detail, is_window)| crate::ShareSourceItem { name: name.into(), detail: detail.into(), is_window, thumb: Image::default(), has_thumb: false })
                        .collect();
                    ui.set_share_monitors(monitors);
                    ui.set_share_windows(windows);
                    ui.set_share_sources(ModelRc::new(VecModel::from(items)));
                });
                // The pictures follow one by one, so the list shows at once.
                let sources = share_sources.clone();
                let thumb_ui = ui.clone();
                tokio::task::spawn_blocking(move || {
                    for (i, source) in sources.into_iter().enumerate() {
                        let Some(px) = crate::thumbs::thumbnail(source) else { continue };
                        on_ui(&thumb_ui, move |ui| {
                            crate::update_row(ui.get_share_sources(), i, |s| {
                                s.thumb = Image::from_rgba8(px);
                                s.has_thumb = true;
                            })
                        });
                    }
                });
            }
            VoiceCmd::StartShare { .. } | VoiceCmd::StopShare => {
                let Some(s) = session.as_mut() else {
                    report(&ui, crate::i18n::t("Чтобы показать экран, сначала зайди в голосовой канал"));
                    continue;
                };
                // Showing a screen in a DM call is not done yet; the button is hidden there.
                if s.discord.as_ref().is_some_and(|c| c.is_dm()) {
                    report(&ui, crate::i18n::t("В звонке в личных сообщениях показ экрана пока недоступен"));
                    continue;
                }
                // One share at a time: picking something new replaces what's shown.
                if let Some(sharing) = s.share.take() {
                    stop_share(s.room.as_deref(), sharing).await;
                    shared.sharing.store(false, Ordering::Relaxed);
                    // Looking at our own share: there is nothing to look at now.
                    if shared.preview_on.swap(false, Ordering::Relaxed) {
                        let me = s.identity.clone();
                        on_ui(&ui, move |ui| crate::stream_gone(&ui, &me));
                    }
                }
                if let VoiceCmd::StartShare { index, quality, to } = cmd {
                  'start: {
                        let Some(&source) = share_sources.get(index) else { break 'start };
                    // Discord needs the call we are in; without it the share goes only where it can.
                    let discord = if to.discord() {
                        let me = s.identity.clone();
                        match s.discord.as_ref().and_then(|c| c.place()) {
                            Some((guild, channel, session_id)) => Some(DiscordShare {
                                gateway: gateway.clone(),
                                place: crate::golive_send::Place::Guild { guild: guild.to_string(), channel: channel.to_string() },
                                me,
                                session_id: session_id.to_string(),
                            }),
                            None => {
                                report(&ui, crate::i18n::t("Чтобы показать на Discord, зайди в голосовой канал Discord"));
                                if !to.room() {
                                    break 'start;
                                }
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let to = if discord.is_none() { crate::golive_send::ShareTo::Room } else { to };
                    if to.room() && s.room.is_none() {
                        report(&ui, crate::i18n::t("Сервер Nimbus не подключён: показ экрана доступен только на Discord"));
                        break 'start;
                    }
                    let status_ui = ui.clone();
                    let status: crate::golive_send::Status = Arc::new(move |text: String| {
                        crate::log_line(&format!("go live send: {text}"));
                        if text.starts_with(&crate::i18n::tf("ошибка: {}", &[])) || text == crate::i18n::t("стрим закрыт") {
                            let said = crate::i18n::tf("Показ на Discord: {}", &[&text]);
                            on_ui(&status_ui, move |ui| ui.set_notice(said.into()));
                        }
                    });
                    // A window that closes ends its share the same way the button does.
                    let end_ui = ui.clone();
                    let on_end: Box<dyn FnOnce() + Send> = Box::new(move || {
                        let _ = end_ui.upgrade_in_event_loop(|ui| ui.invoke_stop_share());
                    });
                    let preview_ui = ui.clone();
                    // Our own view of the share: the picture, and what really goes out ("1440P 60 КАДРОВ В СЕКУНДУ").
                    // When the window is slower than the capture, an older picture is replaced, never queued.
                    let me = s.identity.clone();
                    let newest: Arc<Mutex<Option<(Pixels, u32, u32)>>> = Arc::default();
                    let queued = Arc::new(AtomicBool::new(false));
                    let preview: share::Preview = Arc::new(move |pixels: Pixels, height: u32, fps: u32| {
                        *newest.lock().unwrap() = Some((pixels, height, fps));
                        if queued.swap(true, Ordering::AcqRel) {
                            return;
                        }
                        let (newest, queued, me) = (newest.clone(), queued.clone(), me.clone());
                        on_ui(&preview_ui, move |ui| {
                            queued.store(false, Ordering::Release);
                            let Some((pixels, height, fps)) = newest.lock().unwrap().take() else { return };
                            crate::show_stream_picture(&ui, &me, Image::from_rgba8(pixels));
                            crate::show_stream_quality(&ui, &me, crate::i18n::tf("{}P {} КАДРОВ В СЕКУНДУ", &[&height, &fps]));
                        });
                    });
                    match start_share(s.room.as_deref(), source, quality, to, discord, status, on_end, Some((shared.preview_on.clone(), preview))).await {
                        Ok(sharing) => {
                            s.share = Some(sharing);
                            shared.sharing.store(true, Ordering::Relaxed);
                            // Our own stream shows on its tile by itself, with no click on "Watch".
                            shared.preview_on.store(true, Ordering::Relaxed);
                            let me = s.identity.clone();
                            on_ui(&ui, move |ui| crate::set_stream_watched(&ui, &me, true));
                        }
                        Err(e) => report(&ui, e),
                    }
                  }
                }
                let sharing = s.share.is_some();
                on_ui(&ui, move |ui| crate::set_sharing(&ui, sharing));
                push_members(&ui, s.room.as_deref(), &s.identity, &s.name, &HashSet::new(), &shared);
            }
            VoiceCmd::ShareSound(on) => {
                let Some(s) = session.as_mut() else { continue };
                let Some(sharing) = s.share.as_mut() else { continue };
                if on && sharing.sound.is_none() && sharing.sid.is_some() {
                    match publish_sound(s.room.as_deref(), sharing.scope, true, sharing.sound_tap.clone()).await {
                        Ok(sound) => sharing.sound = Some(sound),
                        Err(e) => report(&ui, crate::i18n::tf("Не удалось включить звук демки: {}", &[&e])),
                    }
                } else if !on {
                    if let Some((sid, sound)) = sharing.sound.take() {
                        drop(sound);
                        if let (Some(sid), Some(room)) = (sid, &s.room) {
                            let _ = room.local_participant().unpublish_track(&sid).await;
                        }
                    }
                }
            }
            VoiceCmd::SetProcessing(p) => {
                audio::set_processing(p);
                devices.save();
            }
            VoiceCmd::MicTest(on) => {
                audio::set_loopback(on);
                if !on {
                    test_engine = None;
                } else if session.is_none() && test_engine.is_none() {
                    test_engine = Some(audio::Engine::start(devices.mic_now(), devices.speaker_now()));
                }
                on_ui(&ui, move |ui| ui.set_mic_testing(on));
            }
            VoiceCmd::Camera(on) => {
                if !on {
                    camera_off(&ui, session.as_mut(), &mut rig, &shared);
                    continue;
                }
                let started = match session.as_mut() {
                    Some(s) => camera_on(&ui, s, &devices, &mut rig, &shared),
                    None => Err(crate::i18n::t("Камера работает в голосе Discord: зайди в голосовой канал")),
                };
                if let Err(e) = started {
                    report(&ui, e);
                    camera_off(&ui, session.as_mut(), &mut rig, &shared);
                }
            }
            VoiceCmd::SetCamera(index) => {
                if let Err(e) = devices.pick_camera(index) {
                    report(&ui, crate::i18n::tf("Не удалось сменить камеру: {}", &[&e]));
                }
                devices.push(&ui);
                // A running camera moves to the new one.
                if rig.is_some() {
                    let sending = rig.as_ref().is_some_and(Rig::sending);
                    camera_off(&ui, session.as_mut(), &mut rig, &shared);
                    rig = None;
                    if shared.camera_preview.load(Ordering::Relaxed) {
                        rig = start_rig(&ui, devices.camera_now(), &shared).map_err(|e| report(&ui, e)).ok();
                    }
                    if let (true, Some(s)) = (sending, session.as_mut()) {
                        if let Err(e) = camera_on(&ui, s, &devices, &mut rig, &shared) {
                            report(&ui, e);
                            camera_off(&ui, Some(s), &mut rig, &shared);
                        }
                    }
                }
            }
            VoiceCmd::CameraPreview(on) => {
                shared.camera_preview.store(on, Ordering::Relaxed);
                if on {
                    on_ui(&ui, |ui| ui.set_camera_preview_error("".into()));
                }
                if on && rig.is_none() {
                    match start_rig(&ui, devices.camera_now(), &shared) {
                        Ok(started) => rig = Some(started),
                        Err(e) => {
                            shared.camera_preview.store(false, Ordering::Relaxed);
                            on_ui(&ui, move |ui| {
                                ui.set_camera_preview_on(false);
                                ui.set_camera_preview_error(e.into());
                            });
                        }
                    }
                } else if !on {
                    on_ui(&ui, |ui| ui.set_camera_preview_frame(Image::default()));
                    // Not on in a call either: the camera goes off.
                    if !rig.as_ref().is_some_and(Rig::sending) {
                        rig = None;
                    }
                }
            }
            VoiceCmd::DiscordPeer { user_id, joined } => {
                if let Some(s) = &session {
                    peer_cue(&shared, &s.audio, &user_id, joined);
                }
            }
            VoiceCmd::PlaySound(samples) => {
                if let Some(s) = &session {
                    s.audio.lock().unwrap().play_sound(&samples);
                }
            }
            VoiceCmd::DiscordStream(kind, data) => {
                let Some(s) = session.as_mut() else { continue };
                // Our own Go Live stream: its events come the same way.
                if let Some(sharing) = s.share.as_mut() {
                    if sharing.discord.as_mut().is_some_and(|stream| stream.on_event(&kind, &data)) {
                        sharing.discord = None;
                        if sharing.sid.is_none() {
                            on_ui(&ui, |ui| ui.invoke_stop_share());
                        }
                    }
                }
                // The streamer stopped: close the view instead of freezing on the last frame.
                let ended: Vec<String> = s.golive.iter_mut().filter_map(|(id, watch)| watch.on_event(&kind, &data).then(|| id.clone())).collect();
                for id in ended {
                    s.golive.remove(&id);
                    on_ui(&ui, move |ui| crate::stream_gone(&ui, &id));
                }
            }
            VoiceCmd::StopOne(id) => {
                let Some(s) = session.as_mut() else { continue };
                s.golive.remove(&id);
                if id == s.identity {
                    shared.preview_on.store(false, Ordering::Relaxed);
                }
                if shared.watched.lock().unwrap().as_deref() == Some(id.as_str()) {
                    s.audio.lock().unwrap().set_watched(None);
                    *shared.watched.lock().unwrap() = None;
                }
                on_ui(&ui, move |ui| crate::stream_gone(&ui, &id));
            }
            VoiceCmd::Watch(identity) => {
                crate::log_line(&format!(
                    "watch: {:?} (room: {}, discord voice: {})",
                    identity,
                    session.is_some(),
                    session.as_ref().is_some_and(|s| s.discord.as_ref().and_then(|c| c.place()).is_some())
                ));
                // Nothing asked for: every stream closes.
                let Some(id) = identity else {
                    if let Some(s) = &mut session {
                        s.golive.clear();
                        s.audio.lock().unwrap().set_watched(None);
                    }
                    shared.preview_on.store(false, Ordering::Relaxed);
                    *shared.watched.lock().unwrap() = None;
                    on_ui(&ui, |ui| crate::stop_all_streams(&ui));
                    continue;
                };
                let Some(s) = session.as_mut() else {
                    report(&ui, crate::i18n::t("Показ можно открыть, когда голос подключён: подожди секунду или зайди в канал заново"));
                    continue;
                };
                let in_room = id != s.identity && s.room.as_ref().is_some_and(|room| room.remote_participants().values().any(|p| p.identity().to_string() == id));
                if id == s.identity && s.share.is_some() {
                    // Our own share: not received from anywhere, so the capture shows a copy of it.
                    shared.preview_on.store(true, Ordering::Relaxed);
                } else if in_room {
                    s.audio.lock().unwrap().set_watched(Some(id.clone()));
                    *shared.watched.lock().unwrap() = Some(id.clone());
                } else if !s.golive.contains_key(&id) {
                    // Not in the Nimbus room: a Discord Go Live stream. One already coming in just goes up on stage.
                    if s.discord.as_ref().is_some_and(|c| c.is_dm()) {
                        report(&ui, crate::i18n::t("Стримы в звонках в личных сообщениях пока не открываются"));
                        continue;
                    }
                    let Some(watch) = watch_stream(&ui, &gateway, s, &id) else {
                        report(&ui, crate::i18n::t("Стрим Discord можно открыть, когда голос Discord подключён"));
                        continue;
                    };
                    if s.golive.len() >= MAX_STREAMS {
                        if let Some(old) = s.golive.keys().next().cloned() {
                            s.golive.remove(&old);
                            on_ui(&ui, move |ui| crate::stream_gone(&ui, &old));
                        }
                    }
                    s.golive.insert(id.clone(), watch);
                }
                on_ui(&ui, move |ui| {
                    crate::set_stream_watched(&ui, &id, true);
                    crate::focus_stream(&ui, &id);
                });
            }
        }
    }
}

/// Our camera while something wants it: the call (sent to Discord, shown on our tile)
/// or the settings' preview. Dropping it switches the camera off.
struct Rig {
    capture: crate::camera::Capture,
    /// The GPU encoder feeding Discord, while the camera is on in a call.
    encoder: Arc<Mutex<Option<crate::hwenc::Encoder>>>,
    /// Our identity while our tile shows the camera.
    tile: Arc<Mutex<Option<String>>>,
}

impl Rig {
    fn sending(&self) -> bool {
        self.encoder.lock().unwrap().is_some()
    }
}

/// How often our own small pictures are made (the tile and the settings' preview).
const CAMERA_PICTURE_EVERY: Duration = Duration::from_millis(66);
/// Their width at most.
const CAMERA_PICTURE_WIDTH: u32 = 640;

/// Switches the camera on: frames to the encoder (once there is one) and, a few times a
/// second, a small mirrored picture to our tile and the settings' preview.
fn start_rig(ui: &Weak<AppWindow>, device: Option<String>, shared: &Shared) -> Result<Rig, String> {
    let device = device.ok_or_else(|| crate::i18n::t("Камера не найдена"))?;
    let encoder: Arc<Mutex<Option<crate::hwenc::Encoder>>> = Arc::default();
    let tile: Arc<Mutex<Option<String>>> = Arc::default();
    let (frame_encoder, frame_tile, preview, ui) = (encoder.clone(), tile.clone(), shared.camera_preview.clone(), ui.clone());
    let mut last_picture: Option<Instant> = None;
    let on_frame: crate::camera::OnFrame = Box::new(move |frame: crate::camera::Frame| {
        if let Some(encoder) = frame_encoder.lock().unwrap().as_ref() {
            encoder.push(crate::hwenc::Input { nv12: frame.nv12.clone(), timestamp_us: frame.time_us });
        }
        let me = frame_tile.lock().unwrap().clone();
        let previewing = preview.load(Ordering::Relaxed);
        let due = last_picture.is_none_or(|t| t.elapsed() >= CAMERA_PICTURE_EVERY);
        if (me.is_none() && !previewing) || !due || !crate::WINDOW_SHOWN.load(Ordering::Relaxed) {
            return;
        }
        last_picture = Some(Instant::now());
        let pixels = camera_picture(&frame);
        if let Some(me) = me {
            crate::set_camera_frame(&ui, me, Some(pixels.clone()));
        }
        if previewing {
            on_ui(&ui, move |ui| ui.set_camera_preview_frame(Image::from_rgba8(pixels)));
        }
    });
    let capture = crate::camera::start(Some(device), crate::camera::WANTED, on_frame).map_err(|e| crate::i18n::tf("Не удалось включить камеру: {}", &[&e]))?;
    Ok(Rig { capture, encoder, tile })
}

/// Our camera's frame as a small picture, mirrored as people expect to see themselves.
fn camera_picture(frame: &crate::camera::Frame) -> Pixels {
    let (w, h) = crate::camera::preview_size(frame.width, frame.height, CAMERA_PICTURE_WIDTH);
    let rgba = crate::camera::nv12_to_rgba(&frame.nv12, frame.width, frame.height, w, h, true);
    let mut pixels = Pixels::new(w, h);
    pixels.make_mut_bytes().copy_from_slice(&rgba);
    pixels
}

/// Our camera on in the call: the capture (unless the settings' preview already runs it),
/// the GPU encoder, our stream declared on Discord's voice connection and our tile.
fn camera_on(ui: &Weak<AppWindow>, s: &mut Session, devices: &Devices, rig: &mut Option<Rig>, shared: &Shared) -> Result<(), String> {
    let Some(call) = s.discord.as_mut() else {
        return Err(crate::i18n::t("Камера работает в голосе Discord: зайди в голосовой канал"));
    };
    if rig.is_none() {
        *rig = Some(start_rig(ui, devices.camera_now(), shared)?);
    }
    let Some(running) = rig.as_ref() else { return Ok(()) };
    if running.sending() {
        return Ok(());
    }
    let mode = running.capture.mode;
    let (width, height) = (mode.width & !1, mode.height & !1);
    let source = livekit::webrtc::video_source::native::NativeVideoSource::new_encoded(livekit::webrtc::video_source::VideoResolution { width, height });
    let encoder = crate::hwenc::start(width, height, mode.fps, crate::discord_video::START_BITRATE, source, false).map_err(|e| {
        crate::log_line(&format!("camera: no hardware encoder ({e})"));
        crate::i18n::t("Для камеры нужен аппаратный кодировщик H.264 (NVIDIA, AMD или Intel)")
    })?;
    let handle = encoder.handle();
    handle.add_sink(call.video().frame_sink());
    let (keyframes, bitrate) = (handle.clone(), handle);
    let controls = crate::golive_send::Controls { keyframe: Arc::new(move || keyframes.request_keyframe()), bitrate: Arc::new(move |bps| bitrate.set_bitrate(bps)) };
    call.video().send(Some(crate::discord_video::Outgoing { width, height, fps: mode.fps, controls }));
    call.set_camera(true);
    *running.encoder.lock().unwrap() = Some(encoder);
    *running.tile.lock().unwrap() = Some(s.identity.clone());
    on_ui(ui, |ui| ui.set_camera_on(true));
    Ok(())
}

/// Our camera off in the call: "video off" to Discord, our tile back to the avatar, and
/// the camera itself off unless the settings' preview still shows it.
fn camera_off(ui: &Weak<AppWindow>, s: Option<&mut Session>, rig: &mut Option<Rig>, shared: &Shared) {
    let was_on = rig.as_ref().is_some_and(Rig::sending);
    if let Some(running) = rig.as_ref() {
        *running.encoder.lock().unwrap() = None;
        if let Some(me) = running.tile.lock().unwrap().take() {
            crate::set_camera_frame(ui, me, None);
        }
    }
    if let (true, Some(call)) = (was_on, s.and_then(|s| s.discord.as_mut())) {
        call.video().send(None);
        call.set_camera(false);
    }
    if !shared.camera_preview.load(Ordering::Relaxed) {
        *rig = None;
    }
    on_ui(ui, |ui| ui.set_camera_on(false));
}

/// Where a call goes: the Nimbus room on `server`, and the same channel in
/// Discord's own voice (through `gateway`) when it's in a guild. A DM or group
/// call is Discord's voice alone.
struct Place<'a> {
    gateway: &'a UnboundedSender<Value>,
    guild_id: &'a str,
    dm: bool,
    channel_id: &'a str,
    server: &'a str,
}

async fn join(
    ui: &Weak<AppWindow>,
    http: &reqwest::Client,
    devices: &Devices,
    shared: &Arc<Shared>,
    place: Place<'_>,
    identity: &str,
    name: &str,
) -> Result<Session, String> {
    let started = Instant::now();
    let channel_id = place.channel_id;
    // Discord's own voice is asked for first, so it connects while the Nimbus room is still being reached.
    let engine = audio::Engine::start(devices.mic_now(), devices.speaker_now());
    let current = *shared.switches.lock().unwrap();
    // People in the Nimbus room; their Discord audio would be heard twice (filled in once the room is reached).
    let nimbus = Arc::new(Mutex::new(HashSet::<String>::new()));
    let discord = (!place.guild_id.is_empty() || place.dm).then(|| {
        let (gateway, guild) = (place.gateway.clone(), (!place.dm).then_some(place.guild_id));
        // Other people's cameras go to their tiles.
        let camera_ui = ui.clone();
        let display: crate::discord_video::Display = Arc::new(move |user: String, pixels: Option<Pixels>| crate::set_camera_frame(&camera_ui, user, pixels));
        crate::discord_voice::Call::join(gateway, guild, channel_id, identity, &engine, nimbus.clone(), current.muted, current.deafened, display)
    });

    // The Nimbus room is optional: a Discord channel works without it (no server, no network to it).
    let connected = async {
        if place.dm {
            return Err("a DM call has no Nimbus room".to_string());
        }
        // Discord's answers for its own voice wait in the queue while this runs: a server that isn't
        // there must not hold the call up for the seconds Windows spends on a refused connection.
        if discord.is_some() && !server_answers(place.server).await {
            return Err(format!("nothing answers at {}", place.server));
        }
        let (url, token) = fetch_token(http, place.server, channel_id, identity, name).await?;
        let token_at = started.elapsed();
        let (room, events) = Room::connect(&url, &token, RoomOptions::default())
            .await
            .map_err(|e| crate::i18n::tf("не удалось подключиться к серверу голоса: {}", &[&e]))?;
        let room = Arc::new(room);
        let room_at = started.elapsed();

        let mic = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(engine.source()));
        let options = TrackPublishOptions { source: TrackSource::Microphone, dtx: true, ..Default::default() };
        room.local_participant()
            .publish_track(LocalTrack::Audio(mic.clone()), options)
            .await
            .map_err(|e| crate::i18n::tf("не удалось включить микрофон: {}", &[&e]))?;
        *nimbus.lock().unwrap() = room_identities(&room);
        crate::log_line(&format!(
            "join: token {} ms, room {} ms, microphone published {} ms",
            token_at.as_millis(),
            room_at.as_millis(),
            started.elapsed().as_millis()
        ));
        Ok::<_, String>((room, events, mic))
    }
    .await;
    let (room, mic, events) = match connected {
        Ok((room, events, mic)) => (Some(room), Some(mic), Some(events)),
        Err(e) if discord.is_some() => {
            crate::log_line(&format!("join: no Nimbus room, Discord voice only ({e})"));
            (None, None, None)
        }
        Err(e) => return Err(e),
    };
    let level = engine.level_probe();
    let discord_speaking = discord.as_ref().map(|c| c.speaking());
    let audio = Arc::new(Mutex::new(engine));

    let viewers: Arc<Mutex<HashMap<String, JoinHandle<()>>>> = Arc::default();
    let (task_ui, task_shared, task_viewers, task_audio) = (ui.clone(), shared.clone(), viewers.clone(), audio.clone());
    let (task_identity, task_name) = (identity.to_string(), name.to_string());
    let events = match room.clone().zip(events) {
        Some((task_room, mut events)) => tokio::spawn(async move {
        let mut speaking: HashSet<String> = HashSet::new();
        push_members(&task_ui, Some(&task_room), &task_identity, &task_name, &speaking, &task_shared);
        // Our ring follows the mic within 50 ms and stays lit 300 ms after a word.
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut heard_at: Option<Instant> = None;
        // People talking in the Discord call, as last shown.
        let mut discord_talking: HashSet<String> = HashSet::new();
        loop {
            let event = tokio::select! {
                _ = tick.tick() => {
                    if let Some(speaking) = &discord_speaking {
                        let now = speaking.now(Duration::from_millis(300));
                        if now != discord_talking {
                            discord_talking = now.clone();
                            crate::set_discord_speaking(&task_ui, now);
                        }
                    }
                    let switches = *task_shared.switches.lock().unwrap();
                    if level() > ME_SPEAKING_LEVEL && !switches.muted && !switches.deafened {
                        heard_at = Some(Instant::now());
                    }
                    let now = heard_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300));
                    if task_shared.me_speaking.swap(now, Ordering::Relaxed) != now {
                        push_members(&task_ui, Some(&task_room), &task_identity, &task_name, &speaking, &task_shared);
                    }
                    continue;
                }
                event = events.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            match &event {
                RoomEvent::ParticipantConnected(p) => peer_cue(&task_shared, &task_audio, &p.identity().to_string(), true),
                RoomEvent::ParticipantDisconnected(p) => peer_cue(&task_shared, &task_audio, &p.identity().to_string(), false),
                _ => {}
            }
            if matches!(event, RoomEvent::ParticipantConnected(_) | RoomEvent::ParticipantDisconnected(_)) {
                *nimbus.lock().unwrap() = room_identities(&task_room);
            }
            match event {
                RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), publication, participant } => {
                    let stream_of = (publication.source() == TrackSource::ScreenshareAudio).then(|| participant.identity().to_string());
                    let user = stream_of.is_none().then(|| participant.identity().to_string());
                    task_audio.lock().unwrap().listen(publication.sid().to_string(), track.rtc_track(), stream_of, user);
                }
                RoomEvent::TrackSubscribed { track: RemoteTrack::Video(track), publication, participant } => {
                    if publication.source() == TrackSource::Screenshare {
                        let viewer = spawn_viewer(&task_ui, track, participant.identity().to_string(), task_shared.clone());
                        task_viewers.lock().unwrap().insert(publication.sid().to_string(), viewer);
                    }
                }
                RoomEvent::TrackUnsubscribed { publication, participant, .. } => {
                    task_audio.lock().unwrap().forget(&publication.sid().to_string());
                    if let Some(viewer) = task_viewers.lock().unwrap().remove(&publication.sid().to_string()) {
                        viewer.abort();
                    }
                    let mut watched = task_shared.watched.lock().unwrap();
                    if publication.kind() == TrackKind::Video && watched.as_deref() == Some(participant.identity().to_string().as_str()) {
                        *watched = None;
                        task_audio.lock().unwrap().set_watched(None);
                        let who = participant.identity().to_string();
                        on_ui(&task_ui, move |ui| crate::stream_gone(&ui, &who));
                    }
                }
                RoomEvent::ActiveSpeakersChanged { speakers } => {
                    speaking = speakers.iter().map(|p| p.identity().to_string()).collect();
                }
                RoomEvent::Disconnected { reason } => {
                    set_status(&task_ui, None, "", false);
                    report(&task_ui, crate::i18n::tf("Голос отключился: {}", &[&format!("{reason:?}")]));
                    return;
                }
                RoomEvent::ParticipantConnected(_)
                | RoomEvent::ParticipantDisconnected(_)
                | RoomEvent::TrackMuted { .. }
                | RoomEvent::TrackUnmuted { .. }
                | RoomEvent::TrackPublished { .. }
                | RoomEvent::TrackUnpublished { .. }
                | RoomEvent::Reconnected => {}
                _ => continue,
            }
            push_members(&task_ui, Some(&task_room), &task_identity, &task_name, &speaking, &task_shared);
        }
        }),
        // Without the room there is nobody to hear speaking but the Discord side and ourselves.
        None => tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            let mut heard_at: Option<Instant> = None;
            let mut discord_talking: HashSet<String> = HashSet::new();
            push_members(&task_ui, None, &task_identity, &task_name, &HashSet::new(), &task_shared);
            loop {
                tick.tick().await;
                if let Some(speaking) = &discord_speaking {
                    let now = speaking.now(Duration::from_millis(300));
                    if now != discord_talking {
                        discord_talking = now.clone();
                        crate::set_discord_speaking(&task_ui, now);
                    }
                }
                let switches = *task_shared.switches.lock().unwrap();
                if level() > ME_SPEAKING_LEVEL && !switches.muted && !switches.deafened {
                    heard_at = Some(Instant::now());
                }
                let now = heard_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300));
                if task_shared.me_speaking.swap(now, Ordering::Relaxed) != now {
                    push_members(&task_ui, None, &task_identity, &task_name, &HashSet::new(), &task_shared);
                }
            }
        }),
    };
    let link = room.clone().map(watch_link);
    let mut session = Session { room, mic, identity: identity.to_string(), name: name.to_string(), audio, discord, golive: HashMap::new(), events: Some(events), link, viewers, share: None, channel_id: channel_id.to_string() };
    apply_switches(&mut session, current);
    session.audio.lock().unwrap().cue(audio::Cue::Join);
    Ok(session)
}

fn room_identities(room: &Room) -> HashSet<String> {
    room.remote_participants().values().map(|p| p.identity().to_string()).collect()
}

async fn leave(session: Session, shared: &Shared, ui: &Weak<AppWindow>) {
    // Stop listening first, so our own disconnect isn't reported as a drop.
    if let Some(events) = &session.events {
        events.abort();
    }
    if let Some(link) = &session.link {
        link.abort();
    }
    LINK_RTT_MS.store(0, Ordering::Relaxed);
    LINK_LOSS_PERMILLE.store(0, Ordering::Relaxed);
    // The leave sound plays before the speakers close with the session.
    session.audio.lock().unwrap().cue(audio::Cue::Leave);
    tokio::time::sleep(Duration::from_millis(250)).await;
    for (_, viewer) in session.viewers.lock().unwrap().drain() {
        viewer.abort();
    }
    if let Some(sharing) = session.share {
        stop_share(session.room.as_deref(), sharing).await;
    }
    shared.sharing.store(false, Ordering::Relaxed);
    shared.preview_on.store(false, Ordering::Relaxed);
    *shared.watched.lock().unwrap() = None;
    on_ui(ui, |ui| {
        crate::stop_all_streams(&ui);
        crate::set_sharing(&ui, false);
    });
    if let Some(room) = &session.room {
        let _ = room.close().await;
    }
}

fn share_options(prepared: &share::Prepared) -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Screenshare,
        // Frames from our own GPU encoder go out untouched; raw frames use WebRTC's encoder.
        video_encoder: if prepared.encoder.is_some() {
            livekit::options::VideoEncoderBackend::PreEncoded
        } else {
            livekit::options::VideoEncoderBackend::Auto
        },
        // Must match what the hardware encoder produces.
        video_codec: VideoCodec::H264,
        simulcast: false,
        video_encoding: Some(VideoEncoding { max_bitrate: prepared.max_bitrate, max_framerate: prepared.fps as f64 }),
        ..Default::default()
    }
}

/// Where a share to Discord goes: the call we are in.
struct DiscordShare {
    gateway: UnboundedSender<Value>,
    place: crate::golive_send::Place,
    me: String,
    session_id: String,
}

/// Starts showing a monitor or a window: in the Nimbus room, on Discord's own servers, or both
/// from one encoder. Discord's limits for the account cut the quality for everything then.
#[allow(clippy::too_many_arguments)]
async fn start_share(
    room: Option<&Room>,
    source: share::Source,
    mut quality: share::Quality,
    to: crate::golive_send::ShareTo,
    discord: Option<DiscordShare>,
    status: crate::golive_send::Status,
    on_end: Box<dyn FnOnce() + Send>,
    preview: Option<(Arc<AtomicBool>, share::Preview)>,
) -> Result<Sharing, String> {
    if discord.is_some() {
        let (height, fps) = crate::golive_send::cap_quality(quality.height, quality.fps, crate::golive_send::premium());
        quality = share::Quality { height, fps, ..quality };
    }
    let prepared = share::prepare(source, quality, !to.room())?;
    // What Discord gets is the GPU encoder's own H.264; WebRTC's software encoder is not reachable.
    let handle = prepared.encoder.as_ref().map(|e| e.handle());
    if discord.is_some() && handle.is_none() {
        return Err(crate::i18n::t("Для показа на Discord нужен аппаратный кодировщик H.264 (NVIDIA, AMD или Intel)"));
    }
    let (sid, track_sid) = if let (true, Some(room)) = (to.room(), room) {
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(prepared.source.clone()));
        let publication = room
            .local_participant()
            .publish_track(LocalTrack::Video(track), share_options(&prepared))
            .await
            .map_err(|e| crate::i18n::tf("не удалось начать показ экрана: {}", &[&e]))?;
        (Some(publication.sid()), Some(publication))
    } else {
        (None, None)
    };
    // The same encoder feeds Discord: its frames, its keyframe requests, its bitrate.
    let stream = match (discord, handle) {
        (Some(d), Some(handle)) => {
            let params = crate::golive_send::Params { width: prepared.width, height: prepared.height, fps: prepared.fps, max_bitrate: prepared.max_bitrate, sound: quality.sound };
            let (keyframes, bitrate) = (handle.clone(), handle.clone());
            let controls = crate::golive_send::Controls { keyframe: Arc::new(move || keyframes.request_keyframe()), bitrate: Arc::new(move |bps| bitrate.set_bitrate(bps)) };
            let stream = crate::golive_send::Stream::start(d.gateway, &d.place, &d.me, &d.session_id, params, controls, status);
            handle.add_sink(stream.video_sink());
            Some(stream)
        }
        _ => None,
    };
    let sound_tap = stream.as_ref().map(|s| s.sound_tap());
    let stop = Arc::new(AtomicBool::new(false));
    let target = share::Target { preview, prepared, stop: stop.clone(), frames: Arc::new(AtomicU64::new(0)), on_end: Some(on_end) };
    if let Err(e) = share::start(target) {
        if let (Some(publication), Some(room)) = (track_sid, room) {
            let _ = room.local_participant().unpublish_track(&publication.sid()).await;
        }
        return Err(e);
    }
    // Without sound the picture still goes out.
    let sound = if quality.sound {
        match publish_sound(room, source.sound_scope(), to.room(), sound_tap.clone()).await {
            Ok(sound) => Some(sound),
            Err(e) => {
                crate::log_line(&format!("stream sound: {e}"));
                None
            }
        }
    } else {
        None
    };
    Ok(Sharing { stop, sid, discord: stream, sound_tap, scope: source.sound_scope(), sound })
}

/// Captures the system sound (or one app's) as the share's sound: published to the room when
/// `publish` is set, and copied to `tap` for Discord.
async fn publish_sound(room: Option<&Room>, scope: audio::Scope, publish: bool, tap: Option<crate::golive_send::SoundTap>) -> Result<(Option<TrackSid>, audio::SystemSound), String> {
    let options = livekit::webrtc::audio_source::AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false };
    let source = livekit::webrtc::audio_source::native::NativeAudioSource::new(options, audio::SAMPLE_RATE, 2, 0);
    let sid = if let (true, Some(room)) = (publish, room) {
        let track = LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(source.clone()));
        let options = TrackPublishOptions {
            source: TrackSource::ScreenshareAudio,
            // Music and games, not speech: stereo at a high bitrate, no silence trimming.
            audio_encoding: Some(livekit::options::audio::MUSIC_HIGH_QUALITY_STEREO.encoding),
            dtx: false,
            ..Default::default()
        };
        Some(room.local_participant().publish_track(LocalTrack::Audio(track), options).await.map_err(|e| e.to_string())?.sid())
    } else {
        None
    };
    Ok((sid, audio::SystemSound::start(scope, source, tap)))
}

async fn stop_share(room: Option<&Room>, sharing: Sharing) {
    sharing.stop.store(true, Ordering::Relaxed);
    // Dropping the Discord stream tells Discord it is over.
    drop(sharing.discord);
    if let (Some(sid), Some(room)) = (sharing.sid, room) {
        let _ = room.local_participant().unpublish_track(&sid).await;
    }
    if let Some((sid, sound)) = sharing.sound {
        drop(sound);
        if let (Some(sid), Some(room)) = (sid, room) {
            let _ = room.local_participant().unpublish_track(&sid).await;
        }
    }
}

/// Reads someone's screen share; frames reach the UI only while it's the watched one.
fn spawn_viewer(ui: &Weak<AppWindow>, track: RemoteVideoTrack, identity: String, shared: Arc<Shared>) -> JoinHandle<()> {
    let ui = ui.clone();
    tokio::spawn(async move {
        let mut stream = NativeVideoStream::new(track.rtc_track());
        let mut last_shown: Option<Instant> = None;
        let mut meter = QualityMeter::new(&ui, &identity);
        let latest = Arc::new(Latest::default());
        while let Some(frame) = stream.next().await {
            if shared.watched.lock().unwrap().as_deref() != Some(identity.as_str()) {
                continue;
            }
            meter.frame(frame.buffer.height());
            if last_shown.is_some_and(|t| t.elapsed() < VIEW_FRAME_GAP) {
                continue;
            }
            last_shown = Some(Instant::now());
            // Nobody sees it (the window is minimized): no need to turn it into a picture.
            if !crate::WINDOW_SHOWN.load(Ordering::Relaxed) {
                continue;
            }
            show_latest(&ui, &latest, &identity, frame_pixels(&frame));
        }
    })
}

/// The newest picture of a watched stream, on its way to the window. When the window is slower than
/// the stream, older pictures are replaced instead of piling up in a queue (each is several megabytes).
#[derive(Default)]
struct Latest {
    slot: Mutex<Option<Pixels>>,
    queued: AtomicBool,
    /// About how wide the picture is on screen, in real pixels; 0 until it has been shown.
    shown_width: AtomicU32,
}

/// Halves a picture until it is less than twice as wide as its place on screen. Further down
/// the renderer skips pixels, and fine detail turns into jagged blocks.
fn fit_to_shown(mut pixels: Pixels, shown_width: u32) -> Pixels {
    while shown_width > 0 && pixels.width() >= shown_width * 2 {
        pixels = halved(&pixels);
    }
    pixels
}

/// Half the size, each new pixel the average of the four it replaces.
fn halved(pixels: &Pixels) -> Pixels {
    let (w, h) = (pixels.width() as usize / 2, pixels.height() as usize / 2);
    let stride = pixels.width() as usize * 4;
    let from = pixels.as_bytes();
    let mut small = Pixels::new(w as u32, h as u32);
    for (y, row) in small.make_mut_bytes().chunks_exact_mut(w * 4).enumerate() {
        let (upper, lower) = (&from[y * 2 * stride..], &from[(y * 2 + 1) * stride..]);
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            for (c, out) in pixel.iter_mut().enumerate() {
                let (a, b) = (x * 8 + c, x * 8 + 4 + c);
                *out = ((upper[a] as u32 + upper[b] as u32 + lower[a] as u32 + lower[b] as u32 + 2) / 4) as u8;
            }
        }
    }
    small
}

fn show_latest(ui: &Weak<AppWindow>, latest: &Arc<Latest>, who: &str, pixels: Pixels) {
    *latest.slot.lock().unwrap() = Some(pixels);
    if latest.queued.swap(true, Ordering::AcqRel) {
        return; // the window will take the newest one when it gets to it
    }
    let (latest, who) = (latest.clone(), who.to_string());
    on_ui(ui, move |ui| {
        latest.queued.store(false, Ordering::Release);
        latest.shown_width.store(crate::stream_shown_width(&ui, &who), Ordering::Relaxed);
        if let Some(pixels) = latest.slot.lock().unwrap().take() {
            crate::show_stream_picture(&ui, &who, Image::from_rgba8(pixels));
        }
    });
}

/// Converts a decoded frame to RGBA for Slint.
fn frame_pixels(frame: &BoxVideoFrame) -> Pixels {
    let i420 = frame.buffer.to_i420();
    let (w, h) = (i420.width(), i420.height());
    let (stride_y, stride_u, stride_v) = i420.strides();
    let (y, u, v) = i420.data();
    let mut pixels = Pixels::new(w, h);
    // libyuv "ABGR" is R,G,B,A in memory, which is what Slint's Rgba8Pixel holds.
    yuv_helper::i420_to_abgr(y, stride_y, u, stride_u, v, stride_v, pixels.make_mut_bytes(), w * 4, w as i32, h as i32);
    pixels
}

fn apply_switches(session: &mut Session, switches: Switches) {
    if let Some(mic) = &session.mic {
        if switches.muted || switches.deafened {
            mic.mute();
        } else {
            mic.unmute();
        }
    }
    session.audio.lock().unwrap().set_deafened(switches.deafened);
    if let Some(call) = &mut session.discord {
        call.set_switches(switches.muted, switches.deafened);
    }
}

/// The people shown for the call: us, and whoever is in the Nimbus room (nobody without one).
fn push_members(ui: &Weak<AppWindow>, room: Option<&Room>, identity: &str, name: &str, speaking: &HashSet<String>, shared: &Shared) {
    let switches = *shared.switches.lock().unwrap();
    let mut members = vec![Member {
        identity: identity.to_string(),
        name: crate::readable(name),
        muted: switches.muted || switches.deafened,
        deafened: switches.deafened,
        speaking: shared.me_speaking.load(Ordering::Relaxed),
        streaming: shared.sharing.load(Ordering::Relaxed),
    }];
    for participant in room.iter().flat_map(|r| r.remote_participants().into_values()) {
        let identity = participant.identity().to_string();
        let publications = participant.track_publications();
        let muted = publications.values().find(|p| p.kind() == TrackKind::Audio).map(|p| p.is_muted()).unwrap_or(true);
        let streaming = publications.values().any(|p| p.source() == TrackSource::Screenshare && !p.is_muted());
        members.push(Member {
            name: crate::readable(&participant.name()),
            speaking: speaking.contains(&identity),
            identity,
            muted,
            deafened: false,
            streaming,
        });
    }
    // Our own avatar in the user panel lights up too, so you can see the mic picks you up.
    let me_speaking = members[0].speaking;
    let avatars: HashMap<String, crate::Pixels> =
        members.iter().filter_map(|m| crate::voice_avatar(&m.identity).map(|px| (m.identity.clone(), px))).collect();
    on_ui(ui, move |ui| {
        ui.set_me_speaking(me_speaking);
        let items: Vec<VoiceMember> = members
            .iter()
            .map(|m| VoiceMember {
                identity: m.identity.clone().into(),
                name: m.name.clone().into(),
                initial: m.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                avatar: avatars.get(&m.identity).map(|px| slint::Image::from_rgba8(px.clone())).unwrap_or_default(),
                has_avatar: avatars.contains_key(&m.identity),
                muted: m.muted,
                deafened: m.deafened,
                speaking: m.speaking,
                streaming: m.streaming,
                discord: false,
                decoration: crate::voice_frame(&m.identity).map(slint::Image::from_rgba8).unwrap_or_default(),
                has_decoration: crate::voice_frame(&m.identity).is_some(),
            })
            .collect();
        ui.set_voice_members(ModelRc::new(VecModel::from(items)));
        crate::hide_doubles(&ui);
    });
}

/// `--voice-check`: joins a test room with the microphone for a few seconds and
/// reports what happened, so voice can be verified without the UI.
pub async fn self_check(server: &str) -> String {
    let http = reqwest::Client::new();
    let result: Result<String, String> = async {
        let (url, token) = fetch_token(&http, server, "999000333", "1000000000000000001", "voice-check").await?;
        // The same devices and audio engine as the app.
        let devices = Devices::open();
        let (mics, speakers) = (devices.mics.len(), devices.speakers.len());
        let names = |list: &[audio::Device]| list.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ");
        crate::log_line(&format!("mics: {}; speakers: {}", names(&devices.mics), names(&devices.speakers)));
        let (room, mut events) = Room::connect(&url, &token, RoomOptions::default()).await.map_err(|e| format!("connect: {e}"))?;
        let mut engine = audio::Engine::start(devices.mic_now(), devices.speaker_now());
        let mic = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(engine.source()));
        let options = TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() };
        room.local_participant().publish_track(LocalTrack::Audio(mic), options).await.map_err(|e| format!("publish: {e}"))?;

        let mut subscribed = 0;
        let deadline = tokio::time::sleep(Duration::from_secs(6));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = events.recv() => match event {
                    Some(RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), publication, .. }) => {
                        subscribed += 1;
                        engine.listen(publication.sid().to_string(), track.rtc_track(), None, None);
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        let stats = engine.stats();
        let heard = format!(
            "mic_frames={} mic_peak={} remote_frames={} remote_peak={} played_frames={}",
            stats.mic_frames.load(Ordering::Relaxed),
            stats.mic_peak.load(Ordering::Relaxed),
            stats.remote_frames.load(Ordering::Relaxed),
            stats.remote_peak.load(Ordering::Relaxed),
            stats.played_frames.load(Ordering::Relaxed),
        );
        drop(engine);
        let published = room.local_participant().track_publications().len();
        let remotes = room.remote_participants().len();
        let _ = room.close().await;
        Ok(format!(
            "voice-check OK: mics={mics} speakers={speakers} published={published} remote_participants={remotes} remote_audio_subscribed={subscribed} {heard}"
        ))
    }
    .await;
    result.unwrap_or_else(|e| format!("voice-check FAILED: {e}"))
}

/// `--video-check`: shares the screen into a test room for a few seconds and
/// reads any other participant's video, reporting frame counts both ways.
pub async fn video_check(server: &str) -> String {
    let http = reqwest::Client::new();
    let result: Result<String, String> = async {
        let (url, token) = fetch_token(&http, server, "999000444", "1000000000000000001", "video-check").await?;
        let (room, mut events) = Room::connect(&url, &token, RoomOptions::default()).await.map_err(|e| format!("connect: {e}"))?;

        // NIMBUS_CHECK_WINDOW (part of a title) and NIMBUS_CHECK_HEIGHT try the other paths.
        let source = match std::env::var("NIMBUS_CHECK_WINDOW") {
            Ok(part) => share::sources()
                .into_iter()
                .find(|s| s.is_window && s.name.contains(&part))
                .map(|s| s.source)
                .ok_or(format!("no window titled *{part}*"))?,
            Err(_) => share::Source::primary()?,
        };
        let height = std::env::var("NIMBUS_CHECK_HEIGHT").ok().and_then(|h| h.parse().ok()).unwrap_or(0);
        let prepared = share::prepare(source, share::Quality { height, ..Default::default() }, false)?;
        let (width, height, encoder) = (prepared.width, prepared.height, prepared.encoder_name.clone());
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(prepared.source.clone()));
        room.local_participant().publish_track(LocalTrack::Video(track.clone()), share_options(&prepared)).await.map_err(|e| format!("publish: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let target = share::Target { preview: None, prepared, stop: stop.clone(), frames: sent.clone(), on_end: None };
        share::start(target)?;
        let (_, sound) = publish_sound(Some(&room), source.sound_scope(), true, None).await?;

        let received = Arc::new(AtomicU64::new(0));
        let mut first_frame = String::from("none");
        let mut readers = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_secs(8));
        tokio::pin!(deadline);
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                Some(info) = frame_rx.recv() => first_frame = info,
                event = events.recv() => match event {
                    Some(RoomEvent::TrackSubscribed { track: RemoteTrack::Video(track), .. }) => {
                        let (received, frame_tx) = (received.clone(), frame_tx.clone());
                        readers.push(tokio::spawn(async move {
                            let mut stream = NativeVideoStream::new(track.rtc_track());
                            while let Some(frame) = stream.next().await {
                                if received.fetch_add(1, Ordering::Relaxed) == 0 {
                                    let px = frame_pixels(&frame);
                                    let _ = frame_tx.send(format!("{}x{}", px.width(), px.height()));
                                }
                            }
                        }));
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        // What actually left for the network: frames, keyframes, viewer complaints.
        if let Ok(stats) = track.get_stats().await {
            for stat in stats {
                if let livekit::webrtc::stats::RtcStats::OutboundRtp(out) = stat {
                    crate::log_line(&format!(
                        "outbound: encoder={} frames_encoded={} keyframes={} frames_sent={} bytes={} pli={} nack={} size={}x{}",
                        out.outbound.encoder_implementation,
                        out.outbound.frames_encoded,
                        out.outbound.key_frames_encoded,
                        out.outbound.frames_sent,
                        out.sent.bytes_sent,
                        out.outbound.pli_count,
                        out.outbound.nack_count,
                        out.outbound.frame_width,
                        out.outbound.frame_height
                    ));
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let sound_report = format!("sound_frames={} sound_peak={}", sound.frames.load(Ordering::Relaxed), sound.peak.load(Ordering::Relaxed));
        drop(sound);
        for reader in readers {
            reader.abort();
        }
        let _ = room.close().await;
        Ok(format!(
            "video-check OK: encoder={encoder} screen={width}x{height} sent_frames={} received_frames={} first_received={first_frame} {sound_report}",
            sent.load(Ordering::Relaxed),
            received.load(Ordering::Relaxed)
        ))
    }
    .await;
    result.unwrap_or_else(|e| format!("video-check FAILED: {e}"))
}

/// Asks the Nimbus token server for the LiveKit URL and a room token.
/// Where a voice server's address points: its host and port.
fn server_target(server: &str) -> Option<(String, u16)> {
    let url = reqwest::Url::parse(&crate::trust::parse(server).ok()?.normal()).ok()?;
    Some((url.host_str()?.trim_matches(['[', ']']).to_string(), url.port_or_known_default()?))
}

/// Whether anything takes a connection at a voice server's address, within a moment.
async fn server_answers(server: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Some((host, port)) = server_target(server) else { return false };
    let probe = tokio::task::spawn_blocking(move || {
        (host.as_str(), port).to_socket_addrs().is_ok_and(|mut found| found.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()))
    });
    matches!(tokio::time::timeout(Duration::from_millis(600), probe).await, Ok(Ok(true)))
}

async fn fetch_token(http: &reqwest::Client, server: &str, room: &str, identity: &str, name: &str) -> Result<(String, String), String> {
    // Plain http only for home networks, this computer and Tailscale; the open internet needs https.
    let addr = crate::trust::parse(server)?;
    crate::trust::check_transport(&addr)?;
    // Only the checked scheme, host and port are used: a path or query in the address is dropped.
    let server = addr.normal();
    let res = http
        .post(format!("{server}/token"))
        .json(&json!({ "room": room, "identity": identity, "name": name }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| crate::i18n::tf("сервер голоса недоступен ({}): {}", &[&server, &e]))?;
    if !res.status().is_success() {
        return Err(crate::i18n::tf("сервер голоса ответил {}", &[&res.status()]));
    }
    let body: Value = res.json().await.map_err(|e| e.to_string())?;
    let url = body["url"].as_str().ok_or_else(|| crate::i18n::t("сервер голоса не прислал адрес"))?;
    let token = body["token"].as_str().ok_or_else(|| crate::i18n::t("сервер голоса не прислал токен"))?;
    crate::trust::check_ws(url, &addr)?;
    Ok((url.to_string(), token.to_string()))
}

#[cfg(test)]
mod shown_tests {
    use super::*;

    fn picture(w: u32, h: u32, values: &[u8]) -> Pixels {
        let mut p = Pixels::new(w, h);
        p.make_mut_bytes().copy_from_slice(values);
        p
    }

    #[test]
    fn a_picture_is_halved_only_when_far_larger_than_its_place() {
        let big = Pixels::new(1920, 1080);
        assert_eq!(fit_to_shown(big.clone(), 0).width(), 1920, "unknown place: left alone");
        assert_eq!(fit_to_shown(big.clone(), 1000).width(), 1920, "under twice as wide: left alone");
        assert_eq!(fit_to_shown(big.clone(), 960).width(), 960);
        let small = fit_to_shown(big, 400);
        assert_eq!((small.width(), small.height()), (480, 270));
    }

    #[test]
    fn halving_averages_the_four_pixels() {
        let p = picture(2, 2, &[0, 0, 0, 255, 100, 0, 0, 255, 0, 40, 0, 255, 100, 40, 200, 255]);
        let small = halved(&p);
        assert_eq!((small.width(), small.height()), (1, 1));
        assert_eq!(small.as_bytes(), &[50, 20, 50, 255]);
    }

    #[test]
    fn an_odd_size_stays_inside_the_picture() {
        let small = halved(&Pixels::new(5, 3));
        assert_eq!((small.width(), small.height()), (2, 1));
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn a_voice_server_address_gives_its_host_and_port() {
        assert_eq!(server_target("http://localhost:7890"), Some(("localhost".to_string(), 7890)));
        assert_eq!(server_target("https://voice.example.com"), Some(("voice.example.com".to_string(), 443)));
    }

    #[test]
    fn loss_as_tenths_of_a_percent() {
        assert_eq!(loss_permille(0, 0), 0);
        assert_eq!(loss_permille(0, 1000), 0);
        assert_eq!(loss_permille(10, 990), 10, "1 percent");
        assert_eq!(loss_permille(-5, 100), 0, "a negative count is nothing");
    }
}