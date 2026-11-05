//! Watching someone's Discord Go Live stream.
//!
//! Stage 1: ask Discord to watch (main gateway opcode 20) and connect to the
//! stream's own media server the way a viewer does (voice gateway v8 with
//! `video: true`, UDP, DAVE end-to-end encryption).
//! Stage 2: open the transport encryption of each video packet, put H.264
//! frames back together from RTP, and take DAVE's encryption off each frame.
//! Decoding and showing the picture are the next stage.
//!
//! Like the Discord voice in discord_voice.rs this uses the user's account
//! from an unofficial client; Discord may act on that.

use std::collections::{BTreeMap, HashMap, HashSet};

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Says how the stream is doing, for the viewer's title line.
pub type Status = Arc<dyn Fn(String) + Send + Sync>;
/// Shows a decoded picture.
pub type Display = Arc<dyn Fn(crate::Pixels) + Send + Sync>;

/// One stream being watched, from asking until it's closed.
pub struct Watch {
    gateway: UnboundedSender<Value>,
    key: String,
    me: String,
    session_id: String,
    rtc_server_id: Option<String>,
    server: Option<(String, String)>,
    status: Status,
    display: Display,
    /// Where the stream's sound goes: our mix.
    sound: crate::audio::Sink,
    task: Option<JoinHandle<()>>,
}

impl Watch {
    /// Asks Discord to let us watch `streamer` in the voice channel we're in.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        gateway: UnboundedSender<Value>,
        guild: &str,
        channel: &str,
        streamer: &str,
        me: &str,
        session_id: &str,
        status: Status,
        display: Display,
        sound: crate::audio::Sink,
    ) -> Self {
        let key = format!("guild:{guild}:{channel}:{streamer}");
        let _ = gateway.send(json!({ "op": 20, "d": { "stream_key": key } }));
        crate::log_line(&format!("go live: asked to watch {key}"));
        status(crate::i18n::t("подключение…"));
        Self {
            gateway,
            key,
            me: me.to_string(),
            session_id: session_id.to_string(),
            rtc_server_id: None,
            server: None,
            status,
            display,
            sound,
            task: None,
        }
    }

    /// STREAM_CREATE / STREAM_SERVER_UPDATE / STREAM_DELETE from the main
    /// gateway. Returns true once the stream is over.
    pub fn on_event(&mut self, kind: &str, data: &Value) -> bool {
        if data["stream_key"].as_str() != Some(self.key.as_str()) {
            return false;
        }
        match kind {
            "STREAM_CREATE" => {
                crate::log_line(&format!("go live: STREAM_CREATE {}", short(data)));
                self.rtc_server_id = data["rtc_server_id"].as_str().map(String::from);
            }
            "STREAM_SERVER_UPDATE" => {
                crate::log_line(&format!("go live: STREAM_SERVER_UPDATE endpoint={}", data["endpoint"]));
                if let (Some(endpoint), Some(token)) = (data["endpoint"].as_str(), data["token"].as_str()) {
                    self.server = Some((endpoint.to_string(), token.to_string()));
                }
            }
            "STREAM_DELETE" => {
                crate::log_line(&format!("go live: STREAM_DELETE {}", short(data)));
                // "user_requested" is our own earlier viewing of this stream being closed
                // as we open it again; anything else (stream_ended…) ends this one.
                if data["reason"].as_str() == Some("user_requested") {
                    return false;
                }
                if let Some(task) = self.task.take() {
                    task.abort();
                }
                return true;
            }
            _ => return false,
        }
        self.try_connect();
        false
    }

    fn try_connect(&mut self) {
        let (Some(server_id), Some((endpoint, token))) = (self.rtc_server_id.clone(), self.server.clone()) else { return };
        if let Some(old) = self.task.take() {
            old.abort(); // Discord moved the stream to another server
        }
        let streamer = self.key.rsplit(':').next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let volume_key = format!("stream:{streamer}");
        let link = Link { endpoint, token, server_id, me: self.me.clone(), session_id: self.session_id.clone(), streamer, display: self.display.clone(), sound: self.sound.clone(), volume_key };
        let status = self.status.clone();
        self.task = Some(tokio::spawn(async move {
            if let Err(e) = run(link, status.clone()).await {
                crate::log_line(&format!("go live: stopped: {e}"));
                status(crate::i18n::tf("ошибка: {}", &[&e]));
            }
        }));
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = self.gateway.send(json!({ "op": 19, "d": { "stream_key": self.key } }));
        // Its place in our mix (audio.rs) is named after the stream, like its volume: two can play at once.
        self.sound.forget(&format!("stream:{}", self.key.rsplit(':').next().unwrap_or_default()));
        crate::log_line("go live: stopped watching");
    }
}

/// What the stream's media server needs from us.
struct Link {
    endpoint: String,
    token: String,
    server_id: String,
    me: String,
    session_id: String,
    /// Whose stream it is: DAVE keys frames by sender.
    streamer: u64,
    display: Display,
    sound: crate::audio::Sink,
    /// The stream's own volume among the user volumes ("stream:<streamer>").
    volume_key: String,
}

/// The RTP payload types we asked H.264 video, its retransmissions (RTX)
/// and the stream's sound (Opus) to come with.
pub(crate) const H264_PT: u8 = 101;
pub(crate) const RTX_PT: u8 = 102;
pub(crate) const OPUS_PT: u8 = 120;

/// The streams turned silent with their sound button; all the others being watched are heard together.
static SOUND_OFF: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

pub fn stream_sound_off(streamer: &str) -> bool {
    SOUND_OFF.lock().unwrap().contains(streamer)
}

pub fn set_stream_sound_off(streamer: &str, off: bool) {
    let mut silent = SOUND_OFF.lock().unwrap();
    if off {
        silent.insert(streamer.to_string());
    } else {
        silent.remove(streamer);
    }
}

type OpusDecoder = songbird::driver::opus::Decoder;

/// One packet of the stream's sound: transport encryption off, then DAVE
/// (when the call uses it), then Opus to 48 kHz mono for our mix.
fn sound_packet(cipher: &Aes256Gcm, dave: Option<(&mut davey::DaveSession, u64)>, opus: &mut OpusDecoder, packet: &[u8]) -> Result<Vec<i16>, String> {
    let rtp = open_rtp(cipher, packet).ok_or("transport")?;
    let frame = match dave {
        Some((session, user)) => session.decrypt(user, davey::MediaType::AUDIO, &rtp.payload).map_err(|e| format!("DAVE {e:?}"))?,
        None => rtp.payload,
    };
    let mut pcm = vec![0i16; 5760]; // room for Opus's longest frame, 120 ms
    let n = opus.decode(&frame, &mut pcm, false).map_err(|e| format!("Opus {e:?}"))?;
    pcm.truncate(n);
    Ok(pcm)
}

