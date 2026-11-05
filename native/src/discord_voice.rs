//! Discord's own voice, so people without Nimbus are heard too.
//!
//! The account joins the Discord voice channel the way the official client
//! does: a voice state update (opcode 4) on the main gateway, then Discord
//! answers with the session id and a voice server. Songbird does the voice
//! connection itself, end-to-end encryption (DAVE) included.
//! Calls in DMs and groups go the same way, with no guild: the DM's channel
//! id stands in for the guild id where the voice connection needs one.
//!
//! Voices from Discord go into our mix (audio.rs), except people who are in
//! the Nimbus room too: they're already heard there. Our mic goes to both.
//! Cameras share the connection (discord_video.rs).
//!
//! This is an unofficial client using a user account for voice. Discord may
//! ban accounts for that; the user chose it knowing so.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::{Input, RawAdapter};
use songbird::{Config, ConnectionInfo, Driver};
use symphonia_core::io::MediaSource;
use tokio::sync::mpsc::UnboundedSender;

use crate::audio;

/// The opcode 4 payload: join (`channel` set), move, or leave (`None`).
/// `guild_id` is `None` for a call in a DM or group: it goes out as `null`.
pub fn voice_state(guild_id: Option<&str>, channel_id: Option<&str>, muted: bool, deafened: bool) -> Value {
    voice_state_with_video(guild_id, channel_id, muted, deafened, false)
}

/// The same with our camera on or off (self_video), so others see it is on.
pub fn voice_state_with_video(guild_id: Option<&str>, channel_id: Option<&str>, muted: bool, deafened: bool, video: bool) -> Value {
    json!({
        "op": 4,
        "d": { "guild_id": guild_id, "channel_id": channel_id, "self_mute": muted, "self_deaf": deafened, "self_video": video }
    })
}

/// Whether a VOICE_SERVER_UPDATE is for our call. In a guild it names the guild; in a DM or
/// group it has no guild and names the DM's channel instead.
pub fn server_is_ours(data: &Value, guild_id: Option<&str>, channel_id: &str) -> bool {
    match guild_id {
        Some(guild) => data["guild_id"].as_str() == Some(guild),
        None => data["guild_id"].is_null() && data["channel_id"].as_str() == Some(channel_id),
    }
}

/// Whether one of our VOICE_STATE_UPDATEs is for the same guild (or, without one, for no guild).
fn state_is_ours(data: &Value, guild_id: Option<&str>, user_id: &str) -> bool {
    data["user_id"].as_str() == Some(user_id) && data["guild_id"].as_str() == guild_id
}

/// Whether our account's voice state says another client of ours (say, the official app) took the voice connection.
pub fn taken_elsewhere(data: &Value, me: &str, our_session: &str) -> bool {
    data["user_id"].as_str() == Some(me)
        && !data["channel_id"].is_null()
        && !our_session.is_empty()
        && data["session_id"].as_str().is_some_and(|session| session != our_session)
}

/// One Discord voice call, from asking to join until leaving.
pub struct Call {
    // Another client took the voice connection: leaving must not tell Discord anything, or it would drop that one.
    silent_leave: bool,
    driver: Driver,
    gateway: UnboundedSender<Value>,
    /// None in a DM or group call.
    guild_id: Option<String>,
    channel_id: String,
    user_id: String,
    session_id: Option<String>,
    server: Option<(String, String)>,
    connecting: bool,
    speaking: Speaking,
    /// The switches last sent, so the camera's state goes out with them.
    switches: (bool, bool),
    camera: bool,
    video: crate::discord_video::Video,
}

/// When each Discord user's voice was last heard, by user id.
#[derive(Clone, Default)]
pub struct Speaking(Arc<Mutex<HashMap<String, std::time::Instant>>>);

impl Speaking {
    /// People heard within `within`: the speaking ring stays on through short pauses.
    pub fn now(&self, within: Duration) -> HashSet<String> {
        let mut heard = self.0.lock().unwrap();
        heard.retain(|_, at| at.elapsed() < within * 4);
        heard.iter().filter(|(_, at)| at.elapsed() < within).map(|(id, _)| id.clone()).collect()
    }
}

/// Peak (of 32768) above which a Discord voice counts as talking, not hiss.
const SPEAKING_LEVEL: i16 = 1200;

