//! Cameras in a Discord call: ours out, other people's in, on the same voice
//! connection as the sound (discord_voice.rs).
//!
//! Songbird does the voice; its Nimbus patch (vendor/songbird, driver/video.rs)
//! lets this module share the connection: the voice gateway (identify asks for
//! one video stream, READY gives its SSRCs, op 12 says who sends video on which
//! SSRC), the UDP socket and transport key, and the DAVE group.
//!
//! Sending: op 12 declares our camera stream (or that it is off), the GPU
//! encoder's H.264 frames go out DAVE-encrypted and packetised as Go Live does
//! it (golive_send.rs), RTCP asks for keyframes, sets the bitrate and gets lost
//! packets resent. Receiving: frames are put together and decrypted as when a
//! Go Live stream is watched (golive.rs), and decoded on the GPU (h264dec.rs),
//! only while the call screen is shown.
//!
//! Like the rest of the Discord side this uses the user's account from an
//! unofficial client; Discord may act on that. What Discord's own client puts
//! in op 12 for a camera is followed as far as it is known; only a real call
//! can confirm it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::{Aes256Gcm, KeyInit};
use serde_json::{json, Value};
use songbird::driver::video::{DaveHandle, Transport, VideoEvent, VideoLink};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::golive::{self, open_rtp, rand_counter, seal_rtcp, Assembled, Assembler, H264_PT, RTX_PT};
use crate::golive_send::{self, Controls, Feedback, FrameSink, ParamSets, Ssrcs};
use crate::Pixels;

/// Shows (Some) or clears (None) the camera picture of a user (Discord id).
pub type Display = Arc<dyn Fn(String, Option<Pixels>) + Send + Sync>;

/// The ceiling Discord's own client uses for a 720p camera.
pub const MAX_BITRATE: u64 = 2_500_000;
/// What the encoder starts with, before the server's estimate.
pub const START_BITRATE: u64 = 1_500_000;

/// Our camera as it goes out.
#[derive(Clone)]
pub struct Outgoing {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub controls: Controls,
}

enum Input {
    Link(VideoEvent),
    Frame { data: Vec<u8>, key: bool, time_us: i64 },
    Send(Option<Outgoing>),
    Code(u64, tokio::sync::oneshot::Sender<Option<String>>),
}

/// Video on one Discord call. Dropping it ends it.
pub struct Video {
    tx: UnboundedSender<Input>,
    task: JoinHandle<()>,
}

impl Video {
    /// Starts the video side of a call; the link goes into the voice driver's config.
    /// Call inside the tokio runtime.
    pub fn start(me: &str, display: Display) -> (Self, VideoLink) {
        let (tx, rx) = unbounded_channel();
        let events = tx.clone();
        let link = VideoLink::new(move |event| {
            let _ = events.send(Input::Link(event));
        });
        let state = State::new(me, link.clone(), display);
        let task = tokio::spawn(run(state, rx));
        (Self { tx, task }, link)
    }

    /// Our camera on (what it sends) or off.
    pub fn send(&self, outgoing: Option<Outgoing>) {
        let _ = self.tx.send(Input::Send(outgoing));
    }

