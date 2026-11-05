//! Discord REST + gateway, trimmed to what the prototype shows.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const API: &str = "https://discord.com/api/v10";
const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
// Honest UA: we are an unofficial client and do not pretend otherwise.
const USER_AGENT: &str = concat!("Nimbus-Native/", env!("CARGO_PKG_VERSION"), " (unofficial Discord client)");

pub type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Discord IDs are numeric snowflakes; anything else must not reach a URL path.
pub fn snowflake(id: &str) -> Result<&str> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(format!("invalid id: {id}"))
    }
}

pub enum Event {
    Ready(Value),
    MessageCreate(Value),
    VoiceState(Value),
    /// Where our own Discord voice call is served (after we ask to join).
    VoiceServer(Value),
    /// STREAM_CREATE / STREAM_SERVER_UPDATE / STREAM_DELETE (Go Live), by event name.
    Stream(String, Value),
    /// Changes on the DM side: DMs created/updated/deleted, friends and
    /// requests, presences, READY_SUPPLEMENTAL and our notification choices; by event name.
    Home(String, Value),
    /// Changes inside chats: typing, edits, deletions, reactions, soundboard sounds played in our call;
    /// by event name.
    Chat(String, Value),
    Status(String),
}

#[derive(Clone)]
pub struct Api {
    pub http: reqwest::Client,
    token: String,
}