impl Call {
    /// Asks Discord to put us in the voice channel (`guild_id` None: the call of a DM or group).
    /// Call inside the tokio runtime.
    pub fn join(
        gateway: UnboundedSender<Value>,
        guild_id: Option<&str>,
        channel_id: &str,
        user_id: &str,
        engine: &audio::Engine,
        nimbus: Arc<Mutex<HashSet<String>>>,
        muted: bool,
        deafened: bool,
        display: crate::discord_video::Display,
    ) -> Self {
        let (video, link) = crate::discord_video::Video::start(user_id, display);
        let config = Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz48000))).video(link);
        let mut driver = Driver::new(config);
        let speaking = Speaking::default();
        let listener = Listener {
            sink: engine.sink(),
            me: user_id.to_string(),
            nimbus,
            users: Mutex::default(),
            speaking: speaking.clone(),
        };
        let listener = Arc::new(listener);
        driver.add_global_event(CoreEvent::SpeakingStateUpdate.into(), Handler(listener.clone()));
        driver.add_global_event(CoreEvent::VoiceTick.into(), Handler(listener.clone()));
        driver.add_global_event(CoreEvent::ClientDisconnect.into(), Handler(listener));
        let mic = Mic { tap: engine.tap(), real: 0, filler: 0 };
        driver.play_input(Input::from(RawAdapter::new(mic, audio::SAMPLE_RATE, 1)));
        driver.mute(muted || deafened);
        let _ = gateway.send(voice_state(guild_id, Some(channel_id), muted, deafened));
        crate::log_line(&format!("discord voice: asked to join {channel_id}"));
        Self {
            silent_leave: false,
            driver,
            gateway,
            guild_id: guild_id.map(String::from),
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            session_id: None,
            server: None,
            connecting: false,
            speaking,
            switches: (muted, deafened),
            camera: false,
            video,
        }
    }

    /// Who in the Discord call was heard lately (shared, updated as sound arrives).
    pub fn speaking(&self) -> Speaking {
        self.speaking.clone()
    }

    /// Another client has the voice connection now: this one goes without telling Discord.
    pub fn leave_silently(&mut self) {
        self.silent_leave = true;
    }

    /// Our own VOICE_STATE_UPDATE from the main gateway: carries the session id.
    pub fn on_state(&mut self, data: &Value) {
        if !state_is_ours(data, self.guild_id.as_deref(), &self.user_id) {
            return;
        }
        if data["channel_id"].as_str() != Some(self.channel_id.as_str()) {
            return; // an old state, or we were moved or kicked
        }
        self.session_id = data["session_id"].as_str().map(String::from);
        self.try_connect();
    }

    /// VOICE_SERVER_UPDATE: where to connect, and the token for it.
    pub fn on_server(&mut self, data: &Value) {
        if !server_is_ours(data, self.guild_id.as_deref(), &self.channel_id) {
            return;
        }
        let (Some(token), Some(endpoint)) = (data["token"].as_str(), data["endpoint"].as_str()) else { return };
        self.server = Some((token.to_string(), endpoint.to_string()));
        crate::voiceping::set_server(endpoint);
        self.connecting = false; // a new server (Discord moved the call): connect again
        self.try_connect();
    }

    fn try_connect(&mut self) {
        let (Some(session_id), Some((token, endpoint))) = (self.session_id.clone(), self.server.clone()) else { return };
        if self.connecting {
            return;
        }
        let id = |s: &str| s.parse::<u64>().ok().and_then(NonZeroU64::new);
        // A DM call has no guild: the voice server knows it by the DM's channel id.
        let server = self.guild_id.as_deref().unwrap_or(&self.channel_id);
        let (Some(guild), Some(channel), Some(user)) = (id(server), id(&self.channel_id), id(&self.user_id)) else { return };
        self.connecting = true;
        let info = ConnectionInfo {
            channel_id: ChannelId(channel),
            endpoint,
            guild_id: GuildId(guild),
            session_id,
            token,
            user_id: UserId(user),
        };
        let connect = self.driver.connect(info);
        tokio::spawn(async move {
            match connect.await {
                Ok(()) => crate::log_line("discord voice: connected"),
                Err(e) => crate::log_line(&format!("discord voice: connect failed: {e:?}")),
            }
        });
    }

    /// (guild, channel, voice session id) once Discord has given us a session; None in a DM
    /// call (Go Live there is not done).
    pub fn place(&self) -> Option<(&str, &str, &str)> {
        Some((self.guild_id.as_deref()?, &self.channel_id, self.session_id.as_deref()?))
    }

    /// A call in a DM or group rather than a guild voice channel.
    pub fn is_dm(&self) -> bool {
        self.guild_id.is_none()
    }

    pub fn set_switches(&mut self, muted: bool, deafened: bool) {
        self.driver.mute(muted || deafened);
        self.switches = (muted, deafened);
        self.send_state();
    }

    /// Our camera on or off: the main gateway's voice state says so (the picture
    /// itself is declared and sent on the voice connection, see ideo).
    pub fn verification_code(&self, user: u64) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.video.verification_code(user)
    }

    pub fn set_camera(&mut self, on: bool) {
        self.camera = on;
        self.send_state();
    }

    /// The call's video side: our camera out, other people's in.
    pub fn video(&self) -> &crate::discord_video::Video {
        &self.video
    }

    fn send_state(&self) {
        let (muted, deafened) = self.switches;
        let _ = self.gateway.send(voice_state_with_video(self.guild_id.as_deref(), Some(&self.channel_id), muted, deafened, self.camera));
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        crate::voiceping::reset();
        self.driver.leave();
        if !self.silent_leave {
            let _ = self.gateway.send(voice_state(self.guild_id.as_deref(), None, false, false));
        }
        crate::log_line("discord voice: left");
    }
}