    /// The code both sides of the call compare to know nobody sits between them; none while the call isn't encrypted.
    pub fn verification_code(&self, user: u64) -> tokio::sync::oneshot::Receiver<Option<String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(Input::Code(user, tx));
        rx
    }

    /// For the encoder: its frames, until the call ends.
    pub fn frame_sink(&self) -> FrameSink {
        let tx = self.tx.clone();
        Box::new(move |data: &[u8], key: bool, time_us: i64| tx.send(Input::Frame { data: data.to_vec(), key, time_us }).is_ok())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------- gateway messages

/// Op 12 VIDEO for our camera: its stream on (with size, rate and bitrate ceiling)
/// or off. Discord's client says "off" with no video SSRC and the stream inactive.
pub fn camera_declare(ours: Ssrcs, out: Option<(u32, u32, u32)>) -> Value {
    let (on, (width, height, fps)) = (out.is_some(), out.unwrap_or((0, 0, 0)));
    json!({
        "op": 12,
        "d": {
            "audio_ssrc": ours.audio,
            "video_ssrc": if on { ours.video } else { 0 },
            "rtx_ssrc": if on { ours.rtx } else { 0 },
            "streams": [{
                "type": "video",
                "rid": "100",
                "ssrc": ours.video,
                "active": on,
                "quality": if on { 100 } else { 0 },
                "rtx_ssrc": ours.rtx,
                "max_bitrate": if on { MAX_BITRATE } else { 0 },
                "max_framerate": fps,
                "max_resolution": { "type": "fixed", "width": width, "height": height },
            }],
        }
    })
}

/// Op 15 MEDIA_SINK_WANTS: which of the cameras we want and how good (0 none, 100 full):
/// all of them while they can be seen, none otherwise. Only camera SSRCs are named (no
/// "any"), so the voice itself is never touched by it.
pub fn sink_wants(video_ssrcs: &[u32], shown: bool) -> Value {
    let quality = if shown { 100 } else { 0 };
    let mut wants = serde_json::Map::new();
    for ssrc in video_ssrcs {
        wants.insert(ssrc.to_string(), json!(quality));
    }
    json!({ "op": 15, "d": wants })
}

/// Someone's camera stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub user: String,
    pub rtx: u32,
}

/// Who sends camera video on which SSRC, from the voice gateway.
#[derive(Default)]
pub struct Remotes {
    by_video: HashMap<u32, Peer>,
}

/// What op 12 changed.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// Their camera is on, on this video SSRC.
    On { user: String, video: u32 },
    /// Their camera is off.
    Off { user: String },
    None,
}

impl Remotes {
    /// Op 12 from the server about someone else. Their camera stream is the active
    /// "video" stream (Go Live is never on this connection); without streams, the
    /// top-level SSRCs, where 0 means off.
    pub fn on_video(&mut self, d: &Value, me: &str) -> Change {
        let Some(user) = d["user_id"].as_str().filter(|u| *u != me) else { return Change::None };
        let stream = d["streams"].as_array().and_then(|s| s.iter().find(|s| s["type"].as_str().unwrap_or("video") == "video"));
        let (video, rtx, active) = match stream {
            Some(s) => (s["ssrc"].as_u64().unwrap_or(0) as u32, s["rtx_ssrc"].as_u64().unwrap_or(0) as u32, s["active"].as_bool().unwrap_or(true)),
            None => (d["video_ssrc"].as_u64().unwrap_or(0) as u32, d["rtx_ssrc"].as_u64().unwrap_or(0) as u32, true),
        };
        let on = active && video != 0 && d["video_ssrc"].as_u64() != Some(0);
        let had = self.forget(user);
        if on {
            self.by_video.insert(video, Peer { user: user.to_string(), rtx });
            Change::On { user: user.to_string(), video }
        } else if had {
            Change::Off { user: user.to_string() }
        } else {
            Change::None
        }
    }

    /// They left the call (op 13), or their camera stopped: true if they had one.
    pub fn forget(&mut self, user: &str) -> bool {
        let before = self.by_video.len();
        self.by_video.retain(|_, p| p.user != user);
        self.by_video.len() != before
    }

    /// The camera stream a packet belongs to: by its SSRC, or the one whose RTX it is.
    pub fn stream_of(&self, ssrc: u32) -> Option<(u32, &Peer, bool)> {
        if let Some(peer) = self.by_video.get(&ssrc) {
            return Some((ssrc, peer, false));
        }
        self.by_video.iter().find(|(_, p)| p.rtx == ssrc && ssrc != 0).map(|(&video, p)| (video, p, true))
    }

    pub fn video_ssrcs(&self) -> Vec<u32> {
        let mut all: Vec<u32> = self.by_video.keys().copied().collect();
        all.sort_unstable();
        all
    }

    pub fn users(&self) -> Vec<String> {
        self.by_video.values().map(|p| p.user.clone()).collect()
    }
}