/// What a video packet's RTP header says, once its transport encryption is off.
pub(crate) struct Rtp {
    pub seq: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// Opens Discord's `aead_aes256_gcm_rtpsize` transport encryption. The RTP
/// header and the 4-byte extension preamble are plain (and authenticated);
/// the extension body and the payload are encrypted; the packet ends with a
/// 16-byte tag and a 4-byte counter that starts the 12-byte nonce.
pub(crate) fn open_rtp(cipher: &Aes256Gcm, packet: &[u8]) -> Option<Rtp> {
    if packet.len() < 12 + 20 || packet[0] >> 6 != 2 {
        return None;
    }
    let csrcs = (packet[0] & 0x0f) as usize;
    let has_extension = packet[0] & 0x10 != 0;
    let padded = packet[0] & 0x20 != 0;
    let mut plain = 12 + csrcs * 4;
    let extension_words = if has_extension {
        let at = plain;
        plain += 4;
        u16::from_be_bytes([*packet.get(at + 2)?, *packet.get(at + 3)?]) as usize
    } else {
        0
    };
    let end = packet.len();
    if plain + 20 > end {
        return None;
    }
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&packet[end - 4..]);
    let tag = Tag::from_slice(&packet[end - 20..end - 4]);
    let mut body = packet[plain..end - 20].to_vec();
    cipher.decrypt_in_place_detached(Nonce::from_slice(&nonce), &packet[..plain], &mut body, tag).ok()?;
    let mut payload = body.get(extension_words * 4..)?.to_vec();
    if padded {
        let pad = *payload.last()? as usize;
        payload.truncate(payload.len().checked_sub(pad)?);
    }
    Some(Rtp {
        seq: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        marker: packet[1] & 0x80 != 0,
        payload,
    })
}

/// Seals an RTCP (compound) packet the way Discord expects: its first 8 bytes
/// plain and authenticated, the rest encrypted, then the tag and the counter.
pub(crate) fn seal_rtcp(cipher: &Aes256Gcm, rtcp: &[u8], counter: u32) -> Option<Vec<u8>> {
    let (header, rest) = rtcp.split_at(8);
    let mut body = rest.to_vec();
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    let tag = cipher.encrypt_in_place_detached(Nonce::from_slice(&nonce), header, &mut body).ok()?;
    let mut packet = header.to_vec();
    packet.extend_from_slice(&body);
    packet.extend_from_slice(&tag);
    packet.extend_from_slice(&counter.to_be_bytes());
    Some(packet)
}

/// RTCP picture loss indication (RFC 4585): "send a keyframe".
pub(crate) fn pli(ours: u32, media: u32) -> Vec<u8> {
    let mut p = vec![0x81, 206, 0, 2];
    p.extend_from_slice(&ours.to_be_bytes());
    p.extend_from_slice(&media.to_be_bytes());
    p
}

/// RTCP generic NACK (RFC 4585): "these packets never came, send them again".
/// Each entry names one lost sequence number and a bitmask of the 16 after it.
pub(crate) fn nack(ours: u32, media: u32, missing: &[u16]) -> Option<Vec<u8>> {
    let mut entries: Vec<(u16, u16)> = Vec::new();
    for &seq in missing {
        match entries.last_mut() {
            Some((pid, blp)) if seq.wrapping_sub(*pid).wrapping_sub(1) < 16 && seq != *pid => {
                *blp |= 1 << seq.wrapping_sub(*pid).wrapping_sub(1);
            }
            _ => entries.push((seq, 0)),
        }
    }
    if entries.is_empty() {
        return None;
    }
    let words = 2 + entries.len() as u16;
    let mut p = vec![0x81, 205];
    p.extend_from_slice(&words.to_be_bytes());
    p.extend_from_slice(&ours.to_be_bytes());
    p.extend_from_slice(&media.to_be_bytes());
    for (pid, blp) in entries {
        p.extend_from_slice(&pid.to_be_bytes());
        p.extend_from_slice(&blp.to_be_bytes());
    }
    Some(p)
}

/// A receiver report about the video plus a REMB (receiver estimated max
/// bitrate): what a real viewer keeps telling the server, so it doesn't
/// assume a bad connection and turn the quality down.
pub(crate) fn feedback(ours: u32, media: u32, highest_seq: u32, lost: u32, max_bitrate: u64) -> Vec<u8> {
    let mut p = vec![0x81, 201, 0, 7];
    p.extend_from_slice(&ours.to_be_bytes());
    p.extend_from_slice(&media.to_be_bytes());
    let lost = lost.min(0x7f_ffff);
    p.extend_from_slice(&[0, (lost >> 16) as u8, (lost >> 8) as u8, lost as u8]); // fraction lost 0, cumulative
    p.extend_from_slice(&highest_seq.to_be_bytes());
    p.extend_from_slice(&[0; 12]); // jitter, last SR, delay since last SR: unknown
    let mut exp = 0u32;
    while max_bitrate >> exp >= 1 << 18 {
        exp += 1;
    }
    let mantissa = (max_bitrate >> exp) as u32;
    p.extend_from_slice(&[0x8f, 206, 0, 5]);
    p.extend_from_slice(&ours.to_be_bytes());
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(b"REMB");
    p.extend_from_slice(&[1, ((exp << 2) | (mantissa >> 16)) as u8, (mantissa >> 8) as u8, mantissa as u8]);
    p.extend_from_slice(&media.to_be_bytes());
    p
}

/// What we want of the stream: everything, in its full size. Without a size the server
/// has only our silence to go on, as it shows us with 0 for our own stream that nobody displays.
fn sink_wants(video_ssrc: u32, pixels: u64) -> Value {
    if video_ssrc == 0 {
        return json!({ "any": 100 });
    }
    json!({ "any": 100, "pixelCounts": { (video_ssrc.to_string()): pixels } })
}