/// Feeds our mic to Songbird as raw 48 kHz mono f32. Songbird reads at the
/// pace it sends, so the mic's own pace drives it: a read waits for fresh
/// sound, and silence fills in only when the mic gives nothing at all.
struct Mic {
    tap: Arc<audio::Tap>,
    /// Samples handed over that were real sound / silence filler, logged now and then.
    real: u64,
    filler: u64,
}

impl Read for Mic {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = (buf.len() / 4).clamp(1, audio::SAMPLE_RATE as usize / 50);
        let mut pcm = vec![0i16; want];
        let mut got = self.tap.take(&mut pcm);
        // The mic delivers every 10 ms; 40 ms of nothing means it's gone quiet for real.
        for _ in 0..8 {
            if got > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            got = self.tap.take(&mut pcm);
        }
        // No mic sound at all (it failed or is switching): silence keeps the track alive.
        let n = if got == 0 { want } else { got };
        let before = (self.real + self.filler) / (audio::SAMPLE_RATE as u64 * 10);
        if got == 0 { self.filler += n as u64 } else { self.real += n as u64 }
        if (self.real + self.filler) / (audio::SAMPLE_RATE as u64 * 10) != before {
            crate::log_line(&format!("discord voice mic: {} s sound, {} s filler", self.real / 48_000, self.filler / 48_000));
        }
        for (i, s) in pcm[..n].iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&(*s as f32 / 32768.0).to_le_bytes());
        }
        Ok(n * 4)
    }
}