/// The RTP packets (unsealed) of one H.264 frame on our camera stream: parameter sets
/// ahead of a keyframe, `protect` (DAVE) over the frame, then RFC 6184 packets with
/// consecutive sequence numbers from `seq` and the marker on the last.
pub fn frame_packets(
    sets: &mut ParamSets,
    frame: &[u8],
    ssrc: u32,
    seq: u16,
    timestamp: u32,
    protect: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Vec<Vec<u8>>, String> {
    let annex_b = sets.prepare(frame);
    let payload = protect(&annex_b)?;
    let payloads = golive_send::packetize_h264(&golive_send::split_nals(&payload), golive_send::MAX_PAYLOAD);
    Ok(golive_send::rtp_packets(H264_PT, ssrc, seq, timestamp, &payloads))
}

// ---------------------------------------------------------------- the connection

/// The voice connection's means, once it is up.
struct Net {
    cipher: Aes256Gcm,
    udp: std::net::UdpSocket,
    /// The nonce counter for everything video seals (RTP and RTCP).
    counter: u32,
    dave: DaveHandle,
    /// Our audio SSRC on this connection.
    ssrc: u32,
}

impl Net {
    fn from(t: Transport) -> Result<Self, String> {
        if t.mode != "aead_aes256_gcm_rtpsize" {
            return Err(format!("transport mode {} is not supported for video", t.mode));
        }
        let cipher = Aes256Gcm::new_from_slice(&t.secret_key).map_err(|_| "bad transport key".to_string())?;
        Ok(Self { cipher, udp: t.udp, counter: t.nonce_start, dave: t.dave, ssrc: t.ssrc })
    }

    fn send_rtp(&mut self, packet: &[u8]) -> bool {
        self.counter = self.counter.wrapping_add(1);
        golive_send::seal_rtp(&self.cipher, packet, self.counter).is_some_and(|sealed| self.udp.send(&sealed).is_ok())
    }

    fn send_rtcp(&mut self, rtcp: &[u8]) -> bool {
        self.counter = self.counter.wrapping_add(1);
        seal_rtcp(&self.cipher, rtcp, self.counter).is_some_and(|sealed| self.udp.send(&sealed).is_ok())
    }

    /// DAVE over a frame when the call uses it; Err while the group is not ready.
    fn protect(&self, frame: &[u8]) -> Result<Vec<u8>, String> {
        if self.dave.version() == 0 {
            return Ok(frame.to_vec());
        }
        self.dave
            .with_session(|s| {
                if !s.is_ready() {
                    return Err("DAVE not ready".to_string());
                }
                s.encrypt(davey::MediaType::VIDEO, davey::Codec::H264, frame).map(|c| c.into_owned()).map_err(|e| format!("{e:?}"))
            })
            .unwrap_or_else(|| Err("no DAVE session".into()))
    }

    fn unprotect(&self, user: u64, frame: &[u8]) -> Result<Vec<u8>, String> {
        if self.dave.version() == 0 {
            return Ok(frame.to_vec());
        }
        self.dave.with_session(|s| s.decrypt(user, davey::MediaType::VIDEO, frame).map_err(|e| format!("{e:?}"))).unwrap_or_else(|| Err("no DAVE session".into()))
    }
}

/// Our camera stream on the wire.
struct Sending {
    out: Outgoing,
    seq: u16,
    rtx_seq: u16,
    ts_base: u32,
    first_us: Option<i64>,
    last_ts: u32,
    sets: ParamSets,
    ring: VecDeque<(u16, Vec<u8>)>,
    /// A viewer needs a keyframe first; also after frames were dropped.
    started: bool,
    packets: u32,
    octets: u32,
    frames: u64,
    dropped: u64,
    last_keyframe_at: Option<Instant>,
    last_bitrate: u64,
    last_bitrate_at: Option<Instant>,
}

/// Retransmission memory, in packets.
const RING: usize = 512;

impl Sending {
    fn new(out: Outgoing) -> Self {
        let r = rand_counter();
        Self {
            out,
            seq: r as u16,
            rtx_seq: (r >> 16) as u16,
            ts_base: r.rotate_left(7),
            first_us: None,
            last_ts: 0,
            sets: ParamSets::default(),
            ring: VecDeque::new(),
            started: false,
            packets: 0,
            octets: 0,
            frames: 0,
            dropped: 0,
            last_keyframe_at: None,
            last_bitrate: 0,
            last_bitrate_at: None,
        }
    }

    fn keyframe(&mut self) {
        if self.last_keyframe_at.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
            self.last_keyframe_at = Some(Instant::now());
            (self.out.controls.keyframe)();
        }
    }
}