/// The picture size a stream announces, in pixels.
fn announced_pixels(media: &Value) -> u64 {
    let size = &media["streams"][0]["max_resolution"];
    match size["width"].as_u64().unwrap_or(0) * size["height"].as_u64().unwrap_or(0) {
        0 => 1920 * 1080,
        pixels => pixels,
    }
}

pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// What the assembler hands on: a whole frame, or word that one was lost.
pub(crate) enum Assembled {
    Frame(Vec<u8>),
    Lost,
}

/// One frame's packets as they come in.
struct Pending {
    parts: Vec<(u16, Vec<u8>)>,
    /// Sequence number of the packet with the marker bit: the frame's last.
    last: Option<u16>,
    since: std::time::Instant,
}

/// How long a frame may wait for late or retransmitted packets.
const FRAME_WAIT: Duration = Duration::from_millis(200);

/// Puts H.264 frames back together from RTP (RFC 6184: single NAL units,
/// STAP-A, FU-A), as Annex B with 4-byte start codes the way WebRTC's
/// receiver does: DAVE expects exactly that. Frames come out in order; one
/// that misses packets for too long is reported lost.
#[derive(Default)]
pub(crate) struct Assembler {
    /// Frames in flight, by RTP timestamp counted from the first one seen.
    pending: BTreeMap<u32, Pending>,
    base: Option<u32>,
    /// The newest frame already handed on or given up.
    done: Option<u32>,
    pub broken: u64,
}

impl Assembler {
    pub(crate) fn push(&mut self, rtp: Rtp, now: std::time::Instant) -> Vec<Assembled> {
        let base = *self.base.get_or_insert(rtp.timestamp);
        let key = rtp.timestamp.wrapping_sub(base);
        if self.done.is_some_and(|done| key <= done) {
            return Vec::new(); // a duplicate, or too late to help
        }
        let frame = self.pending.entry(key).or_insert_with(|| Pending { parts: Vec::new(), last: None, since: now });
        if !frame.parts.iter().any(|(seq, _)| *seq == rtp.seq) {
            frame.parts.push((rtp.seq, rtp.payload));
        }
        if rtp.marker {
            frame.last = Some(rtp.seq);
        }
        self.drain(now)
    }

    /// Hands on the oldest frames once complete, or once they've waited too long.
    pub(crate) fn drain(&mut self, now: std::time::Instant) -> Vec<Assembled> {
        let mut out = Vec::new();
        while let Some((&key, frame)) = self.pending.first_key_value() {
            if let Some(annex_b) = Self::complete(frame) {
                out.push(Assembled::Frame(annex_b));
            } else if now - frame.since > FRAME_WAIT || self.pending.len() > 15 {
                self.broken += 1;
                out.push(Assembled::Lost);
            } else {
                break;
            }
            self.pending.remove(&key);
            self.done = Some(key);
        }
        out
    }

    /// The frame as Annex B if every packet is there: from one that starts a
    /// NAL unit through the marker, with no gap.
    fn complete(frame: &Pending) -> Option<Vec<u8>> {
        let last = frame.last?;
        let mut parts: Vec<&(u16, Vec<u8>)> = frame.parts.iter().filter(|(seq, _)| last.wrapping_sub(*seq) < 0x8000).collect();
        parts.sort_by_key(|(seq, _)| std::cmp::Reverse(last.wrapping_sub(*seq)));
        let first = parts.first()?;
        if last.wrapping_sub(first.0) as usize + 1 != parts.len() {
            return None;
        }
        // A frame can't start in the middle of a fragmented NAL unit.
        if first.1.first().is_some_and(|h| h & 0x1f == 28) && first.1.get(1).is_some_and(|fu| fu & 0x80 == 0) {
            return None;
        }
        let mut annex_b = Vec::new();
        for (_, payload) in parts {
            let Some(&header) = payload.first() else { continue };
            match header & 0x1f {
                1..=23 => {
                    annex_b.extend_from_slice(&START_CODE);
                    annex_b.extend_from_slice(payload);
                }
                24 => {
                    let mut at = 1;
                    while at + 2 <= payload.len() {
                        let size = u16::from_be_bytes([payload[at], payload[at + 1]]) as usize;
                        let Some(nal) = payload.get(at + 2..at + 2 + size) else { break };
                        annex_b.extend_from_slice(&START_CODE);
                        annex_b.extend_from_slice(nal);
                        at += 2 + size;
                    }
                }
                28 if payload.len() > 2 => {
                    let fu = payload[1];
                    if fu & 0x80 != 0 {
                        annex_b.extend_from_slice(&START_CODE);
                        annex_b.push((header & 0xe0) | (fu & 0x1f));
                    }
                    annex_b.extend_from_slice(&payload[2..]);
                }
                _ => {}
            }
        }
        (!annex_b.is_empty()).then_some(annex_b)
    }
}

/// NAL unit types in an Annex B frame (headers stay readable under DAVE).
pub(crate) fn nal_types(frame: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 4 < frame.len() {
        if frame[i..i + 4] == START_CODE {
            types.push(frame[i + 4] & 0x1f);
            i += 4;
        } else {
            i += 1;
        }
    }
    types
}

/// Video counters, reported every few seconds.
#[derive(Default)]
struct VideoStats {
    packets: u64,
    unopened: u64,
    nacked: u64,
    retransmitted: u64,
    frames: u64,
    keyframes: u64,
    decrypted: u64,
    dave_failed: u64,
    first_error: Option<String>,
    first_frame: Option<String>,
}

/// A JSON payload, compacted and cut for the log.
pub(crate) fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 400 { format!("{}…", &s[..s.char_indices().nth(400).map(|(i, _)| i).unwrap_or(s.len())]) } else { s }
}