impl Seek for Mic {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for Mic {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// `--discord-mic-check`: runs our mic through Songbird's own parsing, the
/// way the Discord call reads it, and reports pace and level. No Discord needed.
pub async fn mic_check() -> String {
    let engine = audio::Engine::start(None, None);
    let mic = Mic { tap: engine.tap(), real: 0, filler: 0 };
    let input = Input::from(RawAdapter::new(mic, audio::SAMPLE_RATE, 1));
    let codecs = songbird::input::codecs::get_codec_registry();
    let probe = songbird::input::codecs::get_probe();
    let started = std::time::Instant::now();
    let mut input = match input.make_playable_async(codecs, probe).await {
        Ok(input) => input,
        Err(e) => return format!("discord-mic-check FAILED: parse: {e:?}"),
    };
    let parse_ms = started.elapsed().as_millis();
    let Some(parsed) = input.parsed_mut() else { return "discord-mic-check FAILED: not parsed".into() };
    let started = std::time::Instant::now();
    let (mut packets, mut samples, mut peak) = (0, 0u64, 0f32);
    while started.elapsed() < Duration::from_secs(3) {
        match parsed.format.next_packet() {
            Ok(packet) => {
                packets += 1;
                for bytes in packet.buf().chunks_exact(4) {
                    peak = peak.max(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).abs());
                    samples += 1;
                }
            }
            Err(e) => return format!("discord-mic-check FAILED after {packets} packets: {e:?}"),
        }
    }
    format!(
        "discord-mic-check: parse {parse_ms} ms, {packets} packets, {:.2} s of audio in 3 s, peak {:.3}",
        samples as f64 / audio::SAMPLE_RATE as f64,
        peak
    )
}

/// Puts Discord voices into our mix.
struct Listener {
    sink: audio::Sink,
    me: String,
    /// Identities in the Nimbus room; their Discord audio is a duplicate.
    nimbus: Arc<Mutex<HashSet<String>>>,
    /// Who sends on which SSRC.
    users: Mutex<HashMap<u32, String>>,
    speaking: Speaking,
}

struct Handler(Arc<Listener>);

#[async_trait::async_trait]
impl EventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let l = &self.0;
        match ctx {
            EventContext::SpeakingStateUpdate(s) => {
                if let Some(user) = s.user_id {
                    l.users.lock().unwrap().insert(s.ssrc, user.0.to_string());
                }
            }
            EventContext::VoiceTick(tick) => {
                let users = l.users.lock().unwrap().clone();
                let nimbus = l.nimbus.lock().unwrap().clone();
                for (ssrc, data) in &tick.speaking {
                    let user = users.get(ssrc);
                    if user.is_some_and(|u| *u == l.me || nimbus.contains(u)) {
                        continue;
                    }
                    if let Some(pcm) = &data.decoded_voice {
                        let loud = pcm.iter().any(|s| s.unsigned_abs() > SPEAKING_LEVEL as u16);
                        // Their own volume for us, as set in their menu.
                        let mut pcm = pcm.clone();
                        if user.is_none_or(|u| audio::apply_user_volume(u, &mut pcm)) {
                            l.sink.push(&format!("discord:{ssrc}"), &pcm);
                        }
                        if let (true, Some(user)) = (loud, user) {
                            l.speaking.0.lock().unwrap().insert(user.clone(), std::time::Instant::now());
                        }
                    }
                }
            }
            EventContext::ClientDisconnect(d) => {
                let gone = d.user_id.0.to_string();
                let mut users = l.users.lock().unwrap();
                users.retain(|ssrc, user| {
                    if *user == gone {
                        l.sink.forget(&format!("discord:{ssrc}"));
                    }
                    *user != gone
                });
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_client_taking_voice_is_noticed() {
        let state = |user: &str, session: &str, channel: Value| json!({ "user_id": user, "session_id": session, "channel_id": channel });
        assert!(taken_elsewhere(&state("1", "other", json!("42")), "1", "ours"));
        // Our own session, somebody else, or leaving voice: nothing to do.
        assert!(!taken_elsewhere(&state("1", "ours", json!("42")), "1", "ours"));
        assert!(!taken_elsewhere(&state("2", "other", json!("42")), "1", "ours"));
        assert!(!taken_elsewhere(&state("1", "other", Value::Null), "1", "ours"));
        // Before READY we don't know our own session.
        assert!(!taken_elsewhere(&state("1", "other", json!("42")), "1", ""));
    }

#[test]
    fn a_dm_call_joins_and_leaves_with_no_guild() {
        let join = voice_state(None, Some("42"), true, false);
        assert_eq!(join["op"], 4);
        assert!(join["d"]["guild_id"].is_null());
        assert_eq!(join["d"]["channel_id"], "42");
        assert_eq!(join["d"]["self_mute"], true);
        let leave = voice_state(None, None, false, false);
        assert!(leave["d"]["guild_id"].is_null() && leave["d"]["channel_id"].is_null());
        assert_eq!(voice_state(Some("7"), Some("42"), false, false)["d"]["guild_id"], "7");
    }

    #[test]
    fn the_camera_goes_out_with_the_voice_state() {
        assert_eq!(voice_state(Some("7"), Some("42"), false, false)["d"]["self_video"], false);
        let on = voice_state_with_video(None, Some("42"), true, false, true);
        assert_eq!(on["d"]["self_video"], true);
        assert_eq!(on["d"]["self_mute"], true);
        assert!(on["d"]["guild_id"].is_null());
    }

    #[test]
    fn a_dm_voice_server_is_matched_by_its_channel() {
        let dm = json!({ "channel_id": "42", "token": "t", "endpoint": "e" });
        assert!(server_is_ours(&dm, None, "42"));
        assert!(!server_is_ours(&dm, None, "43"), "another DM's call");
        assert!(!server_is_ours(&dm, Some("7"), "42"), "we are in a guild");
        let guild = json!({ "guild_id": "7", "token": "t", "endpoint": "e" });
        assert!(server_is_ours(&guild, Some("7"), "42"));
        assert!(!server_is_ours(&guild, Some("8"), "42"));
        assert!(!server_is_ours(&json!({ "guild_id": "7", "channel_id": "42" }), None, "42"), "a guild's, not the DM's");
    }

    #[test]
    fn our_dm_voice_state_has_no_guild() {
        let dm = json!({ "user_id": "1", "guild_id": null, "channel_id": "42", "session_id": "s" });
        assert!(state_is_ours(&dm, None, "1"));
        assert!(!state_is_ours(&dm, Some("7"), "1"));
        assert!(!state_is_ours(&dm, None, "2"), "someone else's");
        assert!(state_is_ours(&json!({ "user_id": "1", "guild_id": "7" }), Some("7"), "1"));
    }
}