/// Someone's camera being received.
struct Incoming {
    user: u64,
    assembler: Assembler,
    /// Only while the call screen is shown.
    decoder: Option<crate::h264dec::Decoder>,
    highest_seq: Option<u32>,
    keyframes: u64,
    last_pli: Option<Instant>,
}

struct State {
    me: String,
    link: VideoLink,
    display: Display,
    net: Option<Net>,
    ours: Ssrcs,
    sending: Option<Sending>,
    remotes: Remotes,
    incoming: HashMap<u32, Incoming>,
    shown: bool,
    /// Kinds of op already logged once.
    seen: HashSet<u64>,
}

impl State {
    fn new(me: &str, link: VideoLink, display: Display) -> Self {
        Self {
            me: me.to_string(),
            link,
            display,
            net: None,
            ours: Ssrcs::default(),
            sending: None,
            remotes: Remotes::default(),
            incoming: HashMap::new(),
            shown: false,
            seen: HashSet::new(),
        }
    }

    fn on_link(&mut self, event: VideoEvent) {
        match event {
            VideoEvent::Connected(transport) => match Net::from(transport) {
                Ok(net) => {
                    crate::log_line(&format!("discord video: connected, audio ssrc {}, ours {:?}", net.ssrc, self.ours));
                    self.net = Some(net);
                    self.declare();
                    self.want();
                }
                Err(e) => crate::log_line(&format!("discord video: {e}")),
            },
            VideoEvent::Disconnected { ssrc } => {
                if self.net.as_ref().is_some_and(|n| n.ssrc == ssrc) {
                    crate::log_line("discord video: voice connection gone");
                    self.net = None;
                }
            }
            VideoEvent::Gateway(message) => self.on_gateway(&message),
            VideoEvent::Packet(packet) => self.on_packet(&packet),
        }
    }

    fn on_gateway(&mut self, message: &Value) {
        let op = message["op"].as_u64().unwrap_or(u64::MAX);
        let d = &message["d"];
        if self.seen.insert(op) {
            crate::log_line(&format!("discord video: op {op} {}", golive::short(d)));
        }
        match op {
            2 => self.ours = Ssrcs::from_ready(d),
            12 => match self.remotes.on_video(d, &self.me) {
                Change::On { user, video } => {
                    crate::log_line(&format!("discord video: {user} camera on, ssrc {video}"));
                    self.incoming.remove(&video);
                    self.want();
                }
                Change::Off { user } => self.camera_gone(&user),
                Change::None => {}
            },
            13 => {
                if let Some(user) = d["user_id"].as_str() {
                    if self.remotes.forget(user) {
                        self.camera_gone(user);
                    }
                }
            }
            _ => {}
        }
    }

    fn camera_gone(&mut self, user: &str) {
        crate::log_line(&format!("discord video: {user} camera off"));
        let id = user.parse::<u64>().unwrap_or(0);
        self.incoming.retain(|_, i| i.user != id);
        (self.display)(user.to_string(), None);
    }

    /// Tells the server what our camera does now.
    fn declare(&mut self) {
        if self.net.is_none() || self.ours.audio == 0 {
            return;
        }
        let out = self.sending.as_ref().map(|s| (s.out.width, s.out.height, s.out.fps));
        self.link.send(camera_declare(self.ours, out));
    }

    /// Tells the server which cameras to forward.
    fn want(&mut self) {
        let cameras = self.remotes.video_ssrcs();
        if self.net.is_some() && !cameras.is_empty() {
            self.link.send(sink_wants(&cameras, self.shown));
        }
    }

    fn on_send(&mut self, out: Option<Outgoing>) {
        match out {
            Some(out) => {
                crate::log_line(&format!("discord video: camera on, {}x{} @{}", out.width, out.height, out.fps));
                (out.controls.bitrate)(START_BITRATE);
                let mut sending = Sending::new(out);
                sending.keyframe();
                self.sending = Some(sending);
            }
            None => {
                if let Some(s) = self.sending.take() {
                    crate::log_line(&format!("discord video: camera off after {} frames ({} dropped)", s.frames, s.dropped));
                }
            }
        }
        self.declare();
    }