/// A starting point for our nonce counter that differs between connections.
pub(crate) fn rand_counter() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1)
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Discord's IP discovery: the server tells us our public address and port.
pub(crate) async fn discover(udp: &UdpSocket, ssrc: u32) -> Result<(String, u16), String> {
    let mut packet = [0u8; 74];
    packet[0..2].copy_from_slice(&1u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    udp.send(&packet).await.map_err(|e| e.to_string())?;
    let mut reply = [0u8; 74];
    let len = tokio::time::timeout(Duration::from_secs(3), udp.recv(&mut reply))
        .await
        .map_err(|_| "IP discovery timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if len < 74 || u16::from_be_bytes([reply[0], reply[1]]) != 2 {
        return Err("bad IP discovery reply".into());
    }
    let address = &reply[8..72];
    let end = address.iter().position(|&b| b == 0).unwrap_or(address.len());
    let ip = String::from_utf8_lossy(&address[..end]).to_string();
    Ok((ip, u16::from_be_bytes([reply[72], reply[73]])))
}

/// Everything the media connection keeps between messages.
struct State {
    link: Link,
    seq_ack: i64,
    udp: Option<Arc<UdpSocket>>,
    dave: Option<davey::DaveSession>,
    dave_version: u16,
    pending: HashMap<u16, u16>,
    users: HashSet<u64>,
    /// Ops already logged once in full (the rest just count).
    seen: HashSet<u64>,
    /// Transport encryption, from the session description.
    cipher: Option<Aes256Gcm>,
    assembler: Assembler,
    video: VideoStats,
    decoder: crate::h264dec::Decoder,
    /// The stream's full size, as it announced it.
    wanted_pixels: u64,
    /// Our SSRC (from READY) and the streamer's video SSRC (from op 12), for keyframe requests.
    our_ssrc: u32,
    video_ssrc: u32,
    rtcp_counter: u32,
    /// When we last asked for a keyframe, so a burst of losses asks once.
    last_keyframe_ask: Option<std::time::Instant>,
    /// Highest video sequence number seen, extended past wraps, for receiver reports.
    highest_seq: Option<u32>,
    opus: Option<OpusDecoder>,
    sound: SoundStats,
}

/// Counters for the stream's sound.
#[derive(Default)]
struct SoundStats {
    packets: u64,
    played: u64,
    failed: u64,
    first_error: Option<String>,
}

impl State {
    fn send_rtcp(&mut self, rtcp: &[u8]) -> bool {
        let (Some(cipher), Some(udp)) = (&self.cipher, &self.udp) else { return false };
        self.rtcp_counter = self.rtcp_counter.wrapping_add(1);
        seal_rtcp(cipher, rtcp, self.rtcp_counter).is_some_and(|packet| udp.try_send(&packet).is_ok())
    }

    /// Asks the streamer for a keyframe: without one, nothing can be decoded.
    fn request_keyframe(&mut self) {
        if self.video_ssrc == 0 || self.last_keyframe_ask.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        // Before the connection is up nothing can be sent: that doesn't count as asked.
        let sent = self.send_rtcp(&pli(self.our_ssrc, self.video_ssrc));
        if sent {
            self.last_keyframe_ask = Some(std::time::Instant::now());
        }
        crate::log_line(&format!("go live: asked for a keyframe (sent {sent})"));
    }

    /// Tells the server we receive well and could take a lot more.
    fn send_feedback(&mut self) {
        if self.video_ssrc == 0 {
            return;
        }
        let rtcp = feedback(self.our_ssrc, self.video_ssrc, self.highest_seq.unwrap_or(0), self.assembler.broken as u32, 20_000_000);
        self.send_rtcp(&rtcp);
    }

    /// A packet jumped ahead of the last one: ask at once for the ones skipped,
    /// so their retransmissions (RTX) arrive while the frame still waits.
    fn ask_for_gap(&mut self, seq: u16) {
        let Some(high) = self.highest_seq else { return };
        let skipped = seq.wrapping_sub(high as u16).wrapping_sub(1);
        if skipped == 0 || skipped > 200 {
            return; // in order, a reordered late one, or a jump too big to repair
        }
        let missing: Vec<u16> = (1..=skipped).map(|i| (high as u16).wrapping_add(i)).collect();
        if let Some(rtcp) = nack(self.our_ssrc, self.video_ssrc, &missing) {
            if self.send_rtcp(&rtcp) {
                self.video.nacked += missing.len() as u64;
            }
        }
    }

    fn note_seq(&mut self, seq: u16) {
        let extended = match self.highest_seq {
            None => seq as u32,
            Some(high) => {
                let delta = seq.wrapping_sub(high as u16) as i16;
                if delta > 0 { high.wrapping_add(delta as u32) } else { high }
            }
        };
        self.highest_seq = Some(extended);
    }

    /// A UDP packet from the media server: H.264 video (or its retransmission)
    /// goes through transport decryption, frame assembly, DAVE and decoding.
    fn on_udp(&mut self, packet: &[u8]) {
        let pt = if packet.len() >= 12 { packet[1] & 0x7f } else { 0 };
        if pt == OPUS_PT {
            self.on_sound(packet);
            return;
        }
        if pt != H264_PT && pt != RTX_PT {
            return;
        }
        let Some(cipher) = &self.cipher else { return };
        self.video.packets += 1;
        let Some(mut rtp) = open_rtp(cipher, packet) else {
            self.video.unopened += 1;
            return;
        };
        if pt == RTX_PT {
            // RFC 4588: the original sequence number, then the original payload.
            // Empty ones are bandwidth probes.
            if rtp.payload.len() <= 2 {
                return;
            }
            rtp.seq = u16::from_be_bytes([rtp.payload[0], rtp.payload[1]]);
            rtp.payload.drain(..2);
            self.video.retransmitted += 1;
        } else {
            self.ask_for_gap(rtp.seq);
            self.note_seq(rtp.seq);
            if rtp.payload.is_empty() {
                return;
            }
        }
        for assembled in self.assembler.push(rtp, std::time::Instant::now()) {
            match assembled {
                Assembled::Frame(frame) => self.on_frame(frame),
                Assembled::Lost => {
                    // Frames after a lost one would decode into smears: wait for a keyframe.
                    self.decoder.lost();
                    self.request_keyframe();
                }
            }
        }
    }

    /// The stream's sound (game, video…): decoded and played with the call.
    fn on_sound(&mut self, packet: &[u8]) {
        let Some(cipher) = &self.cipher else { return };
        self.sound.packets += 1;
        if self.opus.is_none() {
            match OpusDecoder::new(crate::audio::SAMPLE_RATE, songbird::driver::opus::Channels::Mono) {
                Ok(decoder) => self.opus = Some(decoder),
                Err(e) => {
                    self.sound.failed += 1;
                    if self.sound.first_error.is_none() {
                        crate::log_line(&format!("go live: no Opus decoder: {e:?}"));
                        self.sound.first_error = Some(format!("{e:?}"));
                    }
                    return;
                }
            }
        }
        let Some(opus) = self.opus.as_mut() else { return };
        let streamer = self.link.streamer;
        let dave = match (&mut self.dave, self.dave_version) {
            (Some(session), v) if v > 0 => Some((session, streamer)),
            _ => None,
        };
        match sound_packet(cipher, dave, opus, packet) {
            Ok(mut pcm) => {
                self.sound.played += 1;
                // The slider of the stream view; at 0 nothing is played.
                let silent = self.link.volume_key.strip_prefix("stream:").is_some_and(stream_sound_off);
                if !silent && crate::audio::apply_user_volume(&self.link.volume_key, &mut pcm) {
                    self.link.sound.push(&self.link.volume_key, &pcm);
                }
            }
            Err(e) => {
                self.sound.failed += 1;
                if self.sound.first_error.is_none() {
                    crate::log_line(&format!("go live: stream sound failed: {e}"));
                    self.sound.first_error = Some(e);
                }
            }
        }
    }

    fn on_frame(&mut self, frame: Vec<u8>) {
        self.video.frames += 1;
        let types = nal_types(&frame);
        let result = match (&mut self.dave, self.dave_version) {
            (Some(session), v) if v > 0 => session.decrypt(self.link.streamer, davey::MediaType::VIDEO, &frame).map_err(|e| format!("{e:?}")),
            _ => Ok(frame.clone()),
        };
        match result {
            Ok(plain) => {
                self.video.decrypted += 1;
                // Only one that could be opened counts: a keyframe that DAVE refused helps nobody.
                if types.contains(&5) {
                    self.video.keyframes += 1;
                }
                if self.video.first_frame.is_none() {
                    let text = format!("{} bytes encrypted -> {} plain, NAL types {:?} -> {:?}", frame.len(), plain.len(), types, nal_types(&plain));
                    crate::log_line(&format!("go live: first frame: {text}"));
                    self.video.first_frame = Some(text);
                }
                self.decoder.push(plain, types.contains(&5));
            }
            Err(e) => {
                self.video.dave_failed += 1;
                if self.video.first_error.is_none() {
                    crate::log_line(&format!("go live: DAVE decrypt failed: {e} (frame {} bytes, NAL types {types:?})", frame.len()));
                    self.video.first_error = Some(e);
                }
            }
        }
    }

    fn video_report(&self) -> String {
        let v = &self.video;
        format!(
            "video: {} packets ({} unopened, {} asked again, {} retransmitted), {} frames ({} keyframes, {} lost), DAVE ok {} failed {}, decoder dropped {}; sound: {} packets, {} played, {} failed",
            v.packets,
            v.unopened,
            v.nacked,
            v.retransmitted,
            v.frames,
            v.keyframes,
            self.assembler.broken,
            v.decrypted,
            v.dave_failed,
            self.decoder.dropped(),
            self.sound.packets,
            self.sound.played,
            self.sound.failed
        )
    }

    /// DAVE's channel id for a stream is its RTC server id minus one.
    fn dave_channel(&self) -> u64 {
        self.link.server_id.parse::<u64>().unwrap_or(1).saturating_sub(1)
    }

    fn me(&self) -> u64 {
        self.link.me.parse().unwrap_or(0)
    }

    /// Starts (or restarts) our DAVE membership and returns the key package to send.
    fn dave_reinit(&mut self) -> Option<Vec<u8>> {
        let version = NonZeroU16::new(self.dave_version)?;
        let (me, channel) = (self.me(), self.dave_channel());
        let result = match &mut self.dave {
            Some(session) => session.reinit(version, me, channel, None).map_err(|e| format!("{e:?}")).and_then(|_| session.create_key_package().map_err(|e| format!("{e:?}"))),
            None => davey::DaveSession::new(version, me, channel, None).map_err(|e| format!("{e:?}")).and_then(|mut session| {
                let package = session.create_key_package().map_err(|e| format!("{e:?}"));
                self.dave = Some(session);
                package
            }),
        };
        match result {
            Ok(package) => Some(package),
            Err(e) => {
                crate::log_line(&format!("go live: DAVE init failed: {e}"));
                None
            }
        }
    }
}

type Outgoing = Vec<Message>;

pub(crate) fn json_msg(op: u8, d: Value) -> Message {
    Message::Text(json!({ "op": op, "d": d }).to_string())
}

pub(crate) fn binary_msg(op: u8, payload: &[u8]) -> Message {
    let mut data = Vec::with_capacity(payload.len() + 1);
    data.push(op);
    data.extend_from_slice(payload);
    Message::Binary(data)
}

async fn run(link: Link, status: Status) -> Result<(), String> {
    let endpoint = link.endpoint.trim_start_matches("wss://").trim_end_matches('/').to_string();
    let (ws, _) = connect_async(format!("wss://{endpoint}/?v=8")).await.map_err(|e| format!("media server: {e}"))?;
    let (mut tx, mut rx) = ws.split();
    let identify = json!({
        "server_id": link.server_id,
        "user_id": link.me,
        "session_id": link.session_id,
        "token": link.token,
        "video": true,
        "streams": [{ "type": "screen", "rid": "100", "quality": 100 }],
        "max_dave_protocol_version": 1,
    });
    tx.send(json_msg(0, identify)).await.map_err(|e| e.to_string())?;
    crate::log_line(&format!("go live: media server {endpoint}, identify sent"));

    let display = link.display.clone();
    let mut state = State {
        decoder: crate::h264dec::Decoder::start(Box::new(move |pixels| display(pixels))),
        wanted_pixels: 0,
        link,
        seq_ack: -1,
        udp: None,
        dave: None,
        dave_version: 0,
        pending: HashMap::new(),
        users: HashSet::new(),
        seen: HashSet::new(),
        cipher: None,
        assembler: Assembler::default(),
        video: VideoStats::default(),
        our_ssrc: 0,
        video_ssrc: 0,
        rtcp_counter: rand_counter(),
        last_keyframe_ask: None,
        highest_seq: None,
        opus: None,
        sound: SoundStats::default(),
    };
    let mut feedback_tick = tokio::time::interval(Duration::from_secs(1));
    // Until a first keyframe arrives, it is asked for again and again: nothing can be shown without it.
    let mut keyframe_tick = tokio::time::interval(Duration::from_millis(300));
    let mut heartbeat: Option<tokio::time::Interval> = None;
    let mut report = tokio::time::interval(Duration::from_secs(3));
    let (udp_tx, mut udp_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // Packets per (ssrc, payload type) since the last report.
    let mut counts: HashMap<(u32, u8), u64> = HashMap::new();
    let mut video_bytes = 0u64;

    loop {
        let mut out: Outgoing = Vec::new();
        tokio::select! {
            msg = rx.next() => {
                let msg = msg.ok_or("media server closed the connection")?.map_err(|e| e.to_string())?;
                match msg {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                        if let Some(seq) = payload["seq"].as_i64() {
                            state.seq_ack = seq;
                        }
                        on_json(&mut state, &payload, &mut out, &mut heartbeat, &udp_tx, &status).await?;
                    }
                    Message::Binary(data) => {
                        if data.len() >= 3 {
                            state.seq_ack = u16::from_be_bytes([data[0], data[1]]) as i64;
                            on_binary(&mut state, data[2], &data[3..], &mut out);
                        }
                    }
                    Message::Close(frame) => return Err(format!("media server closed: {frame:?}")),
                    _ => {}
                }
            }
            _ = async { match &mut heartbeat { Some(h) => { h.tick().await; } None => std::future::pending().await } } => {
                out.push(json_msg(3, json!({ "t": now_ms(), "seq_ack": state.seq_ack })));
            }
            Some(packet) = udp_rx.recv() => {
                state.on_udp(&packet);
                if packet.len() >= 12 {
                    let pt = packet[1] & 0x7f;
                    if pt == H264_PT || pt == RTX_PT {
                        video_bytes += packet.len() as u64;
                    }
                    let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
                    // RTCP shares the socket; its "payload types" land in 72..=76 after masking.
                    let key = if (72..=76).contains(&pt) { (0, pt) } else { (ssrc, pt) };
                    *counts.entry(key).or_default() += 1;
                }
            }
            _ = keyframe_tick.tick(), if state.udp.is_some() && state.video.keyframes == 0 => state.request_keyframe(),
            _ = feedback_tick.tick() => {
                state.send_feedback();
                // Frames waiting on packets that never come still have to give way.
                for assembled in state.assembler.drain(std::time::Instant::now()) {
                    match assembled {
                        Assembled::Frame(frame) => state.on_frame(frame),
                        Assembled::Lost => {
                            state.decoder.lost();
                            state.request_keyframe();
                        }
                    }
                }
            }
            _ = report.tick() => {
                if !counts.is_empty() {
                    let mut parts: Vec<String> = counts.iter().map(|((ssrc, pt), n)| format!("ssrc {ssrc} pt {pt}: {n}")).collect();
                    parts.sort();
                    crate::log_line(&format!("go live: udp in 3 s: {}; video {} kbps", parts.join(", "), video_bytes * 8 / 3000));
                    video_bytes = 0;
                    crate::log_line(&format!("go live: {}", state.video_report()));
                    counts.clear();
                }
                if state.udp.is_some() && state.video.keyframes == 0 {
                    state.request_keyframe();
                }
                if state.udp.is_some() {
                    let v = &state.video;
                    status(if v.keyframes == 0 {
                        crate::i18n::t("жду ключевой кадр…")
                    } else {
                        crate::i18n::tf("кадров {}, потеряно {}", &[&v.decrypted, &(state.assembler.broken + v.dave_failed)])
                    });
                }
            }
        }
        for msg in out {
            tx.send(msg).await.map_err(|e| e.to_string())?;
        }
    }
}

async fn on_json(
    state: &mut State,
    payload: &Value,
    out: &mut Outgoing,
    heartbeat: &mut Option<tokio::time::Interval>,
    udp_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    status: &Status,
) -> Result<(), String> {
    let op = payload["op"].as_u64().unwrap_or(u64::MAX);
    let d = &payload["d"];
    match op {
        // Hello: start heartbeats.
        8 => {
            let ms = d["heartbeat_interval"].as_f64().unwrap_or(13750.0) as u64;
            let mut interval = tokio::time::interval(Duration::from_millis(ms.max(1000)));
            interval.tick().await;
            *heartbeat = Some(interval);
        }
        // Ready: our SSRC and where the media server listens.
        2 => {
            crate::log_line(&format!("go live: READY {}", short(d)));
            let (ip, port, ssrc) = (d["ip"].as_str().unwrap_or(""), d["port"].as_u64().unwrap_or(0) as u16, d["ssrc"].as_u64().unwrap_or(0) as u32);
            state.our_ssrc = ssrc;
            let modes: Vec<&str> = d["modes"].as_array().map(|m| m.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            let mode = ["aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"].into_iter().find(|m| modes.contains(m)).ok_or("no supported encryption mode")?;
            let udp = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
            udp.connect((ip, port)).await.map_err(|e| e.to_string())?;
            let (address, our_port) = discover(&udp, ssrc).await?;
            let udp = Arc::new(udp);
            let reader = udp.clone();
            let forward = udp_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                while let Ok(n) = reader.recv(&mut buf).await {
                    if forward.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            state.udp = Some(udp);
            // We only watch: H.264 is what the Nimbus decoder will take, offered first.
            let codecs = json!([
                { "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 },
                { "name": "H264", "type": "video", "priority": 1000, "payload_type": 101, "rtx_payload_type": 102, "encode": false, "decode": true },
            ]);
            out.push(json_msg(1, json!({ "protocol": "udp", "data": { "address": address, "port": our_port, "mode": mode }, "codecs": codecs })));
            crate::log_line(&format!("go live: select protocol udp {mode}"));
        }
        // Session description: the agreed codecs, the transport key, DAVE on or off.
        4 => {
            let mut shown = d.clone();
            if let Some(key) = shown.get_mut("secret_key") {
                *key = json!(format!("<{} bytes>", key.as_array().map(|a| a.len()).unwrap_or(0)));
            }
            crate::log_line(&format!("go live: SESSION_DESCRIPTION {}", short(&shown)));
            let key: Vec<u8> = d["secret_key"].as_array().map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default();
            state.cipher = match d["mode"].as_str() {
                Some("aead_aes256_gcm_rtpsize") => Aes256Gcm::new_from_slice(&key).ok(),
                other => {
                    crate::log_line(&format!("go live: can't open transport mode {other:?}"));
                    None
                }
            };
            state.dave_version = d["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
            if let Some(package) = state.dave_reinit() {
                out.push(binary_msg(26, &package));
            }
            // Ask for video: without this the server may send none.
            out.push(json_msg(15, sink_wants(state.video_ssrc, state.wanted_pixels)));
            status(crate::i18n::tf("подключено, видео {}", &[&d["video_codec"].as_str().unwrap_or("?")]));
        }
        6 => {} // heartbeat ack
        // The streamer's media: remember the video SSRC and ask for a keyframe at once.
        12 => {
            if state.seen.insert(op) {
                crate::log_line(&format!("go live: op 12 {}", short(d)));
            }
            if let Some(ssrc) = d["video_ssrc"].as_u64().filter(|&s| s != 0) {
                state.video_ssrc = ssrc as u32;
                state.wanted_pixels = announced_pixels(d);
                out.push(json_msg(15, sink_wants(state.video_ssrc, state.wanted_pixels)));
                state.request_keyframe();
            }
        }
        11 => {
            for id in d["user_ids"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str().and_then(|s| s.parse().ok()) {
                    state.users.insert(id);
                }
            }
        }
        13 => {
            if let Some(id) = d["user_id"].as_str().and_then(|s| s.parse().ok()) {
                state.users.remove(&id);
            }
        }
        // DAVE transitions, as in Songbird's voice connection.
        21 => {
            let (id, version) = (d["transition_id"].as_u64().unwrap_or(0) as u16, d["protocol_version"].as_u64().unwrap_or(0) as u16);
            state.pending.insert(id, version);
            if id == 0 {
                execute_transition(state, 0);
            } else {
                if version == 0 {
                    if let Some(session) = &mut state.dave {
                        session.set_passthrough_mode(true, Some(120));
                    }
                }
                out.push(json_msg(23, json!({ "transition_id": id })));
            }
        }
        22 => execute_transition(state, d["transition_id"].as_u64().unwrap_or(0) as u16),
        24 => {
            if d["epoch"].as_u64() == Some(1) {
                state.dave_version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if let Some(package) = state.dave_reinit() {
                    out.push(binary_msg(26, &package));
                }
            }
        }
        _ => {
            // Video (12), flags (18), platform (20), sink wants (15)…: log each kind once.
            if state.seen.insert(op) {
                crate::log_line(&format!("go live: op {op} {}", short(d)));
            }
        }
    }
    Ok(())
}

fn execute_transition(state: &mut State, id: u16) {
    let Some(version) = state.pending.remove(&id) else { return };
    let old = state.dave_version;
    state.dave_version = version;
    if id > 0 && old == 0 && version != 0 {
        if let Some(session) = &mut state.dave {
            session.set_passthrough_mode(true, Some(10));
        }
    }
    crate::log_line(&format!("go live: DAVE transition {id} -> version {version}"));
}

fn on_binary(state: &mut State, op: u8, payload: &[u8], out: &mut Outgoing) {
    match op {
        25 => {
            if let Some(session) = &mut state.dave {
                if let Err(e) = session.set_external_sender(payload) {
                    crate::log_line(&format!("go live: DAVE external sender: {e:?}"));
                }
            }
        }
        27 if !payload.is_empty() => {
            let kind = if payload[0] == 0 { davey::ProposalsOperationType::APPEND } else { davey::ProposalsOperationType::REVOKE };
            let users: Vec<u64> = state.users.iter().copied().collect();
            if let Some(session) = &mut state.dave {
                match session.process_proposals(kind, &payload[1..], Some(&users)) {
                    Ok(Some(cw)) => {
                        let mut data = cw.commit;
                        if let Some(welcome) = cw.welcome {
                            data.extend_from_slice(&welcome);
                        }
                        out.push(binary_msg(28, &data));
                    }
                    Ok(None) => {}
                    Err(e) => crate::log_line(&format!("go live: DAVE proposals: {e:?}")),
                }
            }
        }
        29 | 30 if payload.len() >= 2 => {
            let id = u16::from_be_bytes([payload[0], payload[1]]);
            let Some(session) = &mut state.dave else { return };
            let result = if op == 29 {
                session.process_commit(&payload[2..]).map_err(|e| format!("{e:?}"))
            } else {
                session.process_welcome(&payload[2..]).map_err(|e| format!("{e:?}"))
            };
            match result {
                Ok(()) => {
                    crate::log_line(&format!("go live: DAVE {} ok (transition {id})", if op == 29 { "commit" } else { "welcome" }));
                    if id != 0 {
                        state.pending.insert(id, state.dave_version);
                        out.push(json_msg(23, json!({ "transition_id": id })));
                    }
                }
                Err(e) => {
                    crate::log_line(&format!("go live: DAVE {op} failed: {e}"));
                    out.push(json_msg(31, json!({ "transition_id": id })));
                    if let Some(package) = state.dave_reinit() {
                        out.push(binary_msg(26, &package));
                    }
                }
            }
        }
        _ => {
            if state.seen.insert(1000 + op as u64) {
                crate::log_line(&format!("go live: binary op {op}, {} bytes", payload.len()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a packet the way Discord does: plain header (+ extension
    /// preamble), encrypted extension body and payload, tag, 4-byte nonce.
    fn seal(cipher: &Aes256Gcm, seq: u16, ts: u32, marker: bool, payload: &[u8], counter: u32) -> Vec<u8> {
        seal_with(cipher, H264_PT | if marker { 0x80 } else { 0 }, seq, ts, payload, counter)
    }

    /// Any payload type (e.g. the stream's Opus sound), no marker.
    pub(super) fn seal_for(cipher: &Aes256Gcm, pt: u8, seq: u16, ts: u32, payload: &[u8], counter: u32) -> Vec<u8> {
        seal_with(cipher, pt, seq, ts, payload, counter)
    }

    fn seal_with(cipher: &Aes256Gcm, second_byte: u8, seq: u16, ts: u32, payload: &[u8], counter: u32) -> Vec<u8> {
        let mut header = vec![0x90, second_byte];
        header.extend_from_slice(&seq.to_be_bytes());
        header.extend_from_slice(&ts.to_be_bytes());
        header.extend_from_slice(&17248u32.to_be_bytes());
        header.extend_from_slice(&[0xbe, 0xde, 0x00, 0x01]); // one-byte extensions, 1 word
        let mut body = vec![0x10, 0xaa, 0x00, 0x00]; // the extension word
        body.extend_from_slice(payload);
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&counter.to_be_bytes());
        let tag = cipher.encrypt_in_place_detached(Nonce::from_slice(&nonce), &header, &mut body).unwrap();
        let mut packet = header;
        packet.extend_from_slice(&body);
        packet.extend_from_slice(&tag);
        packet.extend_from_slice(&counter.to_be_bytes());
        packet
    }

    fn frame_packets(cipher: &Aes256Gcm) -> (Vec<Vec<u8>>, Vec<u8>) {
        // SPS + PPS in a STAP-A, then an IDR slice split over three FU-A packets.
        let sps = [0x67, 1, 2, 3];
        let pps = [0x68, 4, 5];
        let mut stap = vec![24];
        for nal in [&sps[..], &pps[..]] {
            stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            stap.extend_from_slice(nal);
        }
        let idr: Vec<u8> = std::iter::once(0x65).chain(1..=30u8).collect();
        let fu = |start: bool, end: bool, data: &[u8]| {
            let mut p = vec![(0x65 & 0xe0) | 28, (if start { 0x80 } else { 0 }) | (if end { 0x40 } else { 0 }) | 5];
            p.extend_from_slice(data);
            p
        };
        let packets = vec![
            seal(cipher, 65534, 9000, false, &stap, 1),
            seal(cipher, 65535, 9000, false, &fu(true, false, &idr[1..11]), 2),
            seal(cipher, 0, 9000, false, &fu(false, false, &idr[11..21]), 3),
            seal(cipher, 1, 9000, true, &fu(false, true, &idr[21..]), 4),
        ];
        let mut want = Vec::new();
        for nal in [&sps[..], &pps[..], &idr[..]] {
            want.extend_from_slice(&START_CODE);
            want.extend_from_slice(nal);
        }
        (packets, want)
    }

    fn frames(out: Vec<Assembled>) -> Vec<Option<Vec<u8>>> {
        out.into_iter().map(|a| if let Assembled::Frame(f) = a { Some(f) } else { None }).collect()
    }

    #[test]
    fn opens_and_assembles_with_a_late_packet() {
        let cipher = Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap();
        let (packets, want) = frame_packets(&cipher);
        let now = std::time::Instant::now();
        let mut assembler = Assembler::default();
        let mut got = Vec::new();
        // The marker arrives before the third packet; the frame waits for it.
        for i in [0, 1, 3, 2] {
            got.extend(frames(assembler.push(open_rtp(&cipher, &packets[i]).expect("opens"), now)));
        }
        assert_eq!(got, vec![Some(want.clone())]);
        assert_eq!(nal_types(&want), vec![7, 8, 5]);
        assert_eq!(assembler.broken, 0);
    }

    #[test]
    fn reports_a_frame_lost_for_good() {
        let cipher = Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap();
        let (packets, _) = frame_packets(&cipher);
        let now = std::time::Instant::now();
        let mut assembler = Assembler::default();
        for i in [0, 1, 3] {
            assert!(assembler.push(open_rtp(&cipher, &packets[i]).unwrap(), now).is_empty());
        }
        let later = assembler.drain(now + FRAME_WAIT + Duration::from_millis(1));
        assert!(matches!(later.as_slice(), [Assembled::Lost]));
        // The missing packet arriving afterwards changes nothing.
        assert!(assembler.push(open_rtp(&cipher, &packets[2]).unwrap(), now).is_empty());
    }

    #[test]
    fn feedback_encodes_remb() {
        let p = feedback(1, 2, 3, 0, 20_000_000);
        assert_eq!(p.len(), 32 + 24);
        let remb = &p[32..];
        assert_eq!(&remb[12..16], b"REMB");
        let exp = (remb[17] >> 2) as u32;
        let mantissa = (((remb[17] & 3) as u32) << 16) | ((remb[18] as u32) << 8) | remb[19] as u32;
        assert_eq!((mantissa as u64) << exp, 20_000_000 >> exp << exp);
    }
}

#[cfg(test)]
mod sink_tests {
    use super::*;

    #[test]
    fn the_server_hears_how_big_we_want_the_stream() {
        assert_eq!(sink_wants(0, 0), json!({ "any": 100 }));
        assert_eq!(sink_wants(2969, 921_600), json!({ "any": 100, "pixelCounts": { "2969": 921_600 } }));
        assert_eq!(announced_pixels(&json!({ "streams": [{ "max_resolution": { "width": 1280, "height": 720 } }] })), 921_600);
        assert_eq!(announced_pixels(&json!({ "streams": [] })), 1920 * 1080);
    }
}

#[cfg(test)]
mod nack_tests {
    use super::*;

    #[test]
    fn nack_packs_neighbours_into_one_entry() {
        // 65535 and 0..=2 lost: one entry for 65535 with the next three in its bitmask, plus 40 alone.
        let p = nack(1, 2, &[65535, 0, 1, 2, 40]).unwrap();
        assert_eq!(&p[..4], &[0x81, 205, 0, 4]);
        assert_eq!(&p[12..16], &[0xff, 0xff, 0b0000_0000, 0b0000_0111]);
        assert_eq!(&p[16..20], &[0, 40, 0, 0]);
    }
}

#[cfg(test)]
mod sound_tests {
    use super::*;
    use songbird::driver::opus::{Application, Channels, Encoder};

    #[test]
    fn stream_sound_opens_and_decodes() {
        let cipher = Aes256Gcm::new_from_slice(&[9u8; 32]).unwrap();
        // 20 ms of a loud 440 Hz tone, Opus-encoded like a streamer's sound.
        let tone: Vec<i16> = (0..960).map(|i| (12000.0 * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin()) as i16).collect();
        let mut encoder = Encoder::new(48000, Channels::Mono, Application::Audio).unwrap();
        let mut decoder = OpusDecoder::new(48000, Channels::Mono).unwrap();
        let mut peak = 0;
        // A few frames: Opus needs one or two to settle.
        for n in 0..5u16 {
            let mut opus = vec![0u8; 1500];
            let len = encoder.encode(&tone, &mut opus).unwrap();
            opus.truncate(len);
            let packet = tests::seal_for(&cipher, OPUS_PT, n, n as u32 * 960, &opus, 100 + n as u32);
            let pcm = sound_packet(&cipher, None, &mut decoder, &packet).expect("decodes");
            assert_eq!(pcm.len(), 960);
            peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        }
        assert!(peak > 6000, "tone came out too quiet: {peak}");
    }
}
