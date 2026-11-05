//! Showing our screen on Discord's own servers (Go Live), the sending twin of golive.rs.
//!
//! Ask the main gateway for a stream (opcode 18), wait for STREAM_CREATE and
//! STREAM_SERVER_UPDATE, then connect to the stream's media server as the
//! streamer: voice gateway v8, UDP with the same transport encryption the
//! viewer opens, DAVE end-to-end encryption of every video and sound frame.
//! The H.264 frames come from the GPU encoder (hwenc.rs) through a sink, the
//! sound from the system loopback (audio.rs); the server's REMB sets the
//! encoder's bitrate and picture-loss requests make a keyframe.
//!
//! Like the rest of the Discord side this uses the user's account from an
//! unofficial client; Discord may act on that. The pure parts (packets,
//! headers, messages, RTCP) are tested; the conversation with the real server
//! can only be checked by going live.

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::golive::{binary_msg, discover, json_msg, now_ms, rand_counter, seal_rtcp, short, H264_PT, OPUS_PT, RTX_PT, START_CODE};

/// Says how the stream is doing, for the status line.
pub type Status = Arc<dyn Fn(String) + Send + Sync>;

/// Where an encoder's frames go next to LiveKit: called with the Annex B
/// access unit, whether it is a keyframe and its time in µs; false removes it.
pub type FrameSink = Box<dyn Fn(&[u8], bool, i64) -> bool + Send>;

/// Where the system sound's 10 ms stereo chunks (48 kHz, i16) go.
pub type SoundTap = UnboundedSender<Vec<i16>>;

/// The account's Nitro kind (0 none), set when the account is read, for the stream's limits.
static PREMIUM: AtomicU64 = AtomicU64::new(0);

pub fn set_premium(premium_type: u64) {
    PREMIUM.store(premium_type, Ordering::Relaxed);
}

pub fn premium() -> u64 {
    PREMIUM.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- choices

/// Where a share goes (the picker's "где показывать").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareTo {
    /// The Nimbus (LiveKit) room only.
    Room,
    /// Discord's Go Live only.
    Discord,
    /// Both, from one encoder.
    Both,
}

impl ShareTo {
    pub const DEFAULT: ShareTo = ShareTo::Discord;

    pub fn from_index(i: i64) -> Option<Self> {
        match i {
            0 => Some(Self::Room),
            1 => Some(Self::Discord),
            2 => Some(Self::Both),
            _ => None,
        }
    }

    #[allow(dead_code)] // for a settings page that shows the choice by number
    pub fn index(self) -> i32 {
        match self {
            Self::Room => 0,
            Self::Discord => 1,
            Self::Both => 2,
        }
    }

    pub fn room(self) -> bool {
        self != Self::Discord
    }

    pub fn discord(self) -> bool {
        self != Self::Room
    }
}

/// What a Go Live stream may be, by Nitro kind: (height, frame rate). A height of 0 is
/// "as the source", which only the top kind may have. Discord changes these now and then.
pub fn stream_cap(premium_type: u64) -> (u32, u32) {
    match premium_type {
        2 => (0, 60),      // Nitro: up to the source (4K), 60
        1 => (1080, 60),   // Nitro Classic
        _ => (720, 30),    // no Nitro, Nitro Basic
    }
}

/// The picker's height and rate cut to what the account may stream.
pub fn cap_quality(height: u32, fps: u32, premium_type: u64) -> (u32, u32) {
    let (cap_height, cap_fps) = stream_cap(premium_type);
    let height = match (height, cap_height) {
        (h, 0) => h,
        (0, cap) => cap,
        (h, cap) => h.min(cap),
    };
    (height, fps.min(cap_fps))
}

// ---------------------------------------------------------------- gateway messages

/// Which call a stream belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Guild { guild: String, channel: String },
    /// A DM or group call (not wired to the UI yet).
    #[allow(dead_code)] // a DM or group call: not offered yet
    Call { channel: String },
}

impl Place {
    /// The stream key Discord uses for our stream.
    pub fn stream_key(&self, me: &str) -> String {
        match self {
            Place::Guild { guild, channel } => format!("guild:{guild}:{channel}:{me}"),
            Place::Call { channel } => format!("call:{channel}:{me}"),
        }
    }
}

/// Main gateway opcode 18, STREAM_CREATE: "I want to go live here".
pub fn stream_create(place: &Place) -> Value {
    let d = match place {
        Place::Guild { guild, channel } => json!({ "type": "guild", "guild_id": guild, "channel_id": channel, "preferred_region": null }),
        Place::Call { channel } => json!({ "type": "call", "channel_id": channel, "preferred_region": null }),
    };
    json!({ "op": 18, "d": d })
}

/// Opcode 19, STREAM_DELETE: stop.
pub fn stream_delete(key: &str) -> Value {
    json!({ "op": 19, "d": { "stream_key": key } })
}

/// Opcode 22, STREAM_SET_PAUSED.
pub fn stream_paused(key: &str, paused: bool) -> Value {
    json!({ "op": 22, "d": { "stream_key": key, "paused": paused } })
}

/// Voice gateway identify as the streamer: one screen stream, rid 100, quality 100.
pub fn identify(server_id: &str, me: &str, session_id: &str, token: &str) -> Value {
    json!({
        "server_id": server_id,
        "user_id": me,
        "session_id": session_id,
        "token": token,
        "video": true,
        "streams": [{ "type": "screen", "rid": "100", "quality": 100 }],
        "max_dave_protocol_version": 1,
    })
}

/// Select protocol: UDP, and the codecs we send (H.264 with its RTX, Opus).
pub fn select_protocol(address: &str, port: u16, mode: &str) -> Value {
    json!({
        "protocol": "udp",
        "data": { "address": address, "port": port, "mode": mode },
        "codecs": [
            { "name": "opus", "type": "audio", "priority": 1000, "payload_type": OPUS_PT },
            { "name": "H264", "type": "video", "priority": 1000, "payload_type": H264_PT, "rtx_payload_type": RTX_PT, "encode": true, "decode": false },
        ],
    })
}

/// The SSRCs the server gave us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ssrcs {
    pub audio: u32,
    pub video: u32,
    pub rtx: u32,
}

impl Ssrcs {
    /// From READY: our audio SSRC and the screen stream's. By Discord's habit video is audio+1
    /// and its RTX audio+2 when READY names no streams.
    pub fn from_ready(d: &Value) -> Self {
        let audio = d["ssrc"].as_u64().unwrap_or(0) as u32;
        let stream = &d["streams"][0];
        Self {
            audio,
            video: stream["ssrc"].as_u64().map(|s| s as u32).unwrap_or(audio.wrapping_add(1)),
            rtx: stream["rtx_ssrc"].as_u64().map(|s| s as u32).unwrap_or(audio.wrapping_add(2)),
        }
    }
}

/// What we send: picture size, frame rate, ceiling for the bitrate.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate: u64,
    pub sound: bool,
}

/// Opcode 12, VIDEO: which SSRCs carry our picture, and how big it is.
pub fn video_declare(ssrcs: Ssrcs, p: &Params) -> Value {
    json!({
        "audio_ssrc": if p.sound { ssrcs.audio } else { 0 },
        "video_ssrc": ssrcs.video,
        "rtx_ssrc": ssrcs.rtx,
        "streams": [{
            "type": "screen",
            "rid": "100",
            "ssrc": ssrcs.video,
            "active": true,
            "quality": 100,
            "rtx_ssrc": ssrcs.rtx,
            "max_bitrate": p.max_bitrate,
            "max_framerate": p.fps,
            "max_resolution": { "type": "fixed", "width": p.width, "height": p.height },
        }],
    })
}

/// Opcode 5, SPEAKING: the stream's sound is on (flag 2, "soundshare").
pub fn speaking(ssrc: u32, on: bool) -> Value {
    json!({ "speaking": if on { 2 } else { 0 }, "delay": 0, "ssrc": ssrc })
}