    fn on_frame(&mut self, data: Vec<u8>, key: bool, time_us: i64) {
        let (Some(net), Some(s)) = (self.net.as_mut(), self.sending.as_mut()) else { return };
        if self.ours.video == 0 {
            return;
        }
        if !s.started {
            if !key {
                s.keyframe();
                return;
            }
            s.started = true;
        }
        let first_us = *s.first_us.get_or_insert(time_us);
        let timestamp = golive_send::video_timestamp(s.ts_base, first_us, time_us);
        let packets = match frame_packets(&mut s.sets, &data, self.ours.video, s.seq, timestamp, |f| net.protect(f)) {
            Ok(packets) => packets,
            Err(e) => {
                // Frames after a missing one are useless to a viewer: start again at a keyframe.
                s.dropped += 1;
                s.started = false;
                if s.dropped == 1 {
                    crate::log_line(&format!("discord video: frame not sent: {e}"));
                }
                return;
            }
        };
        s.last_ts = timestamp;
        for packet in packets {
            let seq = s.seq;
            s.seq = s.seq.wrapping_add(1);
            if net.send_rtp(&packet) {
                s.packets = s.packets.wrapping_add(1);
                s.octets = s.octets.wrapping_add((packet.len() - 12) as u32);
            }
            if s.ring.len() >= RING {
                s.ring.pop_front();
            }
            s.ring.push_back((seq, packet));
        }
        s.frames += 1;
    }

    fn on_packet(&mut self, packet: &[u8]) {
        if packet.len() < 12 {
            return;
        }
        let pt = packet[1] & 0x7f;
        if (72..=79).contains(&pt) {
            self.on_rtcp(packet);
        } else if self.shown && (pt == H264_PT || pt == RTX_PT) {
            self.on_video(packet, pt == RTX_PT);
        }
    }

