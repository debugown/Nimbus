//! Nimbus patch: a side door for camera video on the voice connection.
//!
//! Songbird only speaks voice. Discord carries camera video on the same voice
//! connection (the same websocket, UDP socket, transport key and DAVE group),
//! so a client that wants to send or receive it needs a way in. A
//! [`VideoLink`] set on the [`Config`] makes the driver:
//!
//! * identify with `video: true` and one video stream, and offer H.264
//!   (payload type 101, RTX 102) next to Opus in select protocol;
//! * hand over the voice gateway's JSON messages that matter for video
//!   (READY, clients connecting and leaving, op 12 VIDEO, flags);
//! * hand over every UDP packet that is not Opus voice (video, its
//!   retransmissions) still transport-encrypted, and a copy of every RTCP
//!   packet;
//! * tell what is needed to send on the same connection once it is up
//!   ([`Transport`]): the socket, the transport key, the DAVE session, and a
//!   nonce start in the half of the counter space the voice sender does not use;
//! * send JSON given to [`VideoLink::send`] on the voice gateway.
//!
//! [`Config`]: crate::Config

use serde_json::Value;
use std::{
    fmt,
    net::UdpSocket,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
        RwLock,
    },
};

/// The RTP payload types offered for H.264 video and its retransmissions.
pub const H264_PAYLOAD_TYPE: u8 = 101;
/// See [`H264_PAYLOAD_TYPE`].
pub const RTX_PAYLOAD_TYPE: u8 = 102;

/// Voice gateway opcodes handed over as they come (READY, clients connect,
/// video, client disconnect, client flags, client platform).
const SHARED_OPS: [u64; 6] = [2, 11, 12, 13, 18, 20];

/// How long the voice gateway took to answer the last heartbeat, in ms; 0 when not known.
pub static LAST_PING_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the driver tells the owner of a [`VideoLink`].
pub enum VideoEvent {
    /// The voice connection is up: what sending on it needs.
    Connected(Transport),
    /// A voice gateway message that matters for video, as JSON.
    Gateway(Value),
    /// A UDP packet that is not Opus voice, or a copy of an RTCP packet; still
    /// transport-encrypted.
    Packet(Vec<u8>),
    /// The voice connection with this (audio) SSRC is gone. A new one may
    /// already be up: its `Connected` can come first.
    Disconnected {
        /// The gone connection's audio SSRC.
        ssrc: u32,
    },
}

/// The voice connection's means, for sending video on it.
pub struct Transport {
    /// Our audio SSRC.
    pub ssrc: u32,
    /// The agreed transport encryption, as Discord names it.
    pub mode: &'static str,
    /// The transport key.
    pub secret_key: Vec<u8>,
    /// Where video's packet counter (the nonce) starts: half the counter space
    /// away from the voice sender's, so the two never use the same nonce.
    pub nonce_start: u32,
    /// The voice connection's UDP socket (a second handle to it).
    pub udp: UdpSocket,
    /// The DAVE end-to-end encryption shared with the voice.
    pub dave: DaveHandle,
}

/// A handle to the voice connection's DAVE session.
#[derive(Clone)]
pub struct DaveHandle {
    session: Arc<RwLock<Option<davey::DaveSession>>>,
    version: Arc<AtomicU16>,
}

impl DaveHandle {
    pub(crate) fn new(session: Arc<RwLock<Option<davey::DaveSession>>>, version: Arc<AtomicU16>) -> Self {
        Self { session, version }
    }

    /// The DAVE protocol version in force (0: transport encryption only).
    #[must_use]
    pub fn version(&self) -> u16 {
        self.version.load(Ordering::Relaxed)
    }

    /// Runs `f` with the session, if there is one.
    pub fn with_session<R>(&self, f: impl FnOnce(&mut davey::DaveSession) -> R) -> Option<R> {
        let mut session = self.session.write().unwrap_or_else(|e| e.into_inner());
        session.as_mut().map(f)
    }
}

struct Inner {
    on_event: Box<dyn Fn(VideoEvent) + Send + Sync>,
    outbox: (flume::Sender<Value>, flume::Receiver<Value>),
}