// ---------------------------------------------------------------- RTP

/// Most payload bytes in one RTP packet: with the 12-byte header, 16-byte tag and
/// 4-byte counter it stays well under a 1500-byte link.
pub const MAX_PAYLOAD: usize = 1150;

/// The 12-byte RTP header (RFC 3550): version 2, no padding, extension or CSRCs.
pub fn rtp_header(pt: u8, marker: bool, seq: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = pt & 0x7f | if marker { 0x80 } else { 0 };
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// Splits an Annex B access unit into its NAL units (start codes of 3 or 4 bytes, dropped).
pub fn split_nals(frame: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // (where the start code begins, where the NAL begins)
    let mut i = 0;
    while i + 3 <= frame.len() {
        if frame[i] == 0 && frame[i + 1] == 0 && frame[i + 2] == 1 {
            let begin = if i > 0 && frame[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (n, &(_, nal)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|&(begin, _)| begin).unwrap_or(frame.len());
        if nal < end {
            nals.push(&frame[nal..end]);
        }
    }
    nals
}

/// Packs NAL units into RTP payloads (RFC 6184): a NAL that fits goes alone, a bigger one
/// is cut into FU-A fragments. The last payload belongs to the packet that gets the marker bit.
pub fn packetize_h264(nals: &[&[u8]], max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for nal in nals {
        let Some(&header) = nal.first() else { continue };
        if nal.len() <= max {
            out.push(nal.to_vec());
            continue;
        }
        let (indicator, kind) = ((header & 0xe0) | 28, header & 0x1f);
        let body = &nal[1..];
        let size = max - 2;
        let count = body.len().div_ceil(size);
        for (n, part) in body.chunks(size).enumerate() {
            let flags = if n == 0 { 0x80 } else { 0 } | if n + 1 == count { 0x40 } else { 0 };
            let mut payload = vec![indicator, flags | kind];
            payload.extend_from_slice(part);
            out.push(payload);
        }
    }
    out
}

/// Whole RTP packets (header and payload, no encryption yet) for one frame's payloads:
/// consecutive sequence numbers from `first_seq`, the marker on the last.
pub fn rtp_packets(pt: u8, ssrc: u32, first_seq: u16, timestamp: u32, payloads: &[Vec<u8>]) -> Vec<Vec<u8>> {
    payloads
        .iter()
        .enumerate()
        .map(|(n, payload)| {
            let header = rtp_header(pt, n + 1 == payloads.len(), first_seq.wrapping_add(n as u16), timestamp, ssrc);
            let mut packet = header.to_vec();
            packet.extend_from_slice(payload);
            packet
        })
        .collect()
}

/// A retransmission (RFC 4588) of a packet: its own SSRC, type and sequence number,
/// the original sequence number in front of the original payload.
pub fn rtx_packet(original: &[u8], rtx_seq: u16, rtx_ssrc: u32) -> Option<Vec<u8>> {
    if original.len() < 12 {
        return None;
    }
    let marker = original[1] & 0x80 != 0;
    let timestamp = u32::from_be_bytes([original[4], original[5], original[6], original[7]]);
    let mut packet = rtp_header(RTX_PT, marker, rtx_seq, timestamp, rtx_ssrc).to_vec();
    packet.extend_from_slice(&original[2..4]);
    packet.extend_from_slice(&original[12..]);
    Some(packet)
}

/// Seals an RTP packet as `aead_aes256_gcm_rtpsize`: the 12-byte header stays plain and is
/// authenticated, the payload is encrypted, and a 16-byte tag and the 4-byte counter
/// that starts the nonce follow.
pub fn seal_rtp(cipher: &Aes256Gcm, packet: &[u8], counter: u32) -> Option<Vec<u8>> {
    let (header, payload) = packet.split_at_checked(12)?;
    let mut body = payload.to_vec();
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    let tag = cipher.encrypt_in_place_detached(Nonce::from_slice(&nonce), header, &mut body).ok()?;
    let mut sealed = header.to_vec();
    sealed.extend_from_slice(&body);
    sealed.extend_from_slice(&tag);
    sealed.extend_from_slice(&counter.to_be_bytes());
    Some(sealed)
}

/// Opens RTCP that came from the server (the reverse of `seal_rtcp`).
pub fn open_rtcp(cipher: &Aes256Gcm, packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < 8 + 20 {
        return None;
    }
    let end = packet.len();
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&packet[end - 4..]);
    let tag = Tag::from_slice(&packet[end - 20..end - 4]);
    let mut body = packet[8..end - 20].to_vec();
    cipher.decrypt_in_place_detached(Nonce::from_slice(&nonce), &packet[..8], &mut body, tag).ok()?;
    let mut plain = packet[..8].to_vec();
    plain.extend_from_slice(&body);
    Some(plain)
}

/// SPS and PPS of the stream, so every keyframe can carry them: a viewer who joins late
/// needs both before the picture.
#[derive(Default)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// The frame as Annex B with 4-byte start codes, SPS and PPS ahead of an IDR slice that lacks them.
    pub fn prepare(&mut self, frame: &[u8]) -> Vec<u8> {
        let nals = split_nals(frame);
        for nal in &nals {
            match nal.first().map(|h| h & 0x1f) {
                Some(7) => self.sps = Some(nal.to_vec()),
                Some(8) => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        let has = |kind: u8| nals.iter().any(|n| n.first().is_some_and(|h| h & 0x1f == kind));
        let mut out = Vec::with_capacity(frame.len() + 64);
        if has(5) {
            if let (false, Some(sps)) = (has(7), &self.sps) {
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(sps);
            }
            if let (false, Some(pps)) = (has(8), &self.pps) {
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(pps);
            }
        }
        for nal in nals {
            if nal.first().is_some_and(|h| h & 0x1f == 9) {
                continue; // access unit delimiters carry nothing for a viewer
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out
    }
}

/// RTP time for a frame: 90 kHz, counted from the first frame, on top of a random start.
pub fn video_timestamp(base: u32, first_us: i64, time_us: i64) -> u32 {
    base.wrapping_add(((time_us - first_us).max(0) as u64 * 9 / 100) as u32)
}

// ---------------------------------------------------------------- RTCP

/// What the server asks of us in RTCP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Feedback {
    /// Picture loss (PLI) or full intra request (FIR): make a keyframe.
    Keyframe,
    /// Receiver estimated max bitrate, in bits a second.
    Remb(u64),
    /// Sequence numbers that never arrived.
    Nack(Vec<u16>),
}

/// Reads a compound RTCP packet (already opened) for the requests we act on.
pub fn parse_rtcp(data: &[u8]) -> Vec<Feedback> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= data.len() {
        let (fmt, kind) = (data[at] & 0x1f, data[at + 1]);
        let len = (u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize + 1) * 4;
        let Some(body) = data.get(at + 4..(at + len).min(data.len())) else { break };
        match (kind, fmt) {
            (206, 1) | (206, 4) => out.push(Feedback::Keyframe),
            (205, 1) => {
                let mut missing = Vec::new();
                for entry in body.get(8..).unwrap_or_default().chunks_exact(4) {
                    let pid = u16::from_be_bytes([entry[0], entry[1]]);
                    let mask = u16::from_be_bytes([entry[2], entry[3]]);
                    missing.push(pid);
                    for bit in 0..16 {
                        if mask & (1 << bit) != 0 {
                            missing.push(pid.wrapping_add(bit + 1));
                        }
                    }
                }
                out.push(Feedback::Nack(missing));
            }
            (206, 15) if body.get(8..12) == Some(b"REMB") && body.len() >= 16 => {
                let exp = (body[13] >> 2) as u32;
                let mantissa = (((body[13] & 3) as u64) << 16) | ((body[14] as u64) << 8) | body[15] as u64;
                out.push(Feedback::Remb(mantissa << exp.min(40)));
            }
            _ => {}
        }
        at += len;
    }
    out
}

/// RTCP sender report (RFC 3550) without report blocks: what a sender says now and then so the
/// server can tell how much we have sent.
pub fn sender_report(ssrc: u32, ntp_ms: u64, rtp_timestamp: u32, packets: u32, octets: u32) -> Vec<u8> {
    // NTP: seconds since 1900, and a fraction of a second.
    let seconds = (ntp_ms / 1000 + 2_208_988_800) as u32;
    let fraction = (((ntp_ms % 1000) << 32) / 1000) as u32;
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&seconds.to_be_bytes());
    p.extend_from_slice(&fraction.to_be_bytes());
    p.extend_from_slice(&rtp_timestamp.to_be_bytes());
    p.extend_from_slice(&packets.to_be_bytes());
    p.extend_from_slice(&octets.to_be_bytes());
    p
}

/// The encoder's bitrate for the server's estimate: all of it, since blocks cost more than the odd resend;
/// above it the server loses most of what comes. Never below a floor where the picture is useless,
/// never above the stream's own ceiling.
pub fn adapt_bitrate(remb: u64, ceiling: u64) -> u64 {
    remb.clamp(500_000.min(ceiling), ceiling)
}

// ---------------------------------------------------------------- DAVE

/// Our end of Discord's end-to-end encryption on the stream connection, driven by the
/// same opcodes as the viewer's (golive.rs).
struct Dave {
    session: Option<davey::DaveSession>,
    version: u16,
    pending: HashMap<u16, u16>,
    users: HashSet<u64>,
    me: u64,
    channel: u64,
}

impl Dave {
    fn new(me: &str, server_id: &str) -> Self {
        // DAVE's channel id for a stream is its RTC server id minus one.
        let channel = server_id.parse::<u64>().unwrap_or(1).saturating_sub(1);
        Self { session: None, version: 0, pending: HashMap::new(), users: HashSet::new(), me: me.parse().unwrap_or(0), channel }
    }

    /// Frames go out encrypted from the moment a version is agreed.
    fn on(&self) -> bool {
        self.version > 0
    }

    fn ready(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_ready())
    }

    fn reinit(&mut self) -> Option<Vec<u8>> {
        let version = NonZeroU16::new(self.version)?;
        let (me, channel) = (self.me, self.channel);
        let result = match &mut self.session {
            Some(session) => session.reinit(version, me, channel, None).map_err(|e| format!("{e:?}")).and_then(|_| session.create_key_package().map_err(|e| format!("{e:?}"))),
            None => davey::DaveSession::new(version, me, channel, None).map_err(|e| format!("{e:?}")).and_then(|mut session| {
                let package = session.create_key_package().map_err(|e| format!("{e:?}"));
                self.session = Some(session);
                package
            }),
        };
        match result {
            Ok(package) => Some(package),
            Err(e) => {
                crate::log_line(&format!("go live send: DAVE init failed: {e}"));
                None
            }
        }
    }

    fn encrypt(&mut self, media: davey::MediaType, codec: davey::Codec, frame: &[u8]) -> Result<Vec<u8>, String> {
        let session = self.session.as_mut().ok_or("no DAVE session")?;
        session.encrypt(media, codec, frame).map(|c| c.into_owned()).map_err(|e| format!("{e:?}"))
    }

    fn execute(&mut self, id: u16) {
        let Some(version) = self.pending.remove(&id) else { return };
        self.version = version;
        crate::log_line(&format!("go live send: DAVE transition {id} -> version {version}"));
    }

    /// JSON ops 21 (prepare transition), 22 (execute), 24 (prepare epoch), plus clients coming and going.
    fn on_json(&mut self, op: u64, d: &Value, out: &mut Vec<Message>) {
        match op {
            11 => {
                for id in d["user_ids"].as_array().into_iter().flatten() {
                    if let Some(id) = id.as_str().and_then(|s| s.parse().ok()) {
                        self.users.insert(id);
                    }
                }
            }
            13 => {
                if let Some(id) = d["user_id"].as_str().and_then(|s| s.parse().ok()) {
                    self.users.remove(&id);
                }
            }
            21 => {
                let (id, version) = (d["transition_id"].as_u64().unwrap_or(0) as u16, d["protocol_version"].as_u64().unwrap_or(0) as u16);
                self.pending.insert(id, version);
                if id == 0 {
                    self.execute(0);
                } else {
                    out.push(json_msg(23, json!({ "transition_id": id })));
                }
            }
            22 => self.execute(d["transition_id"].as_u64().unwrap_or(0) as u16),
            24 if d["epoch"].as_u64() == Some(1) => {
                self.version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if let Some(package) = self.reinit() {
                    out.push(binary_msg(26, &package));
                }
            }
            _ => {}
        }
    }

    /// Binary ops 25 (external sender), 27 (proposals), 29 (commit), 30 (welcome).
    fn on_binary(&mut self, op: u8, payload: &[u8], out: &mut Vec<Message>) {
        match op {
            25 => {
                if let Some(session) = &mut self.session {
                    if let Err(e) = session.set_external_sender(payload) {
                        crate::log_line(&format!("go live send: DAVE external sender: {e:?}"));
                    }
                }
            }
            27 if !payload.is_empty() => {
                let kind = if payload[0] == 0 { davey::ProposalsOperationType::APPEND } else { davey::ProposalsOperationType::REVOKE };
                let users: Vec<u64> = self.users.iter().copied().collect();
                if let Some(session) = &mut self.session {
                    match session.process_proposals(kind, &payload[1..], Some(&users)) {
                        Ok(Some(cw)) => {
                            let mut data = cw.commit;
                            if let Some(welcome) = cw.welcome {
                                data.extend_from_slice(&welcome);
                            }
                            out.push(binary_msg(28, &data));
                        }
                        Ok(None) => {}
                        Err(e) => crate::log_line(&format!("go live send: DAVE proposals: {e:?}")),
                    }
                }
            }
            29 | 30 if payload.len() >= 2 => {
                let id = u16::from_be_bytes([payload[0], payload[1]]);
                let Some(session) = &mut self.session else { return };
                let result = if op == 29 { session.process_commit(&payload[2..]).map_err(|e| format!("{e:?}")) } else { session.process_welcome(&payload[2..]).map_err(|e| format!("{e:?}")) };
                match result {
                    Ok(()) => {
                        crate::log_line(&format!("go live send: DAVE {} ok (transition {id})", if op == 29 { "commit" } else { "welcome" }));
                        if id != 0 {
                            self.pending.insert(id, self.version);
                            out.push(json_msg(23, json!({ "transition_id": id })));
                        }
                    }
                    Err(e) => {
                        crate::log_line(&format!("go live send: DAVE {op} failed: {e}"));
                        out.push(json_msg(31, json!({ "transition_id": id })));
                        if let Some(package) = self.reinit() {
                            out.push(binary_msg(26, &package));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- the stream

/// What the encoder lets us do.
#[derive(Clone)]
pub struct Controls {
    pub keyframe: Arc<dyn Fn() + Send + Sync>,
    pub bitrate: Arc<dyn Fn(u64) + Send + Sync>,
}

struct FrameIn {
    data: Vec<u8>,
    key: bool,
    time_us: i64,
}

struct Inputs {
    video: UnboundedReceiver<FrameIn>,
    sound: UnboundedReceiver<Vec<i16>>,
}

/// One Go Live stream of ours, from asking until it's closed. Dropping it stops it (opcode 19).
pub struct Stream {
    gateway: UnboundedSender<Value>,
    key: String,
    me: String,
    session_id: String,
    rtc_server_id: Option<String>,
    server: Option<(String, String)>,
    params: Params,
    controls: Controls,
    status: Status,
    /// True while the media connection can take frames; before that the sink drops them.
    live: Arc<AtomicBool>,
    video_tx: UnboundedSender<FrameIn>,
    inputs: Arc<tokio::sync::Mutex<Inputs>>,
    sound_tap: SoundTap,
    task: Option<JoinHandle<()>>,
}

impl Stream {
    /// Asks Discord for a stream in the call we are in.
    #[allow(clippy::too_many_arguments)]
    pub fn start(gateway: UnboundedSender<Value>, place: &Place, me: &str, session_id: &str, params: Params, controls: Controls, status: Status) -> Self {
        let key = place.stream_key(me);
        let _ = gateway.send(stream_create(place));
        crate::log_line(&format!("go live send: asked to stream {key} ({}x{} @{}, up to {} bps)", params.width, params.height, params.fps, params.max_bitrate));
        status("запрашиваю стрим…".into());
        let (video_tx, video) = unbounded_channel();
        let (sound_tap, sound) = unbounded_channel();
        Self {
            gateway,
            key,
            me: me.to_string(),
            session_id: session_id.to_string(),
            rtc_server_id: None,
            server: None,
            params,
            controls,
            status,
            live: Arc::new(AtomicBool::new(false)),
            video_tx,
            inputs: Arc::new(tokio::sync::Mutex::new(Inputs { video, sound })),
            sound_tap,
            task: None,
        }
    }

    /// For the encoder: hands each frame to the media connection while there is one.
    pub fn video_sink(&self) -> FrameSink {
        let (tx, live) = (self.video_tx.clone(), self.live.clone());
        Box::new(move |data: &[u8], key: bool, time_us: i64| {
            if !live.load(Ordering::Relaxed) {
                return !tx.is_closed();
            }
            tx.send(FrameIn { data: data.to_vec(), key, time_us }).is_ok()
        })
    }

    /// For the system sound: 10 ms stereo chunks.
    pub fn sound_tap(&self) -> SoundTap {
        self.sound_tap.clone()
    }

    /// STREAM_CREATE / STREAM_SERVER_UPDATE / STREAM_DELETE from the main gateway.
    /// Returns true once the stream is over.
    pub fn on_event(&mut self, kind: &str, data: &Value) -> bool {
        if data["stream_key"].as_str() != Some(self.key.as_str()) {
            return false;
        }
        match kind {
            "STREAM_CREATE" => {
                crate::log_line(&format!("go live send: STREAM_CREATE {}", short(&redacted(data))));
                self.rtc_server_id = data["rtc_server_id"].as_str().map(String::from);
                // The way Discord's own client leaves it: running, not paused.
                let _ = self.gateway.send(stream_paused(&self.key, false));
            }
            "STREAM_SERVER_UPDATE" => {
                crate::log_line(&format!("go live send: STREAM_SERVER_UPDATE endpoint={}", data["endpoint"]));
                if let (Some(endpoint), Some(token)) = (data["endpoint"].as_str(), data["token"].as_str()) {
                    self.server = Some((endpoint.to_string(), token.to_string()));
                }
            }
            "STREAM_DELETE" => {
                crate::log_line(&format!("go live send: STREAM_DELETE {}", short(&redacted(data))));
                // Our own earlier stream being closed as this one begins: this one has not even been created yet.
                if data["reason"].as_str() == Some("user_requested") && self.rtc_server_id.is_none() {
                    return false;
                }
                self.live.store(false, Ordering::Relaxed);
                if let Some(task) = self.task.take() {
                    task.abort();
                }
                (self.status)(crate::i18n::t("стрим закрыт"));
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
        let link = Link { endpoint, token, server_id, me: self.me.clone(), session_id: self.session_id.clone() };
        let (params, controls, status, live, inputs) = (self.params, self.controls.clone(), self.status.clone(), self.live.clone(), self.inputs.clone());
        self.task = Some(tokio::spawn(async move {
            let result = run(link, params, controls, status.clone(), live.clone(), inputs).await;
            live.store(false, Ordering::Relaxed);
            if let Err(e) = result {
                crate::log_line(&format!("go live send: stopped: {e}"));
                status(crate::i18n::tf("ошибка: {}", &[&e]));
            }
        }));
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = self.gateway.send(stream_delete(&self.key));
        crate::log_line("go live send: stopped streaming");
    }
}

/// An event without the token, for the log.
fn redacted(data: &Value) -> Value {
    let mut shown = data.clone();
    if let Some(token) = shown.get_mut("token") {
        *token = json!("<token>");
    }
    shown
}

struct Link {
    endpoint: String,
    token: String,
    server_id: String,
    me: String,
    session_id: String,
}

/// Everything the media connection keeps between messages.
struct Media {
    cipher: Option<Aes256Gcm>,
    udp: Option<Arc<UdpSocket>>,
    ssrcs: Ssrcs,
    params: Params,
    dave: Dave,
    /// One counter for every sealed packet, RTP and RTCP: a nonce is never used twice.
    counter: u32,
    seq_video: u16,
    seq_sound: u16,
    seq_rtx: u16,
    ts_base: u32,
    sound_ts: u32,
    first_us: Option<i64>,
    last_video_ts: u32,
    param_sets: ParamSets,
    /// The last packets sent, for retransmissions (by sequence number).
    ring: VecDeque<(u16, Vec<u8>)>,
    /// Video packets waiting for their moment to go out.
    outbox: VecDeque<Vec<u8>>,
    outbox_bytes: usize,
    pace_credit: f64,
    paced_at: Option<Instant>,
    video_packets: u32,
    video_octets: u32,
    sound_packets: u32,
    sound_octets: u32,
    /// We wait for a keyframe to begin: nothing else helps a viewer.
    started: bool,
    declared: bool,
    stats: SendStats,
    opus: Option<songbird::driver::opus::Encoder>,
    sound_pending: Vec<i16>,
    last_bitrate: u64,
    last_bitrate_at: Option<Instant>,
    last_keyframe_at: Option<Instant>,
}

#[derive(Default)]
struct SendStats {
    frames: u64,
    keyframes: u64,
    packets: u64,
    dropped_no_dave: u64,
    dave_failed: u64,
    resent: u64,
    keyframe_requests: u64,
    sound_frames: u64,
    first_error: Option<String>,
    /// The most that waited to go out since the last report, in bytes: what holds frames up.
    peak_waiting: std::cell::Cell<usize>,
    /// What had gone out at the last report, and when: for the real rate.
    reported: std::cell::Cell<Option<(Instant, u64)>>,
    video_bytes: u64,
}

/// How many sent packets are kept for retransmission.
const RING: usize = 1024;
/// Packets go out at this many times the stream's bitrate, so a big frame (motion, a keyframe)
/// doesn't overflow the socket and the network in one burst.
const PACE_FACTOR: f64 = 2.5;
/// Whatever waits is sent within this long: the picture must not fall behind.
const PACE_MAX_DELAY: f64 = 0.03;
const PACE_TICK: Duration = Duration::from_millis(2);
/// What a frame may send at once, before pacing begins.
const PACE_FIRST_BYTES: f64 = 12_000.0;

/// Bytes a second to send at, for a bitrate and what is waiting.
fn pace_rate(bitrate: u64, queued: usize) -> f64 {
    (bitrate as f64 / 8.0 * PACE_FACTOR).max(queued as f64 / PACE_MAX_DELAY)
}

impl Media {
    fn send_sealed(&mut self, packet: &[u8]) -> bool {
        let (Some(cipher), Some(udp)) = (&self.cipher, &self.udp) else { return false };
        self.counter = self.counter.wrapping_add(1);
        seal_rtp(cipher, packet, self.counter).is_some_and(|sealed| udp.try_send(&sealed).is_ok())
    }

    fn send_rtcp(&mut self, rtcp: &[u8]) -> bool {
        let (Some(cipher), Some(udp)) = (&self.cipher, &self.udp) else { return false };
        self.counter = self.counter.wrapping_add(1);
        seal_rtcp(cipher, rtcp, self.counter).is_some_and(|sealed| udp.try_send(&sealed).is_ok())
    }

    fn note_error(&mut self, what: String) {
        if self.stats.first_error.is_none() {
            crate::log_line(&format!("go live send: {what}"));
            self.stats.first_error = Some(what);
        }
    }

    /// One frame from the encoder: parameter sets, DAVE, packets out.
    fn on_frame(&mut self, frame: FrameIn, controls: &Controls) {
        if !self.declared {
            return;
        }
        if !self.started {
            if !frame.key {
                (controls.keyframe)();
                return;
            }
            self.started = true;
        }
        let annex_b = self.param_sets.prepare(&frame.data);
        let payload_frame = if self.dave.on() {
            if !self.dave.ready() {
                self.stats.dropped_no_dave += 1;
                return;
            }
            match self.dave.encrypt(davey::MediaType::VIDEO, davey::Codec::H264, &annex_b) {
                Ok(encrypted) => encrypted,
                Err(e) => {
                    self.stats.dave_failed += 1;
                    self.note_error(format!("DAVE encrypt failed: {e}"));
                    return;
                }
            }
        } else {
            annex_b
        };
        let first_us = *self.first_us.get_or_insert(frame.time_us);
        let timestamp = video_timestamp(self.ts_base, first_us, frame.time_us);
        self.last_video_ts = timestamp;
        let payloads = packetize_h264(&split_nals(&payload_frame), MAX_PAYLOAD);
        let packets = rtp_packets(H264_PT, self.ssrcs.video, self.seq_video, timestamp, &payloads);
        self.seq_video = self.seq_video.wrapping_add(packets.len() as u16);
        for (n, packet) in packets.into_iter().enumerate() {
            let seq = self.seq_video.wrapping_sub((payloads.len() - n) as u16);
            if self.ring.len() >= RING {
                self.ring.pop_front();
            }
            self.outbox_bytes += packet.len();
            self.outbox.push_back(packet.clone());
            self.ring.push_back((seq, packet));
        }
        // Stuck behind a socket that doesn't take anything: the oldest are of no use any more.
        while self.outbox.len() > RING {
            if let Some(old) = self.outbox.pop_front() {
                self.outbox_bytes -= old.len();
            }
        }
        self.stats.frames += 1;
        if frame.key {
            self.stats.keyframes += 1;
        }
        self.stats.peak_waiting.set(self.stats.peak_waiting.get().max(self.outbox_bytes));
        self.stats.video_bytes += payload_frame.len() as u64;
        self.pump();
    }

    /// Sends what the pace allows by now; the timer in `run` calls again while packets wait.
    fn pump(&mut self) {
        let now = Instant::now();
        let bitrate = if self.last_bitrate > 0 { self.last_bitrate } else { self.params.max_bitrate };
        match self.paced_at {
            // Timers on Windows can be late; what is due by then goes out, not less.
            Some(at) => self.pace_credit += pace_rate(bitrate, self.outbox_bytes) * now.duration_since(at).as_secs_f64().min(0.05),
            None => self.pace_credit = PACE_FIRST_BYTES,
        }
        self.paced_at = Some(now);
        while self.pace_credit > 0.0 {
            let Some(packet) = self.outbox.pop_front() else { break };
            if !self.send_sealed(&packet) {
                self.outbox.push_front(packet);
                return; // the socket is full: the next tick tries again
            }
            self.outbox_bytes -= packet.len();
            self.pace_credit -= packet.len() as f64;
            self.stats.packets += 1;
            self.video_packets = self.video_packets.wrapping_add(1);
            self.video_octets = self.video_octets.wrapping_add((packet.len() - 12) as u32);
        }
        if self.outbox.is_empty() {
            self.paced_at = None;
        }
    }

    /// The server asked for something.
    fn on_feedback(&mut self, feedback: Feedback, controls: &Controls) {
        match feedback {
            Feedback::Keyframe => {
                if self.last_keyframe_at.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                    self.last_keyframe_at = Some(Instant::now());
                    self.stats.keyframe_requests += 1;
                    (controls.keyframe)();
                }
            }
            Feedback::Remb(bps) => {
                let target = adapt_bitrate(bps, self.params.max_bitrate);
                let moved = self.last_bitrate == 0 || target.abs_diff(self.last_bitrate) * 10 > self.last_bitrate;
                if moved && self.last_bitrate_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    self.last_bitrate = target;
                    self.last_bitrate_at = Some(Instant::now());
                    crate::log_line(&format!("go live send: server estimate {bps} bps, encoder to {target}"));
                    (controls.bitrate)(target);
                }
            }
            Feedback::Nack(missing) => {
                for seq in missing.into_iter().take(200) {
                    let Some((_, original)) = self.ring.iter().find(|(s, _)| *s == seq) else { continue };
                    let original = original.clone();
                    self.seq_rtx = self.seq_rtx.wrapping_add(1);
                    if let Some(rtx) = rtx_packet(&original, self.seq_rtx, self.ssrcs.rtx) {
                        if self.send_sealed(&rtx) {
                            self.stats.resent += 1;
                        }
                    }
                }
            }
        }
    }

    /// 10 ms chunks of stereo sound; every two make one 20 ms Opus frame.
    fn on_sound(&mut self, chunk: Vec<i16>) {
        if !self.declared || !self.params.sound || (self.dave.on() && !self.dave.ready()) {
            return;
        }
        self.sound_pending.extend_from_slice(&chunk);
        while self.sound_pending.len() >= 1920 {
            let pcm: Vec<i16> = self.sound_pending.drain(..1920).collect();
            if self.opus.is_none() {
                match songbird::driver::opus::Encoder::new(crate::audio::SAMPLE_RATE, songbird::driver::opus::Channels::Stereo, songbird::driver::opus::Application::Audio) {
                    Ok(encoder) => self.opus = Some(encoder),
                    Err(e) => {
                        self.note_error(format!("no Opus encoder: {e:?}"));
                        self.sound_pending.clear();
                        return;
                    }
                }
            }
            let mut opus = vec![0u8; 1500];
            let Some(Ok(len)) = self.opus.as_mut().map(|e| e.encode(&pcm, &mut opus)) else { continue };
            opus.truncate(len);
            let payload = if self.dave.on() {
                match self.dave.encrypt(davey::MediaType::AUDIO, davey::Codec::OPUS, &opus) {
                    Ok(encrypted) => encrypted,
                    Err(e) => {
                        self.stats.dave_failed += 1;
                        self.note_error(format!("DAVE sound encrypt failed: {e}"));
                        continue;
                    }
                }
            } else {
                opus
            };
            let packet = rtp_packets(OPUS_PT, self.ssrcs.audio, self.seq_sound, self.sound_ts, &[payload]).remove(0);
            self.seq_sound = self.seq_sound.wrapping_add(1);
            self.sound_ts = self.sound_ts.wrapping_add(960);
            if self.send_sealed(&packet) {
                self.sound_packets = self.sound_packets.wrapping_add(1);
                self.sound_octets = self.sound_octets.wrapping_add((packet.len() - 12) as u32);
                self.stats.sound_frames += 1;
            }
        }
    }

    fn send_reports(&mut self) {
        if !self.declared {
            return;
        }
        let now = now_ms();
        let video = sender_report(self.ssrcs.video, now, self.last_video_ts, self.video_packets, self.video_octets);
        self.send_rtcp(&video);
        if self.params.sound {
            let sound = sender_report(self.ssrcs.audio, now, self.sound_ts, self.sound_packets, self.sound_octets);
            self.send_rtcp(&sound);
        }
    }

    fn report(&self) -> String {
        let s = &self.stats;
        let now = Instant::now();
        let kbps = s.reported.replace(Some((now, s.video_bytes))).map_or(0, |(at, bytes)| (s.video_bytes - bytes) * 8 / 1000 / now.duration_since(at).as_secs().max(1));
        format!(
            "sent {} frames ({} keyframes), {} packets, {} resent, {} sound frames; dropped before DAVE was ready {}, DAVE failed {}; keyframe requests {}; most waiting {} KB; {kbps} kbps of {}; DAVE {}",
            s.frames,
            s.keyframes,
            s.packets,
            s.resent,
            s.sound_frames,
            s.dropped_no_dave,
            s.dave_failed,
            s.keyframe_requests,
            s.peak_waiting.take() / 1024,
            if self.last_bitrate > 0 { self.last_bitrate } else { self.params.max_bitrate } / 1000,
            if self.dave.on() { "on" } else { "off" }
        )
    }
}

async fn run(link: Link, params: Params, controls: Controls, status: Status, live: Arc<AtomicBool>, inputs: Arc<tokio::sync::Mutex<Inputs>>) -> Result<(), String> {
    let endpoint = link.endpoint.trim_start_matches("wss://").trim_end_matches('/').to_string();
    let (ws, _) = connect_async(format!("wss://{endpoint}/?v=8")).await.map_err(|e| format!("media server: {e}"))?;
    let (mut tx, mut rx) = ws.split();
    tx.send(json_msg(0, identify(&link.server_id, &link.me, &link.session_id, &link.token))).await.map_err(|e| e.to_string())?;
    crate::log_line(&format!("go live send: media server {endpoint}, identify sent"));

    let mut media = Media {
        cipher: None,
        udp: None,
        ssrcs: Ssrcs::default(),
        params,
        dave: Dave::new(&link.me, &link.server_id),
        counter: rand_counter(),
        seq_video: rand_counter() as u16,
        seq_sound: rand_counter() as u16,
        seq_rtx: rand_counter() as u16,
        ts_base: rand_counter(),
        sound_ts: rand_counter(),
        first_us: None,
        last_video_ts: 0,
        param_sets: ParamSets::default(),
        ring: VecDeque::new(),
        outbox: VecDeque::new(),
        outbox_bytes: 0,
        pace_credit: 0.0,
        paced_at: None,
        video_packets: 0,
        video_octets: 0,
        sound_packets: 0,
        sound_octets: 0,
        started: false,
        declared: false,
        stats: SendStats::default(),
        opus: None,
        sound_pending: Vec::new(),
        last_bitrate: 0,
        last_bitrate_at: None,
        last_keyframe_at: None,
    };
    let mut guard = inputs.lock().await;
    let Inputs { video: frames_in, sound: sound_in } = &mut *guard;
    let mut seq_ack: i64 = -1;
    let mut heartbeat: Option<tokio::time::Interval> = None;
    let mut reports = tokio::time::interval(Duration::from_secs(1));
    let mut log_tick = tokio::time::interval(Duration::from_secs(5));
    let mut pace = tokio::time::interval(PACE_TICK);
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (udp_tx, mut udp_rx) = unbounded_channel::<Vec<u8>>();
    let mut seen: HashSet<u64> = HashSet::new();

    loop {
        let mut out: Vec<Message> = Vec::new();
        tokio::select! {
            msg = rx.next() => {
                let msg = msg.ok_or("media server closed the connection")?.map_err(|e| e.to_string())?;
                match msg {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                        if let Some(seq) = payload["seq"].as_i64() {
                            seq_ack = seq;
                        }
                        on_json(&mut media, &payload, &mut out, &mut heartbeat, &udp_tx, &status, &live, &controls, &mut seen).await?;
                    }
                    Message::Binary(data) if data.len() >= 3 => {
                        seq_ack = u16::from_be_bytes([data[0], data[1]]) as i64;
                        media.dave.on_binary(data[2], &data[3..], &mut out);
                    }
                    Message::Close(frame) => return Err(format!("media server closed: {frame:?}")),
                    _ => {}
                }
            }
            _ = async { match &mut heartbeat { Some(h) => { h.tick().await; } None => std::future::pending().await } } => {
                out.push(json_msg(3, json!({ "t": now_ms(), "seq_ack": seq_ack })));
            }
            Some(packet) = udp_rx.recv() => {
                // Only RTCP comes back to a sender; it says what the server wants.
                if packet.len() > 1 && (200..=206).contains(&packet[1]) {
                    if let Some(cipher) = &media.cipher {
                        if let Some(plain) = open_rtcp(cipher, &packet) {
                            for feedback in parse_rtcp(&plain) {
                                media.on_feedback(feedback, &controls);
                            }
                        }
                    }
                }
            }
            _ = pace.tick(), if !media.outbox.is_empty() => media.pump(),
            Some(frame) = frames_in.recv() => media.on_frame(frame, &controls),
            Some(chunk) = sound_in.recv() => media.on_sound(chunk),
            _ = reports.tick() => media.send_reports(),
            _ = log_tick.tick() => {
                if media.declared {
                    crate::log_line(&format!("go live send: {}", media.report()));
                    status(format!("в эфире · кадров {}", media.stats.frames));
                }
            }
        }
        for msg in out {
            tx.send(msg).await.map_err(|e| e.to_string())?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_json(
    media: &mut Media,
    payload: &Value,
    out: &mut Vec<Message>,
    heartbeat: &mut Option<tokio::time::Interval>,
    udp_tx: &UnboundedSender<Vec<u8>>,
    status: &Status,
    live: &Arc<AtomicBool>,
    controls: &Controls,
    seen: &mut HashSet<u64>,
) -> Result<(), String> {
    let op = payload["op"].as_u64().unwrap_or(u64::MAX);
    let d = &payload["d"];
    match op {
        8 => {
            let ms = d["heartbeat_interval"].as_f64().unwrap_or(13750.0) as u64;
            let mut interval = tokio::time::interval(Duration::from_millis(ms.max(1000)));
            interval.tick().await;
            *heartbeat = Some(interval);
        }
        // Ready: our SSRCs and where the media server listens.
        2 => {
            crate::log_line(&format!("go live send: READY {}", short(d)));
            media.ssrcs = Ssrcs::from_ready(d);
            let (ip, port) = (d["ip"].as_str().unwrap_or(""), d["port"].as_u64().unwrap_or(0) as u16);
            let modes: Vec<&str> = d["modes"].as_array().map(|m| m.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            // The same mode the viewer opens: the only one this client seals.
            let mode = "aead_aes256_gcm_rtpsize";
            if !modes.contains(&mode) {
                return Err("the server offers no supported encryption mode".into());
            }
            let udp = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
            udp.connect((ip, port)).await.map_err(|e| e.to_string())?;
            let (address, our_port) = discover(&udp, media.ssrcs.audio).await?;
            let udp = Arc::new(udp);
            let (reader, forward) = (udp.clone(), udp_tx.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                while let Ok(n) = reader.recv(&mut buf).await {
                    if forward.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            media.udp = Some(udp);
            out.push(json_msg(1, select_protocol(&address, our_port, mode)));
            crate::log_line(&format!("go live send: select protocol udp {mode}, ssrcs {:?}", media.ssrcs));
        }
        // Session description: the transport key and whether DAVE is on.
        4 => {
            let mut shown = d.clone();
            if let Some(key) = shown.get_mut("secret_key") {
                *key = json!(format!("<{} bytes>", key.as_array().map(|a| a.len()).unwrap_or(0)));
            }
            crate::log_line(&format!("go live send: SESSION_DESCRIPTION {}", short(&shown)));
            let key: Vec<u8> = d["secret_key"].as_array().map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default();
            media.cipher = Aes256Gcm::new_from_slice(&key).ok();
            if media.cipher.is_none() {
                return Err("bad transport key".into());
            }
            media.dave.version = d["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
            if let Some(package) = media.dave.reinit() {
                out.push(binary_msg(26, &package));
            }
            // Say what we send, then speak; frames start once the server knows.
            out.push(json_msg(12, video_declare(media.ssrcs, &media.params)));
            if media.params.sound {
                out.push(json_msg(5, speaking(media.ssrcs.audio, true)));
            }
            media.declared = true;
            live.store(true, Ordering::Relaxed);
            (controls.keyframe)();
            status("подключено, жду ключевой кадр".into());
        }
        6 => {} // heartbeat ack
        11 | 13 | 21 | 22 | 24 => media.dave.on_json(op, d, out),
        _ => {
            if seen.insert(op) {
                crate::log_line(&format!("go live send: op {op} {}", short(d)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_paced_but_never_fall_far_behind() {
        // 8 Mbit/s: a megabyte a second, times the pace factor.
        assert_eq!(pace_rate(8_000_000, 1000), 2_500_000.0);
        // A lot waiting: all of it within the longest delay.
        assert_eq!(pace_rate(8_000_000, 1_000_000), 1_000_000.0 / PACE_MAX_DELAY);
    }
    use crate::golive::{open_rtp, Assembled, Assembler};

    fn cipher() -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&[5u8; 32]).unwrap()
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn header_has_version_marker_and_fields() {
        let h = rtp_header(101, true, 0xfffe, 0x01020304, 0xaabbccdd);
        assert_eq!(h, [0x80, 0x80 | 101, 0xff, 0xfe, 1, 2, 3, 4, 0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(rtp_header(120, false, 1, 2, 3)[1], 120);
    }

    #[test]
    fn splits_annex_b_with_both_start_code_lengths() {
        let frame = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        assert_eq!(split_nals(&frame), vec![&[0x67u8, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert!(split_nals(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn small_nals_go_alone_and_big_ones_in_fu_a() {
        let small = [0x67u8, 1, 2];
        let big: Vec<u8> = std::iter::once(0x65).chain((0..250).map(|i| i as u8)).collect();
        let payloads = packetize_h264(&[&small, &big], 100);
        assert_eq!(payloads[0], small);
        let fragments = &payloads[1..];
        assert_eq!(fragments.len(), 3, "250 bytes of body in pieces of 98");
        assert!(fragments.iter().all(|p| p.len() <= 100 && p[0] == (0x65 & 0xe0 | 28)));
        assert_eq!(fragments[0][1], 0x80 | 5);
        assert_eq!(fragments[1][1], 5);
        assert_eq!(fragments[2][1], 0x40 | 5);
    }

    #[test]
    fn packets_carry_sequence_numbers_and_the_marker_on_the_last() {
        let packets = rtp_packets(H264_PT, 7, 65535, 1000, &[vec![1], vec![2], vec![3]]);
        let seqs: Vec<u16> = packets.iter().map(|p| u16::from_be_bytes([p[2], p[3]])).collect();
        assert_eq!(seqs, vec![65535, 0, 1]);
        let markers: Vec<bool> = packets.iter().map(|p| p[1] & 0x80 != 0).collect();
        assert_eq!(markers, vec![false, false, true]);
    }

    #[test]
    fn a_frame_survives_packets_sealing_and_the_viewers_assembler() {
        let c = cipher();
        let idr: Vec<u8> = std::iter::once(0x65).chain((0..3000).map(|i| (i % 251) as u8)).collect();
        let frame = annex_b(&[&[0x67, 1, 2, 3], &[0x68, 4, 5], &idr]);
        let payloads = packetize_h264(&split_nals(&frame), MAX_PAYLOAD);
        let packets = rtp_packets(H264_PT, 99, 65534, 9000, &payloads);
        assert!(packets.len() > 3);
        let now = Instant::now();
        let mut assembler = Assembler::default();
        let mut got = Vec::new();
        for (n, packet) in packets.iter().enumerate() {
            let sealed = seal_rtp(&c, packet, 10 + n as u32).unwrap();
            assert!(sealed.len() <= 1500);
            let rtp = open_rtp(&c, &sealed).expect("the viewer's code opens what we seal");
            for a in assembler.push(rtp, now) {
                if let Assembled::Frame(f) = a {
                    got.push(f);
                }
            }
        }
        assert_eq!(got, vec![frame]);
    }

    #[test]
    fn keyframes_get_parameter_sets_and_delimiters_go() {
        let mut sets = ParamSets::default();
        let first = annex_b(&[&[0x09, 0xf0], &[0x67, 1], &[0x68, 2], &[0x65, 3]]);
        assert_eq!(nal_kinds(&sets.prepare(&first)), vec![7, 8, 5], "AUD dropped, sets kept");
        // A keyframe the encoder sent without them: the remembered ones come first.
        let bare = annex_b(&[&[0x65, 4]]);
        assert_eq!(nal_kinds(&sets.prepare(&bare)), vec![7, 8, 5]);
        // A delta frame is left alone.
        assert_eq!(nal_kinds(&sets.prepare(&annex_b(&[&[0x41, 5]]))), vec![1]);
    }

    fn nal_kinds(frame: &[u8]) -> Vec<u8> {
        split_nals(frame).iter().map(|n| n[0] & 0x1f).collect()
    }

    #[test]
    fn retransmission_wraps_the_original() {
        let original = rtp_packets(H264_PT, 5, 700, 1234, &[vec![9, 8, 7]]).remove(0);
        let rtx = rtx_packet(&original, 3, 6).unwrap();
        assert_eq!(rtx[1], 0x80 | RTX_PT);
        assert_eq!(&rtx[2..4], &3u16.to_be_bytes());
        assert_eq!(&rtx[4..8], &1234u32.to_be_bytes());
        assert_eq!(&rtx[8..12], &6u32.to_be_bytes());
        assert_eq!(&rtx[12..], &[2, 188, 9, 8, 7], "original sequence number 700, then the payload");
        assert!(rtx_packet(&[0; 5], 1, 1).is_none());
    }

    #[test]
    fn rtcp_goes_both_ways_through_the_cipher() {
        let c = cipher();
        let report = sender_report(1, 1_700_000_000_000, 5, 6, 7);
        let sealed = seal_rtcp(&c, &report, 42).unwrap();
        assert_eq!(open_rtcp(&c, &sealed).unwrap(), report);
        assert!(open_rtcp(&c, &sealed[..20]).is_none());
        let mut broken = sealed;
        broken[10] ^= 1;
        assert!(open_rtcp(&c, &broken).is_none());
    }

    #[test]
    fn sender_report_layout() {
        let p = sender_report(0x01020304, 1_000, 99, 3, 4);
        assert_eq!(p.len(), 28);
        assert_eq!(&p[..4], &[0x80, 200, 0, 6]);
        assert_eq!(u32::from_be_bytes([p[8], p[9], p[10], p[11]]), 2_208_988_801);
        assert_eq!(&p[16..20], &99u32.to_be_bytes());
    }

    #[test]
    fn reads_remb_pli_and_nack_from_one_compound_packet() {
        // The viewer's code builds exactly these for the server; we read them back.
        let mut compound = crate::golive::pli(1, 2);
        let nack = {
            // 65535 lost, plus the two after it, and 40.
            let mut p = vec![0x81, 205, 0, 4];
            p.extend_from_slice(&1u32.to_be_bytes());
            p.extend_from_slice(&2u32.to_be_bytes());
            p.extend_from_slice(&[0xff, 0xff, 0, 0b11]);
            p.extend_from_slice(&[0, 40, 0, 0]);
            p
        };
        compound.extend_from_slice(&nack);
        // REMB for 4_000_000 bps: exp 5, mantissa 125000.
        let mut remb = vec![0x8f, 206, 0, 5];
        remb.extend_from_slice(&1u32.to_be_bytes());
        remb.extend_from_slice(&[0; 4]);
        remb.extend_from_slice(b"REMB");
        remb.extend_from_slice(&[1, (5 << 2) | ((125_000u32 >> 16) as u8), (125_000u32 >> 8) as u8, 125_000u32 as u8]);
        remb.extend_from_slice(&2u32.to_be_bytes());
        compound.extend_from_slice(&remb);
        assert_eq!(
            parse_rtcp(&compound),
            vec![Feedback::Keyframe, Feedback::Nack(vec![65535, 0, 1, 40]), Feedback::Remb(125_000 << 5)]
        );
        assert!(parse_rtcp(&[1, 2]).is_empty());
    }

    #[test]
    fn bitrate_follows_the_estimate_within_limits() {
        assert_eq!(adapt_bitrate(10_000_000, 8_000_000), 8_000_000);
        assert_eq!(adapt_bitrate(4_000_000, 8_000_000), 4_000_000);
        assert_eq!(adapt_bitrate(9_000_000, 12_000_000), 9_000_000, "the server's word is the limit");
        assert_eq!(adapt_bitrate(100_000, 8_000_000), 500_000);
        assert_eq!(adapt_bitrate(1, 300_000), 300_000, "a low ceiling wins over the floor");
    }

    #[test]
    fn video_time_runs_at_90_khz_and_wraps() {
        assert_eq!(video_timestamp(1000, 5_000_000, 5_000_000), 1000);
        assert_eq!(video_timestamp(1000, 5_000_000, 6_000_000), 1000 + 90_000);
        assert_eq!(video_timestamp(u32::MAX, 0, 100), 8);
        assert_eq!(video_timestamp(7, 10, 0), 7, "never backwards");
    }

    #[test]
    fn nitro_limits_cut_the_picker_choice() {
        assert_eq!(cap_quality(1440, 60, 0), (720, 30));
        assert_eq!(cap_quality(0, 60, 0), (720, 30), "as the source is not free");
        assert_eq!(cap_quality(720, 15, 0), (720, 15));
        assert_eq!(cap_quality(1440, 60, 1), (1080, 60));
        assert_eq!(cap_quality(0, 60, 1), (1080, 60));
        assert_eq!(cap_quality(1440, 60, 2), (1440, 60));
        assert_eq!(cap_quality(0, 60, 2), (0, 60));
        assert_eq!(cap_quality(1080, 60, 3), (720, 30), "Nitro Basic streams like no Nitro");
    }

    #[test]
    fn share_to_round_trips_and_defaults_to_discord() {
        assert_eq!(ShareTo::DEFAULT, ShareTo::Discord);
        for i in 0..3 {
            assert_eq!(ShareTo::from_index(i).unwrap().index() as i64, i);
        }
        assert!(ShareTo::from_index(3).is_none());
        assert!(ShareTo::Both.room() && ShareTo::Both.discord());
        assert!(!ShareTo::Discord.room() && ShareTo::Room.room() && !ShareTo::Room.discord());
    }

    #[test]
    fn gateway_messages() {
        let place = Place::Guild { guild: "1".into(), channel: "2".into() };
        assert_eq!(place.stream_key("3"), "guild:1:2:3");
        assert_eq!(stream_create(&place), json!({ "op": 18, "d": { "type": "guild", "guild_id": "1", "channel_id": "2", "preferred_region": null } }));
        let call = Place::Call { channel: "9".into() };
        assert_eq!(call.stream_key("3"), "call:9:3");
        assert_eq!(stream_create(&call)["d"], json!({ "type": "call", "channel_id": "9", "preferred_region": null }));
        assert_eq!(stream_delete("k"), json!({ "op": 19, "d": { "stream_key": "k" } }));
        assert_eq!(stream_paused("k", false)["d"], json!({ "stream_key": "k", "paused": false }));
    }

    #[test]
    fn media_server_handshake_messages() {
        let id = identify("55", "3", "sess", "tok");
        assert_eq!(id["server_id"], "55");
        assert_eq!(id["user_id"], "3");
        assert_eq!(id["session_id"], "sess");
        assert_eq!(id["token"], "tok");
        assert_eq!(id["video"], true);
        assert_eq!(id["streams"], json!([{ "type": "screen", "rid": "100", "quality": 100 }]));
        assert_eq!(id["max_dave_protocol_version"], 1);

        let sel = select_protocol("1.2.3.4", 5000, "aead_aes256_gcm_rtpsize");
        assert_eq!(sel["protocol"], "udp");
        assert_eq!(sel["data"], json!({ "address": "1.2.3.4", "port": 5000, "mode": "aead_aes256_gcm_rtpsize" }));
        let h264 = &sel["codecs"][1];
        assert_eq!((h264["name"].as_str(), h264["payload_type"].as_u64(), h264["rtx_payload_type"].as_u64()), (Some("H264"), Some(101), Some(102)));
        assert_eq!((h264["encode"].as_bool(), h264["decode"].as_bool()), (Some(true), Some(false)));
        assert_eq!(sel["codecs"][0]["payload_type"], 120);
    }

    #[test]
    fn ready_names_the_streams_ssrcs_or_falls_back_to_discords_habit() {
        let named = json!({ "ssrc": 10, "streams": [{ "type": "screen", "rid": "100", "ssrc": 20, "rtx_ssrc": 21 }] });
        assert_eq!(Ssrcs::from_ready(&named), Ssrcs { audio: 10, video: 20, rtx: 21 });
        assert_eq!(Ssrcs::from_ready(&json!({ "ssrc": 10 })), Ssrcs { audio: 10, video: 11, rtx: 12 });
    }

    #[test]
    fn video_declaration_and_speaking() {
        let params = Params { width: 1280, height: 720, fps: 30, max_bitrate: 5_000_000, sound: true };
        let d = video_declare(Ssrcs { audio: 10, video: 20, rtx: 21 }, &params);
        assert_eq!((d["audio_ssrc"].as_u64(), d["video_ssrc"].as_u64(), d["rtx_ssrc"].as_u64()), (Some(10), Some(20), Some(21)));
        let s = &d["streams"][0];
        assert_eq!((s["type"].as_str(), s["rid"].as_str(), s["quality"].as_u64(), s["active"].as_bool()), (Some("screen"), Some("100"), Some(100), Some(true)));
        assert_eq!((s["max_bitrate"].as_u64(), s["max_framerate"].as_u64()), (Some(5_000_000), Some(30)));
        assert_eq!(s["max_resolution"], json!({ "type": "fixed", "width": 1280, "height": 720 }));
        let silent = video_declare(Ssrcs { audio: 10, video: 20, rtx: 21 }, &Params { sound: false, ..params });
        assert_eq!(silent["audio_ssrc"], 0);
        assert_eq!(speaking(10, true), json!({ "speaking": 2, "delay": 0, "ssrc": 10 }));
    }

    #[test]
    fn stream_events_for_other_keys_are_ignored_and_ours_connect_once_complete() {
        let (gateway, mut sent) = unbounded_channel();
        let controls = Controls { keyframe: Arc::new(|| {}), bitrate: Arc::new(|_| {}) };
        let params = Params { width: 1280, height: 720, fps: 30, max_bitrate: 5_000_000, sound: false };
        let place = Place::Guild { guild: "1".into(), channel: "2".into() };
        let mut stream = Stream::start(gateway, &place, "3", "sess", params, controls, Arc::new(|_| {}));
        assert_eq!(sent.try_recv().unwrap()["op"], 18);
        assert!(!stream.on_event("STREAM_DELETE", &json!({ "stream_key": "guild:1:2:999" })));
        assert!(stream.rtc_server_id.is_none());
        assert!(!stream.on_event("STREAM_CREATE", &json!({ "stream_key": "guild:1:2:3", "rtc_server_id": "77" })));
        assert_eq!(stream.rtc_server_id.as_deref(), Some("77"));
        assert_eq!(sent.try_recv().unwrap()["op"], 22);
        assert!(stream.on_event("STREAM_DELETE", &json!({ "stream_key": "guild:1:2:3", "reason": "stream_ended" })));
        drop(stream);
        let stop = sent.try_recv().unwrap();
        assert_eq!(stop, stream_delete("guild:1:2:3"));
    }
}