    /// RTCP from the server about what we send.
    fn on_rtcp(&mut self, packet: &[u8]) {
        let Some(net) = self.net.as_mut() else { return };
        let Some(plain) = golive_send::open_rtcp(&net.cipher, packet) else { return };
        if let Some(lost) = crate::voiceping::loss_in_report(&plain, net.ssrc) {
            crate::voiceping::note_loss(lost);
        }
        let Some(s) = self.sending.as_mut() else { return };
        for feedback in golive_send::parse_rtcp(&plain) {
            match feedback {
                Feedback::Keyframe => s.keyframe(),
                Feedback::Remb(bps) => {
                    let target = golive_send::adapt_bitrate(bps, MAX_BITRATE);
                    let moved = s.last_bitrate == 0 || target.abs_diff(s.last_bitrate) * 10 > s.last_bitrate;
                    if moved && s.last_bitrate_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                        (s.last_bitrate, s.last_bitrate_at) = (target, Some(Instant::now()));
                        (s.out.controls.bitrate)(target);
                    }
                }
                Feedback::Nack(missing) => {
                    for seq in missing.into_iter().take(100) {
                        let Some((_, original)) = s.ring.iter().find(|(n, _)| *n == seq) else { continue };
                        s.rtx_seq = s.rtx_seq.wrapping_add(1);
                        if let Some(rtx) = golive_send::rtx_packet(original, s.rtx_seq, self.ours.rtx) {
                            net.send_rtp(&rtx);
                        }
                    }
                }
            }
        }
    }

    /// A packet of someone's camera: put together, decrypted, decoded.
    fn on_video(&mut self, packet: &[u8], is_rtx: bool) {
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let Some((video, peer, _)) = self.remotes.stream_of(ssrc) else { return };
        let user = peer.user.clone();
        let Some(net) = self.net.as_mut() else { return };
        let Some(mut rtp) = open_rtp(&net.cipher, packet) else { return };
        let display = self.display.clone();
        let incoming = self.incoming.entry(video).or_insert_with(|| Incoming {
            user: user.parse().unwrap_or(0),
            assembler: Assembler::default(),
            decoder: None,
            highest_seq: None,
            keyframes: 0,
            last_pli: None,
        });
        if incoming.decoder.is_none() {
            let (show, who) = (display.clone(), user.clone());
            incoming.decoder = Some(crate::h264dec::Decoder::start(Box::new(move |pixels| show(who.clone(), Some(pixels)))));
        }
        if is_rtx {
            if rtp.payload.len() <= 2 {
                return; // a bandwidth probe
            }
            rtp.seq = u16::from_be_bytes([rtp.payload[0], rtp.payload[1]]);
            rtp.payload.drain(..2);
        } else {
            let missing = gap(incoming.highest_seq, rtp.seq);
            incoming.highest_seq = Some(extend_seq(incoming.highest_seq, rtp.seq));
            if let Some(nack) = (!missing.is_empty()).then(|| golive::nack(net.ssrc, video, &missing)).flatten() {
                net.send_rtcp(&nack);
            }
            if rtp.payload.is_empty() {
                return;
            }
        }
        let mut lost = false;
        for assembled in incoming.assembler.push(rtp, Instant::now()) {
            match assembled {
                Assembled::Frame(frame) => {
                    let key = golive::nal_types(&frame).contains(&5);
                    match net.unprotect(incoming.user, &frame) {
                        Ok(plain) => {
                            incoming.keyframes += key as u64;
                            if let Some(decoder) = &mut incoming.decoder {
                                decoder.push(plain, key);
                            }
                        }
                        Err(_) => lost = true,
                    }
                }
                Assembled::Lost => lost = true,
            }
        }
        if lost {
            if let Some(decoder) = &mut incoming.decoder {
                decoder.lost();
            }
            ask_keyframe(net, incoming, video);
        }
    }

    /// Every quarter second: whether the cameras can be seen changed, keyframes still
    /// missing, frames waiting for packets that never came.
    fn on_tick(&mut self) {
        if let Some(net) = &self.net {
            let encrypted = net.dave.version() > 0;
            let code = net.dave.with_session(|s| s.voice_privacy_code().map(String::from)).flatten();
            crate::voiceping::set_privacy(encrypted, code.as_deref());
        }
        let shown = crate::CALL_SHOWN.load(Ordering::Relaxed) && crate::WINDOW_SHOWN.load(Ordering::Relaxed);
        if shown != self.shown {
            self.shown = shown;
            if !shown {
                // Nobody looks: no decoding, and the server is asked to stop sending.
                self.incoming.clear();
            }
            self.want();
        }
        let Some(net) = self.net.as_mut() else { return };
        for (&video, incoming) in &mut self.incoming {
            if incoming.keyframes == 0 {
                ask_keyframe(net, incoming, video);
            }
            let lost = incoming.assembler.drain(Instant::now()).into_iter().any(|a| matches!(a, Assembled::Lost));
            if lost {
                if let Some(decoder) = &mut incoming.decoder {
                    decoder.lost();
                }
                ask_keyframe(net, incoming, video);
            }
        }
    }

    /// Every second: sender report for our camera, receiver feedback for theirs.
    fn on_report(&mut self) {
        let Some(net) = self.net.as_mut() else { return };
        if let Some(s) = &self.sending {
            let report = golive_send::sender_report(self.ours.video, golive::now_ms(), s.last_ts, s.packets, s.octets);
            net.send_rtcp(&report);
        }
        for (&video, incoming) in &self.incoming {
            let rtcp = golive::feedback(net.ssrc, video, incoming.highest_seq.unwrap_or(0), incoming.assembler.broken as u32, MAX_BITRATE);
            net.send_rtcp(&rtcp);
        }
    }
}

fn ask_keyframe(net: &mut Net, incoming: &mut Incoming, video: u32) {
    if incoming.last_pli.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
        return;
    }
    incoming.last_pli = Some(Instant::now());
    net.send_rtcp(&golive::pli(net.ssrc, video));
}

/// Sequence numbers skipped between the highest seen and `seq` (none when in order,
/// late, or too far off to repair).
fn gap(highest: Option<u32>, seq: u16) -> Vec<u16> {
    let Some(high) = highest else { return Vec::new() };
    let skipped = seq.wrapping_sub(high as u16).wrapping_sub(1);
    if skipped == 0 || skipped > 200 {
        return Vec::new();
    }
    (1..=skipped).map(|i| (high as u16).wrapping_add(i)).collect()
}