/// Whether a URL points at this computer or a home or office network. Pictures named in messages
/// are never fetched from there, and a redirect never leads there.
pub fn is_internal_url(url: &reqwest::Url) -> bool {
    use std::net::IpAddr;
    let Some(host) = url.host_str() else { return true };
    let host = host.trim_matches(['[', ']']).to_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            let o = ip.octets();
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || (o[0] == 100 && (64..=127).contains(&o[1]))
        }
        Ok(IpAddr::V6(ip)) => {
            let s = ip.segments();
            ip.is_loopback() || ip.is_unspecified() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

impl Api {
    pub fn new(token: String) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            // A redirect may lead anywhere; never to this computer or a home network, and not forever.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if is_internal_url(attempt.url()) {
                    attempt.error("redirect to an internal address")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .expect("failed to build HTTP client");
        Self { http, token }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.request_from(method, path, body, None).await
    }

    /// `context`: the X-Context-Properties header Discord expects on friend
    /// actions (base64 JSON naming where in the client they came from).
    async fn request_from(&self, method: reqwest::Method, path: &str, body: Option<Value>, context: Option<&str>) -> Result<Value> {
        // Reads are tried again when Discord is busy or the connection hiccups; anything that
        // changes something is sent once (a second try could do it twice).
        let attempts = if method == reqwest::Method::GET { 3 } else { 1 };
        let mut last = String::new();
        for attempt in 0..attempts {
            match self.send_once(method.clone(), path, body.clone(), context).await {
                Ok(v) => return Ok(v),
                Err((true, wait, e)) if attempt + 1 < attempts => {
                    last = e;
                    tokio::time::sleep(wait).await;
                }
                Err((_, _, e)) => return Err(e),
            }
        }
        Err(last)
    }

    /// One try. On failure: whether trying again may help, how long to wait first, and the text.
    async fn send_once(&self, method: reqwest::Method, path: &str, body: Option<Value>, context: Option<&str>) -> std::result::Result<Value, (bool, Duration, String)> {
        let mut req = self.http.request(method, format!("{API}{path}")).header("Authorization", &self.token);
        if let Some(context) = context {
            req = req.header("X-Context-Properties", context);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.map_err(|e| (true, Duration::from_secs(1), err(e)))?;
        let status = res.status();
        let retry_header = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        let text = res.text().await.map_err(|e| (true, Duration::from_secs(1), err(e)))?;
        if !status.is_success() {
            if status.as_u16() == 401 {
                AUTH_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let wait = retry_wait(status.as_u16(), retry_header, &text);
            return Err((status.as_u16() == 429 || status.is_server_error(), wait, format!("Discord API {status}: {text}")));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| (false, Duration::ZERO, err(e)))
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.request(reqwest::Method::PATCH, path, Some(body)).await
    }

    /// Sends a friend request by username.
    pub async fn add_friend(&self, username: &str) -> Result<Value> {
        let body = serde_json::json!({ "username": username, "discriminator": null });
        self.request_from(reqwest::Method::POST, "/users/@me/relationships", Some(body), Some(CONTEXT_ADD_FRIEND)).await
    }

    /// Accepts an incoming friend request.
    pub async fn accept_friend(&self, user_id: &str) -> Result<Value> {
        let path = format!("/users/@me/relationships/{user_id}");
        self.request_from(reqwest::Method::PUT, &path, Some(serde_json::json!({})), Some(CONTEXT_FRIENDS)).await
    }

    /// Adds or takes back our reaction. `emoji` is already escaped for the path.
    pub async fn react(&self, channel: &str, message: &str, emoji: &str, add: bool) -> Result<Value> {
        let path = format!("/channels/{}/messages/{}/reactions/{emoji}/@me", snowflake(channel)?, snowflake(message)?);
        if add {
            self.request(reqwest::Method::PUT, &path, Some(json!({}))).await
        } else {
            self.request(reqwest::Method::DELETE, &path, None).await
        }
    }

    /// Our vote in a poll: the answers chosen, or none to take it back. Sent once, on a click.
    pub async fn poll_vote(&self, channel: &str, message: &str, answers: &[u64]) -> Result<Value> {
        let (path, body) = poll_vote_request(channel, message, answers)?;
        self.request(reqwest::Method::PUT, &path, Some(body)).await
    }

    /// Sends a file (with an optional text) to a channel. `sent` counts the bytes that left,
    /// for a progress line; `limit` is the biggest file the account may send.
    pub async fn upload(&self, channel: &str, path: &std::path::Path, text: &str, sent: std::sync::Arc<std::sync::atomic::AtomicU64>, limit: u64) -> Result<Value> {
        use futures_util::TryStreamExt;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let size = tokio::fs::metadata(path).await.map_err(err)?.len();
        if size > limit {
            return Err(crate::i18n::tf("файл «{}» весит {}, а можно не больше {}", &[&name, &crate::human_size(size), &crate::human_size(limit)]));
        }
        let bytes = bytes::Bytes::from(tokio::fs::read(path).await.map_err(err)?);
        // The body goes out in pieces, each counted as it's taken, so the progress is real.
        let pieces: Vec<bytes::Bytes> = (0..bytes.len()).step_by(UPLOAD_PIECE).map(|a| bytes.slice(a..(a + UPLOAD_PIECE).min(bytes.len()))).collect();
        let stream = futures_util::stream::iter(pieces.into_iter().map(Ok::<_, std::io::Error>)).inspect_ok(move |piece| {
            sent.fetch_add(piece.len() as u64, std::sync::atomic::Ordering::Relaxed);
        });
        let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), size).file_name(name.clone());
        let form = reqwest::multipart::Form::new()
            .text("payload_json", json!({ "content": text, "attachments": [{ "id": 0, "filename": name }] }).to_string())
            .part("files[0]", part);
        let url = format!("{API}/channels/{}/messages", snowflake(channel)?);
        let res = self.http.post(url).header("Authorization", &self.token).multipart(form).send().await.map_err(err)?;
        let status = res.status();
        let body = res.text().await.map_err(err)?;
        if !status.is_success() {
            return Err(format!("Discord API {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(err)
    }

    /// Sends files already in memory (pasted ones) with the text, in one message. Sent once.
    pub async fn upload_files(&self, channel: &str, files: &[(String, Vec<u8>)], text: &str) -> Result<Value> {
        let attachments: Vec<Value> = files.iter().enumerate().map(|(i, (name, _))| json!({ "id": i, "filename": name })).collect();
        let mut form = reqwest::multipart::Form::new().text("payload_json", json!({ "content": text, "attachments": attachments }).to_string());
        for (i, (name, bytes)) in files.iter().enumerate() {
            form = form.part(format!("files[{i}]"), reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone()));
        }
        let url = format!("{API}/channels/{}/messages", snowflake(channel)?);
        let res = self.http.post(url).header("Authorization", &self.token).multipart(form).send().await.map_err(err)?;
        let status = res.status();
        let body = res.text().await.map_err(err)?;
        if !status.is_success() {
            return Err(format!("Discord API {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(err)
    }

    /// PUT with an empty body, and DELETE (for "interested" in a scheduled event, and the like).
    pub async fn put(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::PUT, path, Some(json!({}))).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::DELETE, path, None).await
    }

    /// The biggest file this account may send: what Nitro gives it, or what the server's boosts allow, whichever is more.
    /// `premium_type` is the account's (0 none, 1 Classic, 2 Nitro, 3 Basic), `tier` the server's boost level.
    pub fn upload_limit(premium_type: u64, tier: u64) -> u64 {
        const MB: u64 = 1024 * 1024;
        let account = match premium_type {
            2 => 500 * MB,
            1 | 3 => 50 * MB,
            _ => 10 * MB,
        };
        let server = match tier {
            3 => 100 * MB,
            2 => 50 * MB,
            _ => 10 * MB,
        };
        account.max(server)
    }
    /// Pins or unpins a message.
    pub async fn pin(&self, channel: &str, message: &str, pinned: bool) -> Result<Value> {
        let path = format!("/channels/{}/pins/{}", snowflake(channel)?, snowflake(message)?);
        if pinned {
            self.request(reqwest::Method::PUT, &path, Some(json!({}))).await
        } else {
            self.request(reqwest::Method::DELETE, &path, None).await
        }
    }

    /// Changes the text of one of our messages.
    pub async fn edit_message(&self, channel: &str, message: &str, text: &str) -> Result<Value> {
        let path = format!("/channels/{}/messages/{}", snowflake(channel)?, snowflake(message)?);
        self.request(reqwest::Method::PATCH, &path, Some(json!({ "content": text }))).await
    }

    pub async fn delete_message(&self, channel: &str, message: &str) -> Result<Value> {
        let path = format!("/channels/{}/messages/{}", snowflake(channel)?, snowflake(message)?);
        self.request(reqwest::Method::DELETE, &path, None).await
    }

    /// Shows "… печатает" to the others for ten seconds.
    pub async fn typing(&self, channel: &str) -> Result<Value> {
        self.post(&format!("/channels/{}/typing", snowflake(channel)?), json!({})).await
    }

    /// Rings the others of a DM or group for the call we just joined there. Sent once, only
    /// when the user pressed the call button; never repeated.
    pub async fn ring(&self, channel: &str) -> Result<Value> {
        let (path, body) = ring_request(channel)?;
        self.post(&path, body).await
    }

    /// Sends a friend request to someone by id (from their menu).
    pub async fn request_friend(&self, user_id: &str) -> Result<Value> {
        let path = format!("/users/@me/relationships/{}", snowflake(user_id)?);
        self.request_from(reqwest::Method::PUT, &path, Some(json!({})), Some(CONTEXT_USER_MENU)).await
    }

    /// Server mute / deafen, move to another voice channel (`channel_id`), or
    /// disconnect (`channel_id: null`): a change to someone's member entry.
    pub async fn edit_member(&self, guild: &str, user: &str, change: Value) -> Result<Value> {
        let path = format!("/guilds/{}/members/{}", snowflake(guild)?, snowflake(user)?);
        self.request(reqwest::Method::PATCH, &path, Some(change)).await
    }

    pub async fn kick(&self, guild: &str, user: &str) -> Result<Value> {
        let path = format!("/guilds/{}/members/{}", snowflake(guild)?, snowflake(user)?);
        self.request(reqwest::Method::DELETE, &path, None).await
    }

    pub async fn ban(&self, guild: &str, user: &str) -> Result<Value> {
        let path = format!("/guilds/{}/bans/{}", snowflake(guild)?, snowflake(user)?);
        self.request(reqwest::Method::PUT, &path, Some(json!({ "delete_message_seconds": 0 }))).await
    }

    /// Forwards a message to another chat. Sent once, on a click.
    pub async fn forward(&self, dest: &str, message: &str, channel: &str, guild: Option<&str>) -> Result<Value> {
        let (path, body) = crate::forward::request(dest, message, channel, guild)?;
        self.post(&path, body).await
    }

    /// Our note about someone; none yet (Discord answers 404) is an empty one.
    pub async fn note(&self, user: &str) -> Result<String> {
        match self.get(&crate::notes::path(user)?).await {
            Ok(answer) => Ok(crate::notes::of(&answer)),
            Err(e) if e.starts_with("Discord API 404") => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    pub async fn set_note(&self, user: &str, text: &str) -> Result<Value> {
        self.request(reqwest::Method::PUT, &crate::notes::path(user)?, Some(crate::notes::body(text))).await
    }

    /// A server's (or one of its channels') notification choice; Discord answers with the server's whole entry.
    pub async fn guild_settings(&self, guild: &str, channel: Option<&str>, change: crate::notify::Change) -> Result<Value> {
        let (path, body) = crate::notify::settings_request(guild, channel, change)?;
        self.patch(&path, body).await
    }

    /// Blocks someone; `remove_friend` undoes it.
    pub async fn block(&self, user: &str) -> Result<Value> {
        let path = format!("/users/@me/relationships/{}", snowflake(user)?);
        self.request_from(reqwest::Method::PUT, &path, Some(json!({ "type": 2 })), Some(CONTEXT_USER_MENU)).await
    }

    /// "Игнорировать": their messages fold away, nothing tells them.
    pub async fn ignore(&self, user: &str, on: bool) -> Result<Value> {
        let path = format!("/users/@me/relationships/{}/ignore", snowflake(user)?);
        if on {
            self.put(&path).await
        } else {
            self.delete(&path).await
        }
    }

    /// Our own name for a friend; empty takes it away.
    pub async fn friend_nick(&self, user: &str, name: &str) -> Result<Value> {
        let path = format!("/users/@me/relationships/{}", snowflake(user)?);
        self.patch(&path, json!({ "nickname": if name.is_empty() { Value::Null } else { Value::from(name) } })).await
    }

    /// "Слушать вместе": their track from their place in it, on our own Spotify, through the account linked in Discord.
    /// The key Discord gives for that is used for this one request and goes nowhere else.
    pub async fn spotify_play(&self, track: &str, position_ms: i64) -> Result<()> {
        let links = self.get("/users/@me/connections").await?;
        let link = links.as_array().into_iter().flatten().find(|c| c["type"].as_str() == Some("spotify")).and_then(|c| c["id"].as_str()).map(String::from);
        let link = link.filter(|l| !l.is_empty() && !l.contains('/') && !l.contains('?')).ok_or_else(|| crate::i18n::t("В Discord не подключён Spotify"))?;
        let answer = self.get(&format!("/users/@me/connections/spotify/{link}/access-token")).await?;
        let key = answer["access_token"].as_str().ok_or_else(|| crate::i18n::t("Spotify не дал доступ"))?;
        let body = json!({ "uris": [format!("spotify:track:{track}")], "position_ms": position_ms.max(0) });
        let res = self.http.put("https://api.spotify.com/v1/me/player/play").bearer_auth(key).json(&body).send().await.map_err(|_| crate::i18n::t("Spotify не отвечает"))?;
        match res.status().as_u16() {
            200..=299 => Ok(()),
            404 => Err(crate::i18n::t("Откройте Spotify на любом устройстве")),
            403 => Err(crate::i18n::t("Для этого нужен Spotify Premium")),
            code => Err(crate::i18n::tf("Spotify ответил {}", &[&code])),
        }
    }

    /// Declines a request, cancels ours, or unfriends.
    pub async fn remove_friend(&self, user_id: &str) -> Result<Value> {
        let path = format!("/users/@me/relationships/{user_id}");
        self.request_from(reqwest::Method::DELETE, &path, None, Some(CONTEXT_FRIENDS)).await
    }

    /// Joins the server of an invite whose preview was shown. Sent once, on the join click.
    pub async fn join_invite(&self, preview: &crate::invite::Preview) -> Result<Value> {
        let (path, body) = crate::invite::join_request(&preview.code)?;
        let context = crate::invite::join_context(preview);
        self.request_from(reqwest::Method::POST, &path, Some(body), Some(&context)).await
    }

    /// A new invite to a channel (a day long, any number of uses).
    pub async fn create_invite(&self, channel: &str) -> Result<Value> {
        let (path, body) = crate::invite::create_request(channel)?;
        self.post(&path, body).await
    }

    /// Leaves a server. Discord refuses its owner.
    pub async fn leave_guild(&self, guild: &str) -> Result<Value> {
        self.request(reqwest::Method::DELETE, &format!("/users/@me/guilds/{guild}"), Some(json!({ "lease": false }))).await
    }

    /// A thread started from a message; Discord answers with the thread.
    pub async fn create_thread(&self, channel: &str, message: &str, name: &str) -> Result<Value> {
        let (path, body) = crate::forum::thread_request(channel, message, name)?;
        self.post(&path, body).await
    }

    /// A new post in a forum: its title and first message; Discord answers with the post (a thread).
    pub async fn create_post(&self, forum: &str, title: &str, text: &str) -> Result<Value> {
        let (path, body) = crate::forum::post_request(forum, title, text)?;
        self.post(&path, body).await
    }

    /// Asks where a file of `size` bytes may be put before it is attached to a message.
    pub async fn upload_slot(&self, channel: &str, filename: &str, size: u64) -> Result<Value> {
        let (path, body) = crate::voicemsg::slot_request(channel, filename, size)?;
        self.post(&path, body).await
    }

    /// Puts a file where `upload_slot` said, once. The address is Discord's storage, which gets
    /// the file and nothing else (no sign-in).
    pub async fn put_upload(&self, url: &str, bytes: Vec<u8>) -> Result<()> {
        let url = crate::voicemsg::upload_url(url)?;
        let res = self.http.put(url).header("Content-Type", "audio/ogg").body(bytes).send().await.map_err(err)?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(format!("Discord API {status}: {}", text.chars().take(300).collect::<String>()));
        }
        Ok(())
    }
}

/// What Discord said when it refused, in its own words ("Unknown Invite", "Must be between 1 and 100 in
/// length."); a plain explanation when it said nothing readable.
pub fn refusal(error: &str) -> String {
    let said = error
        .strip_prefix("Discord API ")
        .and_then(|rest| rest.split_once(": "))
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok());
    let Some(said) = said else { return friendly(error) };
    let message = said["message"].as_str().unwrap_or("").to_string();
    // A form error names the field's trouble deeper down: the first one is enough.
    let detail = first_error(&said["errors"]);
    match (message.is_empty(), detail) {
        (true, None) => friendly(error),
        (true, Some(d)) => d,
        (false, Some(d)) => format!("{message}: {d}"),
        (false, None) => message,
    }
}

fn first_error(v: &Value) -> Option<String> {
    if let Some(list) = v["_errors"].as_array() {
        return list.iter().find_map(|e| e["message"].as_str().map(String::from));
    }
    v.as_object()?.values().find_map(first_error)
}

/// The path and body of a ring: `recipients: null` rings everyone in the DM or group.
pub fn ring_request(channel: &str) -> Result<(String, Value)> {
    Ok((format!("/channels/{}/call/ring", snowflake(channel)?), json!({ "recipients": null })))
}

/// The path and body of a poll vote.
pub fn poll_vote_request(channel: &str, message: &str, answers: &[u64]) -> Result<(String, Value)> {
    Ok((format!("/channels/{}/polls/{}/answers/@me", snowflake(channel)?, snowflake(message)?), crate::poll::vote_body(answers)))
}

/// {"location":"Add Friend"} and {"location":"Friends"}, base64.
const CONTEXT_ADD_FRIEND: &str = "eyJsb2NhdGlvbiI6IkFkZCBGcmllbmQifQ==";
const CONTEXT_FRIENDS: &str = "eyJsb2NhdGlvbiI6IkZyaWVuZHMifQ==";
/// The size of one piece of an uploaded file.
const UPLOAD_PIECE: usize = 64 * 1024;

/// {"location":"ContextMenu"}, base64.
const CONTEXT_USER_MENU: &str = "eyJsb2NhdGlvbiI6IkNvbnRleHRNZW51In0=";

/// Keeps a gateway session alive, forwards the events we care about and
/// sends what `outgoing` brings (voice state updates).
pub async fn run_gateway(token: String, events: UnboundedSender<Event>, mut outgoing: UnboundedReceiver<Value>) {
    loop {
        if let Err(e) = session(&token, &events, &mut outgoing).await {
            crate::log_line(&format!("gateway: {e}"));
            crate::log_line(&format!("gateway: no connection: {e}"));
            let _ = events.send(Event::Status(crate::i18n::tf("Нет связи: {}", &[&e])));
        }
        // A token Discord refuses won't work on the next try either: the app goes back to the sign-in screen.
        if events.is_closed() || AUTH_LOST.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// What others see of us: the status and the custom status line, as the gateway takes them. Kept here so that
/// a new session starts with it (IDENTIFY) and every change is sent with it (op 3).
pub static PRESENCE: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

pub fn presence(status: &str, custom: &str) -> Value {
    let activities = if custom.is_empty() { json!([]) } else { json!([{ "type": 4, "name": "Custom Status", "state": custom }]) };
    json!({ "status": status, "since": 0, "activities": activities, "afk": false })
}

async fn session(token: &str, events: &UnboundedSender<Event>, outgoing: &mut UnboundedReceiver<Value>) -> Result<()> {
    let ws = connect_gateway().await?;
    let (mut tx, mut rx) = ws.split();

    let hello = loop {
        match rx.next().await.ok_or("closed before hello")?.map_err(err)? {
            Message::Text(text) => break serde_json::from_str::<Value>(&text).map_err(err)?,
            _ => continue,
        }
    };
    let interval = hello["d"]["heartbeat_interval"].as_u64().ok_or("no heartbeat interval")?;

    let identify = json!({
        "op": 2,
        "d": {
            "token": token,
            "properties": { "os": std::env::consts::OS, "browser": "Nimbus", "device": "Nimbus" },
            "presence": PRESENCE.lock().unwrap().clone().unwrap_or_else(|| presence("online", "")),
            "compress": false,
        }
    });
    tx.send(Message::Text(identify.to_string())).await.map_err(err)?;

    let mut seq = Value::Null;
    let mut beat_sent: Option<std::time::Instant> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(interval));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                beat_sent = Some(std::time::Instant::now());
                tx.send(Message::Text(json!({ "op": 1, "d": seq }).to_string())).await.map_err(err)?;
            }
            Some(payload) = outgoing.recv() => {
                tx.send(Message::Text(payload.to_string())).await.map_err(err)?;
            }
            msg = rx.next() => {
                let text = match msg.ok_or("connection closed")?.map_err(err)? {
                    Message::Text(text) => text,
                    Message::Close(frame) => {
                        // 4004: Discord doesn't accept the token any more (signed out everywhere, password changed).
                        if frame.as_ref().is_some_and(|f| u16::from(f.code) == 4004) {
                            AUTH_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        return Err(format!("closed by server: {frame:?}"));
                    }
                    _ => continue,
                };
                let mut payload: Value = serde_json::from_str(&text).map_err(err)?;
                if !payload["s"].is_null() {
                    seq = payload["s"].clone();
                }
                match payload["op"].as_u64() {
                    Some(0) => {
                        let data = payload["d"].take();
                        let event = match payload["t"].as_str() {
                            Some("READY") => Some(Event::Ready(data)),
                            Some("MESSAGE_CREATE") => Some(Event::MessageCreate(data)),
                            Some("VOICE_STATE_UPDATE") => Some(Event::VoiceState(data)),
                            Some("VOICE_SERVER_UPDATE") => Some(Event::VoiceServer(data)),
                            Some(t @ ("STREAM_CREATE" | "STREAM_SERVER_UPDATE" | "STREAM_UPDATE" | "STREAM_DELETE")) => Some(Event::Stream(t.to_string(), data)),
                            Some(
                                t @ ("READY_SUPPLEMENTAL" | "PRESENCE_UPDATE" | "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE"
                                | "GUILD_MEMBER_UPDATE" | "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE"
                                | "SESSIONS_REPLACE" | "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" | "RELATIONSHIP_REMOVE" | "MESSAGE_ACK" | "CALL_CREATE" | "CALL_UPDATE" | "CALL_DELETE"
                                | "USER_GUILD_SETTINGS_UPDATE" | "CHANNEL_RECIPIENT_ADD" | "CHANNEL_RECIPIENT_REMOVE" | "GUILD_CREATE" | "GUILD_DELETE"),
                            ) => Some(Event::Home(t.to_string(), data)),
                            Some(
                                t @ ("TYPING_START" | "MESSAGE_UPDATE" | "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" | "MESSAGE_REACTION_ADD"
                                | "MESSAGE_REACTION_REMOVE" | "MESSAGE_REACTION_REMOVE_ALL" | "MESSAGE_REACTION_REMOVE_EMOJI"
                                | "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" | "GUILD_MEMBER_LIST_UPDATE" | "VOICE_CHANNEL_EFFECT_SEND"
                                | "INTERACTION_SUCCESS" | "INTERACTION_FAILURE" | "GUILD_SCHEDULED_EVENT_CREATE" | "GUILD_SCHEDULED_EVENT_UPDATE"
                                | "GUILD_SCHEDULED_EVENT_DELETE" | "GUILD_SCHEDULED_EVENT_USER_ADD" | "GUILD_SCHEDULED_EVENT_USER_REMOVE"),
                            ) => Some(Event::Chat(t.to_string(), data)),
                            _ => None,
                        };
                        if let Some(event) = event {
                            if events.send(event).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Some(1) => {
                        tx.send(Message::Text(json!({ "op": 1, "d": seq }).to_string())).await.map_err(err)?;
                    }
                    // The answer to our heartbeat: how long the round trip to Discord took.
                    Some(11) => {
                        if let Some(sent) = beat_sent.take() {
                            GATEWAY_PING_MS.store(sent.elapsed().as_millis().min(u32::MAX as u128) as u32, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    Some(7) => return Err("server requested reconnect".into()),
                    Some(9) => return Err("invalid session".into()),
                    _ => {}
                }
            }
        }
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the gateway: through the proxy this PC is set to use (as a browser would), then directly if that fails.
/// The first connection to Discord that worked only through a proxy was the pages and pictures; the gateway
/// is a different kind of connection, which has to be told about the proxy by hand.
async fn connect_gateway() -> Result<Socket> {
    if let Some(proxy) = system_proxy() {
        match tokio::time::timeout(Duration::from_secs(15), connect_via_proxy(&proxy)).await {
            Ok(Ok(ws)) => {
                crate::log_line(&format!("gateway: connected through the proxy {proxy}"));
                return Ok(ws);
            }
            Ok(Err(e)) => crate::log_line(&format!("gateway: the proxy {proxy} did not work ({e}); trying directly")),
            Err(_) => crate::log_line(&format!("gateway: the proxy {proxy} did not answer in time; trying directly")),
        }
    }
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), connect_async(GATEWAY))
        .await
        .map_err(|_| "Discord did not answer in 20 s (blocked network?)".to_string())?
        .map_err(err)?;
    Ok(ws)
}

async fn connect_via_proxy(proxy: &str) -> Result<Socket> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(proxy).await.map_err(err)?;
    let target = "gateway.discord.gg:443";
    tcp.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.map_err(err)?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || tcp.read(&mut byte).await.map_err(err)? == 0 {
            return Err("the proxy closed the connection".into());
        }
        head.push(byte[0]);
    }
    let first = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string();
    if !first.split_whitespace().nth(1).is_some_and(|code| code == "200") {
        return Err(format!("the proxy answered: {first}"));
    }
    let (ws, _) = tokio_tungstenite::client_async_tls(GATEWAY, tcp).await.map_err(err)?;
    Ok(ws)
}

/// "host:port" of the proxy for secure sites: from the usual environment variables, else from Windows' internet settings (read only).
fn system_proxy() -> Option<String> {
    for name in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Some(found) = std::env::var(name).ok().and_then(|v| proxy_address(&v)) {
            return Some(found);
        }
    }
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let enabled = registry_value(key, "ProxyEnable")?;
    if !(enabled.ends_with("0x1") || enabled == "1") {
        return None;
    }
    proxy_from_settings(&registry_value(key, "ProxyServer")?)
}

fn registry_value(key: &str, name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg").args(["query", key, "/v", name]).creation_flags(0x0800_0000).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let line = text.lines().find(|l| l.trim_start().starts_with(name))?;
    Some(line.split_whitespace().skip(2).collect::<Vec<_>>().join(" "))
}

/// "host:port" from "http://host:port", "host:port" and the like; none for SOCKS proxies.
fn proxy_address(v: &str) -> Option<String> {
    let v = v.trim();
    if v.starts_with("socks") {
        return None;
    }
    let v = v.strip_prefix("http://").or_else(|| v.strip_prefix("https://")).unwrap_or(v);
    let v = v.rsplit('@').next().unwrap_or(v).trim_end_matches('/');
    (v.contains(':') && !v.is_empty()).then(|| v.to_string())
}

/// Windows lists either one address for everything or "http=a:1;https=b:2;socks=c:3".
fn proxy_from_settings(server: &str) -> Option<String> {
    if !server.contains('=') {
        return proxy_address(server);
    }
    let pick = |scheme: &str| server.split(';').find_map(|part| part.trim().strip_prefix(scheme)?.strip_prefix('=').and_then(proxy_address));
    pick("https").or_else(|| pick("http"))
}

/// Set when Discord answers 401 or closes the gateway with 4004: the saved token may have been
/// revoked (signed out everywhere). The app checks it once more, then asks for a new sign-in.
pub static AUTH_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The last round trip to Discord's gateway, in milliseconds (0 until the first answer).
pub static GATEWAY_PING_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How long to wait before asking again: what Discord says, kept between half a second and five.
fn retry_wait(status: u16, header: Option<f64>, body: &str) -> Duration {
    let said = header.or_else(|| serde_json::from_str::<Value>(body).ok().and_then(|v| v["retry_after"].as_f64()));
    let seconds = match (status, said) {
        (_, Some(s)) => s,
        (429, None) => 2.0,
        _ => 1.0,
    };
    Duration::from_secs_f64(seconds.clamp(0.5, 5.0))
}

/// What a failed request means for the person, in words, not "Discord API 403: {...}".
pub fn friendly(error: &str) -> String {
    let code = error.strip_prefix("Discord API ").and_then(|rest| rest.split_whitespace().next()).and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(401) => crate::i18n::t("Вход устарел: войди в Nimbus заново"),
        Some(403) => crate::i18n::t("Нет доступа к этому действию"),
        Some(404) => crate::i18n::t("Не найдено: возможно, уже удалено"),
        Some(429) => crate::i18n::t("Слишком часто, подожди секунду и повтори"),
        Some(c) if c >= 500 => crate::i18n::t("Discord сейчас не отвечает, попробуй позже"),
        Some(400) => crate::i18n::t("Discord не принял запрос"),
        Some(_) => crate::i18n::t("Не получилось: Discord отказал"),
        None if error.contains("error sending request") || error.contains("dns") || error.contains("timed out") || error.contains("connect") => {
            crate::i18n::t("Нет связи с Discord: проверь интернет")
        }
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_addresses_are_recognised() {
        let internal = |s: &str| is_internal_url(&reqwest::Url::parse(s).unwrap());
        for s in ["http://localhost/a", "http://127.0.0.1:7890/", "http://[::1]/", "http://192.168.1.5/x", "http://10.0.0.1/", "http://169.254.1.1/", "http://100.64.0.9/", "http://printer.local/"] {
            assert!(internal(s), "{s}");
        }
        for s in ["https://cdn.discordapp.com/a.png", "https://8.8.8.8/", "https://media.tenor.com/x.gif"] {
            assert!(!internal(s), "{s}");
        }
    }

    #[test]
    fn proxy_addresses_are_read() {
        assert_eq!(proxy_address("http://127.0.0.1:7890"), Some("127.0.0.1:7890".into()));
        assert_eq!(proxy_address("127.0.0.1:8080/"), Some("127.0.0.1:8080".into()));
        assert_eq!(proxy_address("http://user:pass@proxy.local:3128"), Some("proxy.local:3128".into()));
        assert_eq!(proxy_address("socks5://127.0.0.1:1080"), None);
        assert_eq!(proxy_address(""), None);
        assert_eq!(proxy_from_settings("127.0.0.1:10809"), Some("127.0.0.1:10809".into()));
        assert_eq!(proxy_from_settings("http=a:1;https=b:2;socks=c:3"), Some("b:2".into()));
        assert_eq!(proxy_from_settings("http=a:1;socks=c:3"), Some("a:1".into()));
        assert_eq!(proxy_from_settings("socks=c:3"), None);
    }

    #[test]
    fn errors_in_words() {
        assert_eq!(friendly("Discord API 403 Forbidden: {\"code\":50013}"), "Нет доступа к этому действию");
        assert_eq!(friendly("Discord API 404 Not Found: {}"), "Не найдено: возможно, уже удалено");
        assert_eq!(friendly("Discord API 502 Bad Gateway: "), "Discord сейчас не отвечает, попробуй позже");
        assert_eq!(friendly("error sending request for url (https://x)"), "Нет связи с Discord: проверь интернет");
        assert_eq!(friendly("канал не выбран"), "канал не выбран");
    }

    #[test]
    fn refusals_keep_discords_words() {
        assert_eq!(refusal(r#"Discord API 404 Not Found: {"message": "Unknown Invite", "code": 10006}"#), "Unknown Invite");
        let form = r#"Discord API 400 Bad Request: {"message": "Invalid Form Body", "code": 50035, "errors": {"name": {"_errors": [{"code": "BASE_TYPE_BAD_LENGTH", "message": "Must be between 1 and 100 in length."}]}}}"#;
        assert_eq!(refusal(form), "Invalid Form Body: Must be between 1 and 100 in length.");
        assert_eq!(refusal("Discord API 502 Bad Gateway: <html>"), "Discord сейчас не отвечает, попробуй позже");
        assert_eq!(refusal("канал не выбран"), "канал не выбран");
    }

    #[test]
    fn upload_limits_follow_nitro_and_boosts() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(Api::upload_limit(0, 0), 10 * MB);
        assert_eq!(Api::upload_limit(0, 2), 50 * MB, "a boosted server");
        assert_eq!(Api::upload_limit(0, 3), 100 * MB);
        assert_eq!(Api::upload_limit(3, 3), 100 * MB, "the more of the two");
        assert_eq!(Api::upload_limit(2, 0), 500 * MB);
    }

    #[test]
    fn a_ring_goes_to_the_dm_and_rings_everyone() {
        let (path, body) = ring_request("123456789012345678").unwrap();
        assert_eq!(path, "/channels/123456789012345678/call/ring");
        assert_eq!(body, json!({ "recipients": null }));
        assert!(ring_request("12/../34").is_err());
        assert!(ring_request("").is_err());
    }

    #[test]
    fn a_poll_vote_goes_to_the_message_and_checks_ids() {
        let (path, body) = poll_vote_request("111", "222", &[3]).unwrap();
        assert_eq!(path, "/channels/111/polls/222/answers/@me");
        assert_eq!(body, json!({ "answer_ids": ["3"] }));
        assert!(poll_vote_request("111", "2/../2", &[]).is_err());
        assert!(poll_vote_request("", "222", &[]).is_err());
    }

    #[test]
    fn waits_are_bounded() {
        assert_eq!(retry_wait(429, Some(0.01), ""), Duration::from_millis(500));
        assert_eq!(retry_wait(429, None, "{\"retry_after\": 30.0}"), Duration::from_secs(5));
        assert_eq!(retry_wait(429, None, ""), Duration::from_secs(2));
        assert_eq!(retry_wait(503, None, ""), Duration::from_secs(1));
    }
}