/// Lets video share the driver's voice connection; see the module docs.
#[derive(Clone)]
pub struct VideoLink(Arc<Inner>);

impl fmt::Debug for VideoLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VideoLink")
    }
}

impl VideoLink {
    /// `on_event` is called from the driver's own tasks; it should only queue.
    pub fn new(on_event: impl Fn(VideoEvent) + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner { on_event: Box::new(on_event), outbox: flume::unbounded() }))
    }

    /// Sends a JSON message (`{"op": .., "d": ..}`) on the voice gateway.
    /// False when the link is gone.
    pub fn send(&self, message: Value) -> bool {
        self.0.outbox.0.send(message).is_ok()
    }

    pub(crate) fn emit(&self, event: VideoEvent) {
        (self.0.on_event)(event);
    }

    pub(crate) fn outbox(&self) -> flume::Receiver<Value> {
        self.0.outbox.1.clone()
    }

    /// A text message from the voice gateway: handed over when it matters for video.
    pub(crate) fn tap_text(&self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else { return };
        if value["op"].as_u64().is_some_and(|op| SHARED_OPS.contains(&op)) {
            self.emit(VideoEvent::Gateway(value));
        }
    }

    /// A UDP packet from the voice server. Returns true when it was video and
    /// Songbird should not look at it; RTCP is copied and left to Songbird too.
    pub(crate) fn tap_udp(&self, packet: &[u8]) -> bool {
        match classify(packet) {
            Kind::Video => {
                self.emit(VideoEvent::Packet(packet.to_vec()));
                true
            },
            Kind::Rtcp => {
                self.emit(VideoEvent::Packet(packet.to_vec()));
                false
            },
            Kind::Other => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Video,
    Rtcp,
    Other,
}

/// RTP of another payload type than Opus (120) is video; payload types
/// 72..=79 after the marker bit is masked off are RTCP (200..=207).
fn classify(packet: &[u8]) -> Kind {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return Kind::Other;
    }
    match packet[1] & 0x7f {
        72..=79 => Kind::Rtcp,
        120 => Kind::Other,
        _ => Kind::Video,
    }
}

/// The video counter's start: the voice counter's plus half the space.
pub(crate) fn nonce_start(voice: u32) -> u32 {
    voice.wrapping_add(1 << 31)
}

/// Identify asking for video: one camera stream.
pub(crate) fn identify(server_id: u64, user_id: u64, session_id: &str, token: &str, max_dave: u16) -> Value {
    serde_json::json!({
        "op": 0,
        "d": {
            "server_id": server_id.to_string(),
            "user_id": user_id.to_string(),
            "session_id": session_id,
            "token": token,
            "video": true,
            "streams": [{ "type": "video", "rid": "100", "quality": 100 }],
            "max_dave_protocol_version": max_dave,
        }
    })
}

/// Select protocol offering H.264 next to Opus.
pub(crate) fn select_protocol(address: &str, port: u16, mode: &str) -> Value {
    serde_json::json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": { "address": address, "port": port, "mode": mode },
            "codecs": [
                { "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 },
                { "name": "H264", "type": "video", "priority": 1000, "payload_type": H264_PAYLOAD_TYPE, "rtx_payload_type": RTX_PAYLOAD_TYPE, "encode": true, "decode": true },
            ],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_sorted_by_payload_type() {
        let mut p = [0u8; 20];
        p[0] = 0x80;
        p[1] = 120;
        assert_eq!(classify(&p), Kind::Other);
        p[1] = 0x80 | H264_PAYLOAD_TYPE;
        assert_eq!(classify(&p), Kind::Video);
        p[1] = RTX_PAYLOAD_TYPE;
        assert_eq!(classify(&p), Kind::Video);
        p[1] = 201;
        assert_eq!(classify(&p), Kind::Rtcp);
        assert_eq!(classify(&p[..8]), Kind::Other);
    }

    #[test]
    fn video_nonces_start_half_way_round() {
        assert_eq!(nonce_start(5), 5 + (1 << 31));
        assert_eq!(nonce_start(u32::MAX), (1 << 31) - 1);
    }
}