/// The highest sequence number, counted on past wraps.
fn extend_seq(highest: Option<u32>, seq: u16) -> u32 {
    match highest {
        None => seq as u32,
        Some(high) => {
            let delta = seq.wrapping_sub(high as u16) as i16;
            if delta > 0 { high.wrapping_add(delta as u32) } else { high }
        }
    }
}

async fn run(mut state: State, mut rx: UnboundedReceiver<Input>) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut reports = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            input = rx.recv() => match input {
                Some(Input::Link(event)) => state.on_link(event),
                Some(Input::Frame { data, key, time_us }) => state.on_frame(data, key, time_us),
                Some(Input::Send(out)) => state.on_send(out),
                Some(Input::Code(user, answer)) => {
                    let code = state.net.as_ref().and_then(|n| n.dave.with_session(|s| s.get_verification_code(user).ok())).flatten();
                    let _ = answer.send(code);
                }
                None => break,
            },
            _ = tick.tick() => state.on_tick(),
            _ = reports.tick() => state.on_report(),
        }
    }
    for user in state.remotes.users() {
        (state.display)(user, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours() -> Ssrcs {
        Ssrcs { audio: 100, video: 101, rtx: 102 }
    }

    #[test]
    fn camera_on_declares_the_stream() {
        let on = camera_declare(ours(), Some((1280, 720, 30)));
        assert_eq!(on["op"], 12);
        let d = &on["d"];
        assert_eq!((d["audio_ssrc"].as_u64(), d["video_ssrc"].as_u64(), d["rtx_ssrc"].as_u64()), (Some(100), Some(101), Some(102)));
        let s = &d["streams"][0];
        assert_eq!(s["type"], "video");
        assert_eq!(s["rid"], "100");
        assert_eq!(s["active"], true);
        assert_eq!(s["ssrc"], 101);
        assert_eq!(s["rtx_ssrc"], 102);
        assert_eq!(s["max_bitrate"].as_u64(), Some(MAX_BITRATE));
        assert_eq!(s["max_framerate"], 30);
        assert_eq!(s["max_resolution"], json!({ "type": "fixed", "width": 1280, "height": 720 }));
    }

    #[test]
    fn camera_off_declares_no_video() {
        let d = &camera_declare(ours(), None)["d"];
        assert_eq!(d["audio_ssrc"], 100);
        assert_eq!(d["video_ssrc"], 0);
        assert_eq!(d["rtx_ssrc"], 0);
        assert_eq!(d["streams"][0]["active"], false);
        assert_eq!(d["streams"][0]["quality"], 0);
    }

    #[test]
    fn sink_wants_follow_what_is_shown() {
        let shown = sink_wants(&[7, 9], true);
        assert_eq!(shown["op"], 15);
        assert_eq!(shown["d"], json!({ "7": 100, "9": 100 }));
        assert_eq!(sink_wants(&[7], false)["d"], json!({ "7": 0 }));
    }

    #[test]
    fn cameras_are_kept_by_ssrc() {
        let mut r = Remotes::default();
        let on = json!({ "user_id": "5", "audio_ssrc": 50, "video_ssrc": 51, "rtx_ssrc": 52,
            "streams": [{ "type": "video", "rid": "100", "ssrc": 51, "rtx_ssrc": 52, "active": true, "quality": 100 }] });
        assert_eq!(r.on_video(&on, "1"), Change::On { user: "5".into(), video: 51 });
        assert_eq!(r.stream_of(51).map(|(v, p, rtx)| (v, p.user.clone(), rtx)), Some((51, "5".into(), false)));
        assert_eq!(r.stream_of(52).map(|(v, _, rtx)| (v, rtx)), Some((51, true)), "the RTX SSRC leads to the stream");
        assert!(r.stream_of(50).is_none(), "sound is not video");
        // The same person again on new SSRCs: the old ones are gone.
        let moved = json!({ "user_id": "5", "audio_ssrc": 50, "video_ssrc": 61, "rtx_ssrc": 62 });
        assert_eq!(r.on_video(&moved, "1"), Change::On { user: "5".into(), video: 61 });
        assert!(r.stream_of(51).is_none());
        assert_eq!(r.video_ssrcs(), vec![61]);
        // Off: inactive stream, or no video SSRC.
        let off = json!({ "user_id": "5", "audio_ssrc": 50, "video_ssrc": 0, "streams": [{ "type": "video", "ssrc": 61, "active": false }] });
        assert_eq!(r.on_video(&off, "1"), Change::Off { user: "5".into() });
        assert_eq!(r.on_video(&off, "1"), Change::None, "already off");
        assert!(r.video_ssrcs().is_empty());
        // Our own op 12 echoed back is not someone's camera.
        assert_eq!(r.on_video(&json!({ "user_id": "1", "video_ssrc": 9 }), "1"), Change::None);
        // Leaving the call ends their camera.
        r.on_video(&on, "1");
        assert!(r.forget("5"));
        assert!(!r.forget("5"));
    }

    #[test]
    fn a_frame_goes_out_and_comes_back_whole() {
        let key = [3u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        // An SPS, a PPS and an IDR slice too big for one packet.
        let idr: Vec<u8> = std::iter::once(0x65).chain((0..3000).map(|i| (i % 251) as u8)).collect();
        let mut frame = Vec::new();
        for nal in [&[0x67, 0x42, 0xe0, 0x1f][..], &[0x68, 0xce, 0x3c, 0x80][..], &idr[..]] {
            frame.extend_from_slice(&[0, 0, 0, 1]);
            frame.extend_from_slice(nal);
        }
        let mut sets = ParamSets::default();
        let packets = frame_packets(&mut sets, &frame, 101, 65534, 9000, |f| Ok(f.to_vec())).unwrap();
        assert!(packets.len() >= 4, "the slice is cut into FU-A fragments");
        for (n, p) in packets.iter().enumerate() {
            assert_eq!(p[1] & 0x7f, H264_PT);
            assert_eq!(p[1] & 0x80 != 0, n + 1 == packets.len(), "marker on the last only");
            assert_eq!(u16::from_be_bytes([p[2], p[3]]), 65534u16.wrapping_add(n as u16));
            assert_eq!(&p[8..12], &101u32.to_be_bytes());
            assert!(p.len() <= 12 + golive_send::MAX_PAYLOAD);
        }
        // Sealed as on the wire, opened and put together as a receiver does (the last two swapped on the way).
        let mut assembler = Assembler::default();
        let mut got = Vec::new();
        let mut order: Vec<usize> = (0..packets.len()).collect();
        order.swap(packets.len() - 1, packets.len() - 2);
        for n in order {
            let sealed = golive_send::seal_rtp(&cipher, &packets[n], 7 + n as u32).unwrap();
            let rtp = open_rtp(&cipher, &sealed).expect("opens");
            got.extend(assembler.push(rtp, Instant::now()));
        }
        let frames: Vec<Vec<u8>> = got.into_iter().filter_map(|a| if let Assembled::Frame(f) = a { Some(f) } else { None }).collect();
        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn protection_failing_sends_nothing() {
        let mut sets = ParamSets::default();
        assert!(frame_packets(&mut sets, &[0, 0, 0, 1, 0x41, 1, 2], 1, 0, 0, |_| Err("not ready".into())).is_err());
    }

    #[test]
    fn gaps_are_asked_for_and_wraps_counted() {
        assert_eq!(gap(Some(10), 13), vec![11, 12]);
        assert!(gap(Some(10), 11).is_empty() && gap(Some(10), 9).is_empty() && gap(None, 5).is_empty());
        assert_eq!(gap(Some(65535), 1), vec![0]);
        assert_eq!(extend_seq(Some(65535), 2), 65538);
        assert_eq!(extend_seq(Some(70000), 4463), 70000, "a late one does not go back");
    }

    #[test]
    fn rtcp_sealed_by_us_opens_with_the_shared_key() {
        let cipher = Aes256Gcm::new_from_slice(&[1u8; 32]).unwrap();
        let pli = golive::pli(100, 51);
        let sealed = seal_rtcp(&cipher, &pli, 5).unwrap();
        assert_eq!(golive_send::open_rtcp(&cipher, &sealed), Some(pli));
    }
}
