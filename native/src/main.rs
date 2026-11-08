#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Nimbus native prototype: servers, channels and text chat, with no WebView.
//! Network work runs on a tokio thread; the Slint UI owns the main thread.
//! It signs in with the token the main Nimbus app saved in Windows Credential Manager.
//!
//! Lists go on screen as soon as their text is known; avatars, icons and
//! pictures load in the background and fill in their rows when they arrive.

mod audio;
mod badge;
mod camera;
mod chat;
mod clipboard;
mod netcheck;
mod fold;
mod folders;
mod clock;
mod components;
mod discord;
mod events;
mod emoji;
mod forward;
mod gifs;
mod discord_video;
mod discord_voice;
mod golive;
mod golive_send;
mod update;
mod weblogin;
mod h264dec;
mod forum;
mod home;
mod i18n;
mod invite;

/// Whether the window can be seen at all (not minimized, not hidden in the tray).
pub static WINDOW_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// The call screen is open: cameras in the call are only decoded then.
pub static CALL_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
mod thumbs;
mod members;
mod mentions;
mod slash;
mod soundboard;
mod names;
mod notes;
mod notify;
mod pages;
mod perms;
mod usermenu;
mod poll;
mod prefs;
mod system;
mod trust;
mod search;
mod servers;
mod unread;
mod userprofile;
mod voiceping;
mod gifcache;
mod changelog;
mod hwenc;
mod share;
mod voice;
mod voicemsg;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use discord::{snowflake, Api, Event};
use futures_util::stream::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel, Weak};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use unicode_normalization::UnicodeNormalization;

slint::include_modules!();

const CDN: &str = "https://cdn.discordapp.com";
const HOME: &str = "@me";
/// A guild without a `[nimbus-stream] <address>` channel topic uses a server on this PC.
const DEFAULT_SERVER: &str = "http://localhost:7890";
const GROUP_WINDOW_SECS: i64 = 5 * 60;
// Pictures are decoded at twice their on-screen size so they stay sharp on
// high-DPI displays. Animated GIFs use the on-screen size to save memory.
const AVATAR_PX: u32 = 80;
const DM_AVATAR_PX: u32 = 64;
const GUILD_ICON_PX: u32 = 96;
const PICTURE_MAX_W: f64 = 400.0;
const PICTURE_MAX_H: f64 = 300.0;
const MAX_GIF_FRAMES: usize = 150;
const MAX_GIF_BYTES: u64 = 15 * 1024 * 1024;
/// How long typing in the GIF panel must pause before a search goes out.
const GIF_PAUSE: Duration = Duration::from_millis(300);
/// How long a server's channels and our roles in it are kept without asking again.
const GUILD_CACHE_FOR: Duration = Duration::from_secs(10 * 60);
const THREADS_CACHE_FOR: Duration = Duration::from_secs(60);
/// The most a still picture (avatar, attachment preview, embed image) may weigh.
const MAX_STILL_BYTES: u64 = 25 * 1024 * 1024;

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// A voice channel as it was asked for, to ask for it again.
#[derive(Clone)]
struct LastJoin {
    guild_id: String,
    dm: bool,
    channel_id: String,
    channel_name: String,
    guild_name: String,
    server: String,
}

/// What the UI asks the network side to do.
enum Cmd {
    /// A new invite to a server, for the window with its friends.
    InviteToGuild(String),
    /// A server's invite link sent to a friend: them, the link.
    InviteFriend(String, String),
    SelectGuild(String),
    SelectChannel(String),
    Send(String),
    /// A token typed into the sign-in screen.
    Login(String),
    JoinVoice(String),
    /// Back into the voice channel another client of this account took over.
    VoiceReconnect,
    /// Into the call of the open DM or group: the call button (rings the others once), or
    /// answering the call already going on there (no ring).
    DmCall { ring: bool },
    /// The call line's "decline": only hides it here; Discord is told nothing.
    DmCallDecline,
    LeaveVoice,
    VoiceSwitches(bool, bool),
    Voice(voice::VoiceCmd),
    /// On the friends or requests page: open the DM (user id, or the request's channel id).
    PersonOpen(String),
    /// A privacy setting of the account, by name and value (see `privacy_body`).
    Privacy(String, i64),
    /// Who may send friend requests: everyone, friends of friends, members of shared servers.
    FriendSources(bool, bool, bool),
    /// The line under our name (empty removes it).
    CustomStatus(String),
    /// The "about me" of our profile.
    Bio(String),
    /// Our avatar or banner: "avatar", "banner" (both ask for a picture) or "banner-clear".
    ProfilePicture(String),
    /// The display name (empty goes back to the username) and the pronouns.
    ProfileName(String),
    ProfilePronouns(String),
    /// Accept an incoming friend request, or remove a friend request either way.
    PersonAccept(String),
    PersonRemove(String),
    FriendsTab(i32),
    /// Send a friend request to a username.
    AddFriend(String),
    /// "Find or start a conversation": the text typed, and the result picked.
    Search(String),
    SearchPick(usize),
    /// Something was typed into the message box.
    Typing,
    /// The member list was scrolled near its end.
    MoreMembers,
    /// The emoji panel's search box changed.
    EmojiQuery(String),
    /// The text of the message box changed (for @ and # suggestions).
    InputChanged(String),
    /// A suggestion picked from the list.
    PickSuggest(usize),
    /// A row of the slash command panel picked.
    SlashPick(usize),
    /// The soundboard: opened, its search typed into, a sound clicked (its id, its server or ""),
    /// others' sounds heard here or not.
    SoundboardOpen,
    SoundQuery(String),
    SoundPlay(String, String),
    SoundsMuted(bool),
    /// A reaction clicked: message id, reaction key.
    React(String, String),
    /// A poll's answer clicked: message id, answer id.
    PollVote(String, u64),
    /// A bot's button clicked: message id, custom id. A select menu opened, then its choice (the values picked).
    ComponentClick(String, String),
    SelectOpen(String, String),
    ComponentSelect(String, String, Vec<String>),
    /// Ctrl+V in the message box with pictures or files on the clipboard; a pending file's ✕.
    Paste(Vec<clipboard::Pasted>),
    PendingRemove(usize),
    /// Scheduled events of the open server: the window opened; "Интересно" switched; deleted; created.
    EventsOpen,
    EventCreateOpen,
    EventInterest(String, bool),
    EventDelete(String),
    EventCreate(events::Draft),
    /// A voice message's play button: message id.
    VoicePlay(String),
    /// Message menu: copy its text; put it in the box to edit; send the edited text.
    CopyMessage(String),
    /// A link to the message, to the clipboard.
    CopyLink(String),
    ShowPins,
    /// The arrows in the title bar.
    GoBack,
    GoAhead,
    /// The latest messages that mention us, in the found panel.
    ShowMentions,
    AttachFile,
    LoadNewer,
    ReturnLatest,
    /// A reply: the message answered, the text.
    Reply(String, String),
    BeginEdit(String),
    EditMessage(String, String),
    /// The chat was scrolled to the top: bring the page before.
    LoadOlder,
    /// Search the messages (of the server, or the DM).
    FindMessages(String),
    /// Open a search hit: its channel, its message.
    JumpTo(String, String),
    /// Right-click on someone (user id), in a server or not.
    UserMenu(String),
    /// Right-click on a row of the DM list (channel id).
    DmMenu(String),
    /// A person's whole profile, in its own window.
    FullProfile(String),
    /// A notification was clicked: the server ("" for a DM) and the channel.
    OpenChat(String, String),
    /// A person's card: who, and whether it is the small one over the chat (else the panel beside a DM).
    UserCard(String, bool),
    /// Written in a card's message box: to whom, what.
    CardMessage(String, String),
    /// The picture viewer wants this picture at full size.
    ViewImage(String),
    /// Save a picture: asks where.
    SaveImage(String),
    /// A choice in that menu: what, and for whom.
    UserAction(String, String),
    /// Their own volume for us (percent).
    UserVolume(String, u32),
    /// Our note about someone, as left in their menu: who, the text.
    SaveNote(String, String),
    /// The GIF panel: what's typed (after a pause), scrolled to the end, a GIF picked (its address).
    GifQuery(String),
    GifMore,
    SendGif(String),
    /// Message menu "Переслать": the message; then what's typed in the picker; then the place picked.
    ForwardOpen(String),
    ForwardQuery(String),
    ForwardTo(String),
    /// Right-click on a server (channel empty) or a channel: its notification menu; then a choice in it.
    NotifyMenu(String, String),
    NotifyChange(String, String, String),
    /// The join window: what was typed (its preview is asked for), then the join click on a previewed code.
    InviteLookup(String),
    InviteJoin(String),
    /// A channel menu's "Пригласить": a new invite to the channel.
    CreateInvite(String),
    /// A thread from a message (the message, the name); a forum post (the forum, title, text).
    CreateThread(String, String),
    CreatePost(String, String, String),
    /// The voice message button: start recording, stop and send, or throw it away.
    RecordStart,
    RecordSend,
    RecordCancel,
}

// ---------- Data for the UI thread ----------
// slint::Image can't cross threads, so rows travel as plain data and become
// Slint items inside the event loop.

#[derive(Default)]
struct Guild {
    id: String,
    name: String,
    badge: unread::Badge,
}

#[derive(Default)]
struct Channel {
    id: String,
    name: String,
    kind: i32,
    clickable: bool,
    has_avatar: bool,
    subtitle: String,
    status: i32,
    badge: i32,
    unread: bool,
    /// A thread under its channel.
    is_thread: bool,
    /// Muted (or in a muted server): drawn dimmed.
    muted: bool,
    /// A folded category's header, or a row under one.
    folded: bool,
}

/// Block kinds, matching `Block.kind` in app.slint.
const TEXT: i32 = 0;
const CODE: i32 = 1;
const QUOTE: i32 = 2;
const HEADING: i32 = 3;
const LINK: i32 = 4;
const SPOILER: i32 = 5;

/// A message's picture at full size: what the viewer loads, what "open" and "save" use.
#[derive(Clone, Default)]
struct PictureLinks {
    full: String,
    original: String,
}

struct Blk {
    kind: i32,
    text: String,
    url: String,
}

struct Msg {
    /// A line of the chat itself ("X added Y"), drawn instead of the text.
    system_line: Option<SystemLine>,
    picture_links: PictureLinks,
    id: String,
    author_id: String,
    author: String,
    time: String,
    short_time: String,
    blocks: Vec<Blk>,
    grouped: bool,
    system: bool,
    /// On-screen size of the picture, if the message has one.
    picture: Option<(f32, f32)>,
    edited: bool,
    reactions: Vec<chat::Reaction>,
    /// The message a search jumped to: it's marked on screen.
    highlight: bool,
    /// The author's name colour (RGB), from their highest role that has one.
    color: Option<u32>,
    /// Files and link previews under the text.
    cards: Vec<Card>,
    /// The message names us (or everyone): it's marked.
    mention: bool,
    pinned: bool,
    /// A message of emoji only: its emoji, drawn big.
    emojis: Vec<chat::Piece>,
    sticker: Option<Sticker>,
    poll: Option<poll::Poll>,
    voice: Option<voicemsg::Clip>,
    /// Forwarded from elsewhere: the blocks are the original's, shown as a quote.
    forwarded: bool,
    /// Codes of the invite links in the text, each drawn as a card.
    invites: Vec<String>,
    /// A bot's buttons and select menus, and the message they belong to (for the interaction a click sends).
    components: Vec<Vec<components::Component>>,
    target: components::Target,
    /// Only we can see it (a bot's ephemeral answer).
    ephemeral: bool,
}

/// An invite seen this session: asked about once; its preview (none when Discord knows no such
/// invite), its server's icon, and whether we are in that server.
#[derive(Clone, Default)]
struct SeenInvite {
    done: bool,
    preview: Option<invite::Preview>,
    icon: Option<Pixels>,
    joined: bool,
}

/// Invites by code; one request per code for the whole session.
static INVITES: std::sync::LazyLock<Mutex<HashMap<String, SeenInvite>>> = std::sync::LazyLock::new(Mutex::default);

/// An invite card as far as it is known.
fn invite_item(code: &str) -> InviteItem {
    let seen = INVITES.lock().unwrap().get(code).cloned().unwrap_or_default();
    let p = seen.preview.as_ref();
    let name = p.map(|p| p.name.clone()).unwrap_or_default();
    InviteItem {
        code: code.into(),
        guild: p.map(|p| p.guild_id.clone()).unwrap_or_default().into(),
        initials: initials(&name).into(),
        name: name.into(),
        has_icon: seen.icon.is_some(),
        icon: seen.icon.map(Image::from_rgba8).unwrap_or_default(),
        counts: p.map(invite::counts_line).unwrap_or_default().into(),
        state: match (seen.done, p.is_some()) {
            (false, _) => 0,
            (true, true) => 1,
            (true, false) => 2,
        },
        joined: seen.joined,
    }
}

/// Redraws the cards of one invite wherever it is on screen, and the join window if it shows it.
fn show_invite(ui: &AppWindow, code: &str) {
    let messages = ui.get_messages();
    for row in 0..messages.row_count() {
        let Some(m) = messages.row_data(row) else { continue };
        for i in 0..m.invites.row_count() {
            if m.invites.row_data(i).is_some_and(|v| v.code.as_str() == code) {
                m.invites.set_row_data(i, invite_item(code));
            }
        }
    }
    if ui.get_join_preview().code.as_str() == code {
        ui.set_join_preview(invite_item(code));
    }
}

/// A line under the message box saying what went wrong; it goes away by itself.
fn flash(ui: &Weak<AppWindow>, text: String) {
    let shown = text.clone();
    on_ui(ui, move |ui| ui.set_flash(shown.into()));
    let ui = ui.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        on_ui(&ui, move |ui| {
            if ui.get_flash().as_str() == text {
                ui.set_flash("".into());
            }
        });
    });
}

thread_local! {
    /// Custom emoji pictures (reactions, big emoji, poll answers) by emoji id, the current frame
    /// of animated ones; lives on the UI thread.
    static EMOJIS: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

const EMOJI_PX: u32 = 48;

/// A custom emoji's picture as drawn now; None for plain emoji and pictures not here yet.
fn emoji_image(key: &str) -> Option<Image> {
    let id = chat::emoji_id(key)?;
    EMOJIS.with(|e| e.borrow().get(id).cloned())
}

fn emoji_pic(key: &str, text: &str) -> EmojiPic {
    let image = emoji_image(key);
    EmojiPic { key: key.into(), text: text.into(), has_image: image.is_some(), image: image.unwrap_or_default() }
}

fn poll_item(p: &poll::Poll) -> PollItem {
    let total = p.total();
    let answers: Vec<PollAnswerItem> = p
        .answers
        .iter()
        .map(|a| PollAnswerItem {
            id: a.id.min(i32::MAX as u64) as i32,
            text: readable(&a.text).into(),
            emoji: emoji_pic(if a.emoji_key.is_empty() { &a.emoji } else { &a.emoji_key }, &a.emoji),
            count: a.count.min(i32::MAX as u64) as i32,
            share: if total == 0 { 0.0 } else { a.count as f32 / total as f32 },
            me: a.me,
        })
        .collect();
    let ends = p.expiry_ms.and_then(DateTime::from_timestamp_millis).map(|t| stamp(&t.with_timezone(&Local))).unwrap_or_default();
    PollItem {
        question: readable(&p.question).into(),
        answers: ModelRc::new(VecModel::from(answers)),
        total: total.min(i32::MAX as u64) as i32,
        has_results: p.has_results,
        multi: p.multi,
        ended: p.ended(Local::now().timestamp_millis()),
        ends: ends.into(),
    }
}

/// A bot's button or select menu as drawn. `chosen`: what we picked in a menu since it was shown, by custom id.
fn component_item(c: &components::Component, chosen: &HashMap<String, Vec<String>>) -> ComponentItem {
    let summary = match chosen.get(&c.custom_id) {
        Some(values) => c.options.iter().filter(|o| values.contains(&o.value)).map(|o| o.label.as_str()).collect::<Vec<_>>().join(", "),
        None => c.summary(),
    };
    ComponentItem {
        kind: c.is_select() as i32,
        style: c.style.min(9) as i32,
        label: readable(&c.label).into(),
        emoji: emoji_pic(if c.emoji_key.is_empty() { &c.emoji } else { &c.emoji_key }, &c.emoji),
        custom_id: c.custom_id.clone().into(),
        url: c.url.clone().into(),
        disabled: c.disabled,
        pending: false,
        placeholder: readable(&c.placeholder).into(),
        summary: readable(&summary).into(),
        usable: c.kind == components::STRING_SELECT,
        demo_open: false,
    }
}

fn component_rows(rows: &[Vec<components::Component>], chosen: &HashMap<String, Vec<String>>) -> ModelRc<ComponentRow> {
    let rows: Vec<ComponentRow> = rows
        .iter()
        .map(|r| ComponentRow { items: ModelRc::new(VecModel::from(r.iter().map(|c| component_item(c, chosen)).collect::<Vec<_>>())) })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// The options of an open select menu; `chosen` the values picked so far.
fn select_option_items(c: &components::Component, chosen: &[String]) -> Vec<SelectOptionItem> {
    c.options
        .iter()
        .map(|o| SelectOptionItem {
            label: readable(&o.label).into(),
            value: o.value.clone().into(),
            description: readable(&o.description).into(),
            emoji: emoji_pic(if o.emoji_key.is_empty() { &o.emoji } else { &o.emoji_key }, &o.emoji),
            chosen: chosen.contains(&o.value),
        })
        .collect()
}

/// How long a server's scheduled events are kept without asking again.
const EVENTS_CACHE_FOR: Duration = Duration::from_secs(5 * 60);

/// A scheduled event's card. `me`: our user id (our own events can be cancelled).
fn event_item(e: &events::ScheduledEvent, when: String, place: String, interested: bool, avatar: Option<Pixels>, me: &str) -> EventItem {
    EventItem {
        id: e.id.clone().into(),
        link: events::link(&e.guild, &e.id).into(),
        when: when.into(),
        active: e.active(),
        name: readable(&e.name).into(),
        description: readable(&e.description).into(),
        place: place.into(),
        place_kind: e.entity.min(3) as i32,
        count: e.users.min(i32::MAX as u64) as i32,
        interested,
        has_avatar: avatar.is_some(),
        avatar: avatar.map(Image::from_rgba8).unwrap_or_default(),
        initial: initials(&e.creator_name).chars().next().map(String::from).unwrap_or_default().into(),
        mine: !me.is_empty() && e.creator_id == me,
    }
}

/// The new event as its window's fields have it.
fn event_draft(ui: &AppWindow) -> events::Draft {
    events::Draft {
        entity: ui.get_event_kind().clamp(1, 3) as u64,
        channel: ui.get_event_channel().to_string(),
        location: ui.get_event_location().to_string(),
        name: ui.get_event_name().to_string(),
        date: ui.get_event_date().to_string(),
        time: ui.get_event_time().to_string(),
        end_date: ui.get_event_end_date().to_string(),
        end_time: ui.get_event_end_time().to_string(),
        description: ui.get_event_desc().to_string(),
    }
}

/// "Далее" on the details: the fields are checked (the reason stays on the page) and the preview card made.
fn event_preview(ui: &AppWindow) {
    let draft = event_draft(ui);
    if let Err(e) = events::create_body(&draft, &Local::now()) {
        ui.set_event_error(e.into());
        return;
    }
    let channels = ui.get_event_channels();
    let place = match draft.entity {
        events::EXTERNAL => draft.location.trim().to_string(),
        _ => (0..channels.row_count()).filter_map(|i| channels.row_data(i)).find(|c| c.id.as_str() == draft.channel).map(|c| c.name.to_string()).unwrap_or_default(),
    };
    let when = events::start_of(&draft).map(|t| events::when(t, Local::now().date_naive())).unwrap_or_default();
    let e = events::ScheduledEvent {
        id: String::new(),
        guild: String::new(),
        name: draft.name.trim().to_string(),
        description: draft.description.trim().to_string(),
        start_ms: 0,
        end_ms: None,
        status: 1,
        entity: draft.entity,
        channel: None,
        location: String::new(),
        creator_id: String::new(),
        creator_avatar: None,
        creator_name: String::new(),
        users: 1,
    };
    ui.set_event_preview(event_item(&e, when, place, true, None, ""));
    ui.set_event_step(3);
}

/// The card of a file waiting above the message box.
fn pending_item(name: String, size: String, thumb: Option<Pixels>) -> PendingFileItem {
    PendingFileItem { name: name.into(), size: size.into(), has_image: thumb.is_some(), image: thumb.map(Image::from_rgba8).unwrap_or_default() }
}

/// Marks a bot's button or menu on screen as waiting for the bot's answer, or not any more.
fn set_component_pending(ui: &AppWindow, message: &str, custom_id: &str, pending: bool) {
    let Some(row) = message_row(ui, message) else { return };
    let Some(m) = ui.get_messages().row_data(row) else { return };
    for r in 0..m.components.row_count() {
        let Some(items) = m.components.row_data(r).map(|r| r.items) else { continue };
        for i in 0..items.row_count() {
            if items.row_data(i).is_some_and(|c| c.custom_id.as_str() == custom_id) {
                update_row(items.clone(), i, |c| c.pending = pending);
            }
        }
    }
}

fn clip_item(c: &voicemsg::Clip) -> VoiceClipItem {
    VoiceClipItem {
        bars: ModelRc::new(VecModel::from(c.bars.clone())),
        total: voicemsg::clock(c.secs).into(),
        elapsed: voicemsg::clock(0.0).into(),
        progress: 0.0,
        playing: false,
    }
}

fn reaction_items(list: &[chat::Reaction]) -> ModelRc<ReactionItem> {
    let items: Vec<ReactionItem> = list
        .iter()
        .map(|r| {
            let image = emoji_image(&r.key);
            ReactionItem {
                key: r.key.clone().into(),
                text: r.text.clone().into(),
                has_image: image.is_some(),
                image: image.unwrap_or_default(),
                count: r.count as i32,
                me: r.me,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

// ---------- Tray, closing, starting with Windows ----------

thread_local! {
    static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
    static TRAY_IDS: RefCell<Option<(tray_icon::menu::MenuId, tray_icon::menu::MenuId)>> = const { RefCell::new(None) };
}

/// Which colour of the app icon is chosen (0 as made).
static APP_ICON: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The app icon in a shade: 0 as made (white on black), 1 black on white, 2 white on dark grey, 3 grey on black.
fn icon_variant(n: i32, size: u32) -> image::RgbaImage {
    let base = image::load_from_memory(include_bytes!("../assets/icon.png")).map(|i| i.to_rgba8()).unwrap_or_else(|_| image::RgbaImage::new(size, size));
    let base = image::imageops::resize(&base, size, size, image::imageops::FilterType::Lanczos3);
    let mut shaded = base;
    for p in shaded.pixels_mut() {
        // How much of the bars this pixel is: the square is black, the bars white.
        let bar = p[0] as u32;
        let (back, ink): (u32, u32) = match n {
            1 => (255, 0),
            2 => (46, 255),
            3 => (0, 150),
            _ => (0, 255),
        };
        let shade = ((back * (255 - bar) + ink * bar) / 255) as u8;
        p[0] = shade;
        p[1] = shade;
        p[2] = shade;
    }
    shaded
}

fn slint_image(img: &image::RgbaImage) -> Image {
    Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

/// Gives Windows the icon of a window that is on screen. Without it the taskbar shows what it remembers for
/// the exe's path, which stays the old picture after an update.
fn set_window_icon(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::{platform::windows::WindowExtWindows, window::Icon};
    let picture = |size: u32| {
        let img = icon_variant(APP_ICON.load(std::sync::atomic::Ordering::Relaxed), size);
        let (w, h) = img.dimensions();
        Icon::from_rgba(img.into_raw(), w, h).ok()
    };
    // The small one is for the title and Alt+Tab, the big one for the taskbar.
    let (small, big) = (picture(32), picture(64));
    window.with_winit_window(|native| {
        native.set_window_icon(small);
        native.set_taskbar_icon(big);
    });
}

/// Puts the chosen colour of the icon on the window, the title bar and the tray.
fn apply_icon(ui: &AppWindow, n: i32) {
    APP_ICON.store(n, std::sync::atomic::Ordering::Relaxed);
    ui.global::<Appearance>().set_icon_current(slint_image(&icon_variant(n, 128)));
    set_window_icon(ui.window());
    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            let png = icon_variant(n, 64);
            if let Ok(icon) = tray_icon::Icon::from_rgba(png.to_vec(), png.width(), png.height()) {
                let _ = tray.set_icon(Some(icon));
            }
        }
    });
}

/// Streamer mode follows the switch, or a screen being shown.
fn sync_streamer(ui: &AppWindow) {
    notify::set_streamer(ui.global::<Appearance>().get_streamer_live());
}

/// Puts the icon by the clock, or takes it away.
fn set_tray(on: bool) {
    if !on {
        TRAY.with(|t| *t.borrow_mut() = None);
        return;
    }
    if TRAY.with(|t| t.borrow().is_some()) {
        return;
    }
    let png = icon_variant(APP_ICON.load(std::sync::atomic::Ordering::Relaxed), 64);
    let Ok(icon) = tray_icon::Icon::from_rgba(png.to_vec(), png.width(), png.height()) else { return };
    let menu = tray_icon::menu::Menu::new();
    let open = tray_icon::menu::MenuItem::new(i18n::t("Открыть Nimbus"), true, None);
    let quit = tray_icon::menu::MenuItem::new(i18n::t("Выход"), true, None);
    let _ = menu.append_items(&[&open, &quit]);
    let built = tray_icon::TrayIconBuilder::new().with_icon(icon).with_tooltip("Nimbus").with_menu(Box::new(menu)).build();
    if let Ok(tray) = built {
        TRAY_IDS.with(|ids| *ids.borrow_mut() = Some((open.id().clone(), quit.id().clone())));
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
    }
}

/// A maximized window that is dragged: Windows doesn't move it, so it goes back to its usual size first,
/// with the same part of the title bar under the cursor.
fn restore_under_cursor(ui: &AppWindow) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowPlacement, GetWindowRect, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, WINDOWPLACEMENT};
    let hwnd = ui
        .window()
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        })
        .flatten();
    let Some(hwnd) = hwnd else { return };
    let hwnd = HWND(hwnd as *mut _);
    let (mut cursor, mut big) = (POINT::default(), RECT::default());
    let mut place = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe {
        if GetCursorPos(&mut cursor).is_err() || GetWindowRect(hwnd, &mut big).is_err() || GetWindowPlacement(hwnd, &mut place).is_err() {
            return;
        }
        let width = place.rcNormalPosition.right - place.rcNormalPosition.left;
        let share = (cursor.x - big.left) as f32 / (big.right - big.left).max(1) as f32;
        let x = cursor.x - (width as f32 * share) as i32;
        let y = cursor.y - (16.0 * ui.window().scale_factor()) as i32;
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

thread_local! {
    static MOVED_WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static MOVED_TIMER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BEING_MOVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// How often a look is taken, and how often the window is drawn while it is being moved.
const MOVE_LOOK_MS: u32 = 100;
const MOVE_DRAW_MS: u32 = 16;

unsafe extern "system" fn moved_tick(_: windows::Win32::Foundation::HWND, _: u32, _: usize, _: u32) {
    use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, SetTimer, GUITHREADINFO, GUI_INMOVESIZE};
    let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    let moving = unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok() && info.flags.0 & GUI_INMOVESIZE.0 != 0;
    if moving != BEING_MOVED.with(|b| b.replace(moving)) {
        let every = if moving { MOVE_DRAW_MS } else { MOVE_LOOK_MS };
        unsafe { SetTimer(None, MOVED_TIMER.with(|t| t.get()), every, Some(moved_tick)) };
    }
    if moving {
        slint::platform::update_timers_and_animations();
        if let Some(ui) = MOVED_WINDOW.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade())) {
            ui.window().request_redraw();
        }
    }
}

/// While a window is moved or resized, Windows runs a message loop of its own and Slint's timers,
/// animations and redraws get no turn, so a stream stood still until the mouse let go. A thread timer
/// runs in that loop as well, and gives them their turn.
fn keep_drawing_while_moved(ui: &AppWindow) {
    MOVED_WINDOW.with(|w| *w.borrow_mut() = Some(ui.as_weak()));
    let id = unsafe { windows::Win32::UI::WindowsAndMessaging::SetTimer(None, 0, MOVE_LOOK_MS, Some(moved_tick)) };
    MOVED_TIMER.with(|t| t.set(id));
}

/// Closing to the tray, the tray's menu, and the starting-with-Windows switch.
fn setup_system(ui: &AppWindow) {
    let saved = prefs::read();
    let tray_on = saved["tray"].as_bool().unwrap_or(true);
    ui.set_tray_on(tray_on);
    set_tray(tray_on);
    ui.set_autostart_on(system::autostart_enabled());
    ui.on_forget_servers(|| prefs::write("servers", json!({})));
    ui.on_nav_match(|text, query| text.to_lowercase().contains(&query.trim().to_lowercase()));
    // Sign out: forget the saved account here and start a fresh copy, which opens at the sign-in screen.
    ui.on_sign_out(|| {
        forget_token();
        if let Ok(exe) = std::env::current_exe() {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("cmd")
                .raw_arg("/C ping -n 3 127.0.0.1 >nul & start \"\" \"%NIMBUS_EXE%\"")
                .env("NIMBUS_EXE", exe)
                .creation_flags(0x0800_0000)
                .spawn();
        }
        std::process::exit(0);
    });
    ui.on_tray_toggled(|on| {
        prefs::write("tray", json!(on));
        set_tray(on);
    });
    let weak = ui.as_weak();
    ui.on_autostart_toggled(move |on| {
        // Only ever done because the person flipped the switch; if Windows refuses, the switch goes back.
        if !system::set_autostart(on) {
            if let Some(ui) = weak.upgrade() {
                ui.set_autostart_on(!on);
            }
        }
    });
    keep_drawing_while_moved(ui);
    // Our own title bar: the window has no Windows frame.
    {
        use slint::winit_030::WinitWindowAccessor;
        let weak = ui.as_weak();
        ui.on_window_drag(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.window().is_maximized() {
                    restore_under_cursor(&ui);
                }
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        });
        let weak = ui.as_weak();
        ui.on_window_minimize(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
        let weak = ui.as_weak();
        ui.on_window_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                let now = ui.window().is_maximized();
                ui.window().set_maximized(!now);
            }
        });
        let weak = ui.as_weak();
        ui.on_window_close(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().dispatch_event(slint::platform::WindowEvent::CloseRequested);
            }
        });
    }
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        let to_tray = weak.upgrade().is_some_and(|ui| ui.get_tray_on()) && TRAY.with(|t| t.borrow().is_some());
        if !to_tray {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    // The tray's menu and a click on its icon.
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    let last_tick = std::cell::Cell::new(Instant::now());
    let ticks = std::cell::Cell::new(0u32);
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
        // Late by much: the window was busy all that time and didn't answer.
        let gap = last_tick.replace(Instant::now()).elapsed();
        if gap > Duration::from_millis(400) {
            log_line(&format!("window stalled for about {} ms", gap.as_millis() - 200));
        }
        let ids = TRAY_IDS.with(|ids| ids.borrow().clone());
        // A minimized or hidden window shows no streams: their pictures are not even made then.
        if let Some(ui) = weak.upgrade() {
            use slint::winit_030::WinitWindowAccessor;
            let minimized = ui.window().with_winit_window(|w| w.is_minimized().unwrap_or(false)).unwrap_or(false);
            WINDOW_SHOWN.store(ui.window().is_visible() && !minimized, std::sync::atomic::Ordering::Relaxed);
            CALL_SHOWN.store(ui.get_show_call() && ui.get_voice_channel() != "", std::sync::atomic::Ordering::Relaxed);
            // Once a second, while in voice.
            ticks.set(ticks.get().wrapping_add(1));
            if ticks.get() % 5 == 0 && ui.get_voice_connected() {
                let shown = voiceping::tick();
                ui.set_voice_stats(VoiceStats {
                    last: shown.last as i32,
                    average: shown.average as i32,
                    path: shown.path.into(),
                    top: shown.top as i32,
                    start: shown.start.into(),
                    end: shown.end.into(),
                    server: shown.server.into(),
                    has_loss: shown.loss.is_some(),
                    loss: format!("{:.1}", shown.loss.unwrap_or(0.0)).into(),
                    encrypted: shown.encrypted,
                    code: shown.code.into(),
                });
            }
        }
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            match &ids {
                Some((open, _)) if event.id == *open => {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                    }
                }
                Some((_, quit)) if event.id == *quit => {
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        }
        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if let tray_icon::TrayIconEvent::Click { button: tray_icon::MouseButton::Left, .. } = event {
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.show();
                }
            }
        }
    });
    // The timer must outlive this function.
    std::mem::forget(timer);
}

// ---------- Notifications ----------

thread_local! {
    static TOAST: RefCell<Option<ToastState>> = const { RefCell::new(None) };
}

/// Plays Nimbus's own notification sound.
fn play_chime() {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY};
    static WAV: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let wav = WAV.get_or_init(notify::chime_wav);
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC);
    }
}

/// Whose message a notification shows, and where a click on it leads ("" as the server of a DM).
#[derive(Clone)]
struct ToastSource {
    author: String,
    guild: String,
    channel: String,
}

struct ToastState {
    window: ToastWindow,
    timer: slint::Timer,
    source: ToastSource,
    // Messages merged into it after the first.
    count: i32,
}

/// The Windows handle of a window on screen.
fn hwnd_of(window: &slint::Window) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        })
        .flatten()
}

/// A small window in the lower right corner for a few seconds; nothing when Nimbus is in front.
fn show_toast(ui: &AppWindow, toast_text: notify::ToastText, who: ToastSource, sound: bool) {
    use slint::winit_030::WinitWindowAccessor;
    if ui.window().with_winit_window(|w| w.has_focus()).unwrap_or(false) {
        return;
    }
    let screen = ui.window().with_winit_window(|w| w.current_monitor().map(|m| m.size())).flatten();
    let (sw, sh) = screen.map(|s| (s.width as i32, s.height as i32)).unwrap_or((1920, 1080));
    let still = ui.global::<Appearance>().get_reduce_motion();
    let merged = TOAST.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            let Ok(window) = ToastWindow::new() else { return false };
            let (weak, toast_weak) = (ui.as_weak(), window.as_weak());
            window.on_clicked(move || {
                if let Some(t) = toast_weak.upgrade() {
                    t.invoke_leave();
                }
                let Some(ui) = weak.upgrade() else { return };
                let _ = ui.show();
                ui.window().set_minimized(false);
                ui.window().with_winit_window(|w| w.focus_window());
                let opened = TOAST.with(|cell| cell.borrow().as_ref().map(|t| (t.source.guild.clone(), t.source.channel.clone())));
                if let Some((guild, channel)) = opened {
                    ui.invoke_toast_open(guild.into(), channel.into());
                }
            });
            let toast_weak = window.as_weak();
            window.on_gone(move || {
                if let Some(t) = toast_weak.upgrade() {
                    let _ = t.hide();
                }
            });
            let toast_weak = window.as_weak();
            window.on_hovered(move |over| {
                TOAST.with(|cell| {
                    let cell = cell.borrow();
                    let Some(toast) = cell.as_ref() else { return };
                    if over {
                        toast.timer.stop();
                    } else if let Some(t) = toast_weak.upgrade() {
                        start_toast_timer(&toast.timer, &t);
                    }
                });
            });
            *cell = Some(ToastState { window, timer: slint::Timer::default(), source: who.clone(), count: 0 });
        }
        let Some(toast) = cell.as_mut() else { return false };
        let window = &toast.window;
        // More from the chat already on screen goes into the same notification.
        let merged = window.window().is_visible() && toast.source.channel == who.channel;
        toast.count = if merged { toast.count + 1 } else { 0 };
        if toast.source.author != who.author {
            window.set_has_avatar(false);
        }
        toast.source = who.clone();
        window.set_still(still);
        window.set_heading(toast_text.heading.as_str().into());
        window.set_place(toast_text.place.as_str().into());
        window.set_body(toast_text.body.as_str().into());
        window.set_anonymous(toast_text.anonymous);
        window.set_initial(notify::initial_of(&toast_text.heading).into());
        window.set_more(toast.count);
        if !merged {
            let scale = window.window().scale_factor();
            let (w, h) = ((360.0 * scale) as i32, (96.0 * scale) as i32);
            window.window().set_position(slint::PhysicalPosition::new(sw - w - 16, sh - h - 64));
            window.invoke_appear();
            let _ = window.show();
            if let Some(hwnd) = hwnd_of(window.window()) {
                system::round_corners(hwnd);
            }
        }
        start_toast_timer(&toast.timer, window);
        merged
    });
    // One sound per notification, not one per message in it.
    if sound && !merged {
        play_chime();
    }
}

/// How long a notification stays once the pointer is off it.
const TOAST_FOR: Duration = Duration::from_secs(6);

fn start_toast_timer(timer: &slint::Timer, window: &ToastWindow) {
    let weak = window.as_weak();
    timer.start(slint::TimerMode::SingleShot, TOAST_FOR, move || {
        if let Some(t) = weak.upgrade() {
            t.invoke_leave();
        }
    });
}

/// The sender's picture, when it comes and if they are still the one on screen.
fn set_toast_avatar(author: &str, picture: Pixels) {
    TOAST.with(|cell| {
        if let Some(toast) = cell.borrow().as_ref().filter(|t| t.source.author == author) {
            toast.window.set_avatar(Image::from_rgba8(picture));
            toast.window.set_has_avatar(true);
        }
    });
}

/// `--demo --demo-toast=N`: a notification with made-up words, nothing real is looked at.
fn demo_toast(variant: i32) -> Result<(), slint::PlatformError> {
    let window = ToastWindow::new()?;
    let p = notify::Prefs::DEFAULT;
    let text = match variant {
        1 => notify::toast_text(&p, "demo_bob", "мой сервер · #общий-чат", "Собираемся в девять, кто будет? Напишите заранее, чтобы мы успели собрать две полные команды и найти свободный сервер без очереди, а то в прошлый раз ждали почти час."),
        2 => notify::toast_text(&notify::Prefs { show_text: false, ..p }, "demo_bob", "мой сервер · #общий-чат", "секрет"),
        _ => notify::toast_text(&p, "Рина", &i18n::t("Личное сообщение"), "Привет! Ты сегодня будешь?"),
    };
    window.set_heading(text.heading.as_str().into());
    window.set_place(text.place.as_str().into());
    window.set_body(text.body.as_str().into());
    window.set_anonymous(text.anonymous);
    window.set_initial(notify::initial_of(&text.heading).into());
    if variant == 0 {
        window.set_avatar(demo_ball(80, (235, 170, 90), true));
        window.set_has_avatar(true);
    }
    if variant == 1 {
        window.set_more(2);
    }
    if let Some((x, y)) = std::env::var("NIMBUS_DEMO_AT").ok().as_deref().and_then(demo_position) {
        window.window().set_position(slint::PhysicalPosition::new(x, y));
    }
    window.invoke_appear();
    let weak = window.as_weak();
    let path = std::env::args().find_map(|a| a.strip_prefix("--snapshot=").map(str::to_string));
    if let Some(path) = path {
        slint::Timer::single_shot(Duration::from_millis(1200), move || {
            if let Some(window) = weak.upgrade() {
                if let Ok(buf) = window.window().take_snapshot() {
                    let _ = image::save_buffer(&path, buf.as_bytes(), buf.width(), buf.height(), image::ColorType::Rgba8);
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    window.run()
}
/// The changelog in the window, and the note that this version is new here (not in the demo).
fn setup_changelog(ui: &AppWindow) {
    let lines: Vec<ChangelogLine> = changelog::lines()
        .into_iter()
        .map(|line| match line {
            changelog::Line::Version(text) => ChangelogLine { kind: 0, text: text.into() },
            changelog::Line::Section(text) => ChangelogLine { kind: 1, text: text.into() },
            changelog::Line::Item(text) => ChangelogLine { kind: 2, text: text.into() },
            changelog::Line::Plain(text) => ChangelogLine { kind: 3, text: text.into() },
        })
        .collect();
    let a = ui.global::<Appearance>();
    a.set_changelog(ModelRc::new(VecModel::from(lines)));
    a.set_nimbus_version(update::VERSION.into());
    let demo = std::env::args().any(|arg| arg == "--demo");
    let new_here = !demo && changelog::note_version(update::VERSION);
    let shown = new_here || std::env::args().any(|arg| arg == "--demo-updated" || arg == "--demo-changelog");
    a.set_updated_open(shown);
    a.set_changelog_open(std::env::args().any(|arg| arg == "--demo-changelog"));
}

/// The interface language: English until the person picks another, then remembered.
fn setup_language(ui: &AppWindow) {
    let apply = |lang: &str| {
        let lang = ["ru", "uk", "es", "de", "fr", "pt", "pl", "tr"].into_iter().find(|known| *known == lang).unwrap_or("en");
        i18n::set_language(lang);
        if let Err(e) = slint::select_bundled_translation(lang) {
            log_line(&format!("language {lang}: {e}"));
        }
        lang
    };
    let saved = prefs::read()["language"].as_str().unwrap_or("en").to_string();
    let a = ui.global::<Appearance>();
    a.set_language(apply(&saved).into());
    let weak = ui.as_weak();
    a.on_language_picked(move |lang| {
        let lang = apply(&lang);
        prefs::write("language", serde_json::json!(lang));
        if let Some(ui) = weak.upgrade() {
            ui.global::<Appearance>().set_language(lang.into());
        }
    });
}

/// Loads the saved notification choices and keeps them saved.
fn setup_notifications(ui: &AppWindow) {
    let saved = notify::Prefs::from_json(&prefs::read()["notify"]);
    notify::set_current(saved);
    ui.set_notify_enabled(saved.enabled);
    ui.set_notify_sound(saved.sound);
    ui.set_notify_text(saved.show_text);
    ui.set_notify_everyone(saved.everyone);
    let weak = ui.as_weak();
    ui.on_notify_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let p = notify::Prefs {
            enabled: ui.get_notify_enabled(),
            sound: ui.get_notify_sound(),
            show_text: ui.get_notify_text(),
            everyone: ui.get_notify_everyone(),
        };
        notify::set_current(p);
        prefs::write("notify", p.to_json());
    });
}

/// Puts saved looks on screen.
fn apply_look(ui: &AppWindow, look: prefs::Look) {
    let a = ui.global::<Appearance>();
    a.set_scale(look.scale);
    a.set_compact(look.compact);
    a.set_show_time(look.show_time);
    a.set_reduce_motion(look.reduce_motion);
    a.set_message_anim(look.message_anim);
    a.set_contrast(look.contrast);
    a.set_glass(look.glass);
    a.set_time12(look.time12);
    a.set_date_iso(look.date_iso);
    a.set_theme(look.theme);
    names::set_light(look.theme == 3);
    a.set_accent(look.accent);
    a.set_streamer(look.streamer);
    a.set_streamer_auto(look.streamer_auto);
    a.set_hide_from_capture(look.hide_from_capture);
    a.set_app_icon(look.app_icon);
    a.set_icon_previews(ModelRc::new(VecModel::from((0..4).map(|n| slint_image(&icon_variant(n, 64))).collect::<Vec<_>>())));
    clock::set(look.time12, look.date_iso);
    apply_icon(ui, look.app_icon);
    sync_streamer(ui);
}

/// What the settings pages show now.
fn read_look(ui: &AppWindow) -> prefs::Look {
    let a = ui.global::<Appearance>();
    prefs::Look {
        scale: a.get_scale(),
        compact: a.get_compact(),
        show_time: a.get_show_time(),
        reduce_motion: a.get_reduce_motion(),
        message_anim: a.get_message_anim(),
        contrast: a.get_contrast(),
        glass: a.get_glass(),
        time12: a.get_time12(),
        date_iso: a.get_date_iso(),
        theme: { names::set_light(a.get_theme() == 3); a.get_theme() },
        accent: a.get_accent(),
        streamer: a.get_streamer(),
        streamer_auto: a.get_streamer_auto(),
        hide_from_capture: a.get_hide_from_capture(),
        app_icon: a.get_app_icon(),
    }
}

thread_local! {
    /// The friends of the open invite window, before its search.
    static INVITE_PEOPLE: RefCell<Vec<PersonItem>> = const { RefCell::new(Vec::new()) };
}

/// The invite window's friends that fit its search. Rows that are the same stay untouched, so the
/// list keeps its place while avatars load.
fn show_invite_people(ui: &AppWindow) {
    let wanted = ui.get_invite_query().to_lowercase();
    let fits = |p: &&PersonItem| wanted.is_empty() || p.name.to_lowercase().contains(&wanted) || p.subtitle.to_lowercase().contains(&wanted);
    let rows: Vec<PersonItem> = INVITE_PEOPLE.with_borrow(|all| all.iter().filter(fits).cloned().collect());
    let model = ui.get_invite_people();
    let Some(list) = model.as_any().downcast_ref::<VecModel<PersonItem>>() else {
        ui.set_invite_people(ModelRc::new(VecModel::from(rows)));
        return;
    };
    if list.row_count() != rows.len() {
        list.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if list.row_data(i).as_ref() != Some(&row) {
            list.set_row_data(i, row);
        }
    }
}

fn change_invite_person(ui: &AppWindow, id: &str, change: impl FnOnce(&mut PersonItem)) {
    INVITE_PEOPLE.with_borrow_mut(|all| {
        if let Some(person) = all.iter_mut().find(|p| p.id == id) {
            change(person);
        }
    });
    show_invite_people(ui);
}

/// A gateway GUILD_CREATE as its id, name and icon hash; none while the server is unavailable.
fn new_guild(data: &Value) -> Option<(String, String, Option<String>)> {
    if data["unavailable"].as_bool() == Some(true) {
        return None;
    }
    // As in READY, newer payloads nest these under `properties`.
    let p = if data["properties"].is_object() { &data["properties"] } else { data };
    let id = data["id"].as_str()?.to_string();
    Some((id, readable(p["name"].as_str()?), p["icon"].as_str().map(String::from)))
}

/// The server rail's clicks and drags.
fn setup_rail(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_invite_search(move || {
        if let Some(ui) = weak.upgrade() {
            show_invite_people(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_rail_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            sync_rail(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_rail_toggle(move |key| {
        folders::toggle(&key);
        if let Some(ui) = weak.upgrade() {
            sync_rail(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_rail_drop(move |from, row, zone| {
        let Some(ui) = weak.upgrade() else { return };
        let present: Vec<String> = ui.get_guilds().iter().filter(|g| g.id != HOME).map(|g| g.id.to_string()).collect();
        // The rail's first line is the messages, which aren't part of the saved order.
        folders::dropped(&present, &from, (row - 1).max(0) as usize, folders::Zone::from_int(zone));
        sync_rail(&ui);
    });
}

/// Draws the server rail from the servers and the saved order and folders. Rows that are the same
/// stay untouched, so loading an icon doesn't redraw the whole rail.
fn sync_rail(ui: &AppWindow) {
    let guilds: Vec<GuildItem> = ui.get_guilds().iter().collect();
    let present: Vec<String> = guilds.iter().filter(|g| g.id != HOME).map(|g| g.id.to_string()).collect();
    let selected = ui.get_selected_guild();
    let find = |id: &str| guilds.iter().find(|g| g.id == id).cloned().unwrap_or_default();
    let empty = || ModelRc::new(VecModel::from(Vec::<GuildItem>::new()));
    let plain = |item: GuildItem, inside: bool, last: bool| RailItem { item, inside, last, folder: false, open: false, selected: false, thumbs: empty() };
    let mut rows: Vec<RailItem> = guilds.iter().filter(|g| g.id == HOME).map(|g| plain(g.clone(), false, false)).collect();
    for row in folders::rows(&present) {
        match row {
            folders::Row::Server { id, folder, last } => rows.push(plain(find(&id), folder.is_some(), last)),
            folders::Row::Folder { id, open, servers } => {
                let inside: Vec<GuildItem> = servers.iter().map(|s| find(s)).collect();
                let item = GuildItem {
                    id: folders::folder_key(id).into(),
                    unread: inside.iter().any(|g| g.unread),
                    mentions: inside.iter().map(|g| g.mentions).sum(),
                    voice: inside.iter().map(|g| g.voice).max().unwrap_or(0),
                    ..Default::default()
                };
                let selected = inside.iter().any(|g| g.id == selected);
                let thumbs = ModelRc::new(VecModel::from(inside.into_iter().take(4).collect::<Vec<_>>()));
                rows.push(RailItem { item, folder: true, open, selected, inside: false, last: false, thumbs });
            }
        }
    }
    let model = ui.get_rail();
    let Some(list) = model.as_any().downcast_ref::<VecModel<RailItem>>() else {
        ui.set_rail(ModelRc::new(VecModel::from(rows)));
        return;
    };
    if list.row_count() != rows.len() {
        list.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        let same = list.row_data(i).is_some_and(|old| {
            old.thumbs.iter().eq(row.thumbs.iter()) && RailItem { thumbs: row.thumbs.clone(), ..old } == row
        });
        if !same {
            list.set_row_data(i, row);
        }
    }
}

/// The Typing animation: it starts when the slide from the side (260 ms in `MessageRow`) ends.
const TYPING_AFTER: Duration = Duration::from_millis(260);
const TYPING_TICK: Duration = Duration::from_millis(33);
/// A long message types faster, so it never takes more than about 1.6 s.
const TYPING_MAX_TICKS: usize = 48;

/// Whether a block appears letter by letter; the others (a hidden spoiler among them) come whole.
fn types_out(kind: i32) -> bool {
    matches!(kind, TEXT | CODE | QUOTE)
}

/// How many steps a block takes to type.
fn typed_len(b: &Block) -> usize {
    if types_out(b.kind) { b.text.chars().count() } else { 1 }
}

/// The blocks of a message with `count` steps typed, a caret where the typing is. Plain until the
/// end: half of a **bold** would show its marks.
fn typed_blocks(full: &[Block], count: usize) -> Vec<Block> {
    let mut left = count;
    let mut out = Vec::new();
    for b in full {
        let len = typed_len(b);
        if left >= len {
            out.push(b.clone());
            left -= len;
            continue;
        }
        if types_out(b.kind) {
            let text = b.text.chars().take(left).chain(['▌']).collect::<String>();
            out.push(Block { styled: slint::StyledText::from_plain_text(&text), text: text.into(), ..b.clone() });
        }
        break;
    }
    out
}

/// The next steps of the typing. `shown` is the row's own list: an edit replaces it, and this then
/// goes on unseen, without undoing the edit.
fn type_out(weak: slint::Weak<AppWindow>, shown: std::rc::Rc<VecModel<Block>>, full: Vec<Block>, count: usize, step: usize) {
    let Some(ui) = weak.upgrade() else { return };
    let follow = ui.invoke_near_bottom();
    let done = count >= full.iter().map(typed_len).sum::<usize>();
    shown.set_vec(if done { full.clone() } else { typed_blocks(&full, count) });
    if follow {
        ui.invoke_follow_bottom();
    }
    if !done {
        slint::Timer::single_shot(TYPING_TICK, move || type_out(weak, shown, full, count + step, step));
    }
}

/// Puts a new message at the end of the chat: it comes in with the animation from the settings.
/// `fresh` goes away soon after, or the row would play it again when scrolled back to.
fn push_fresh(ui: &AppWindow, mut row: MessageItem) {
    let model = ui.get_messages();
    let Some(list) = model.as_any().downcast_ref::<VecModel<MessageItem>>() else { return };
    let id = row.id.clone();
    row.fresh = true;
    let a = ui.global::<Appearance>();
    let typing = a.get_message_anim() == 1 && !a.get_reduce_motion() && !row.system;
    let full: Vec<Block> = if typing { row.blocks.iter().collect() } else { Vec::new() };
    let shown = full.iter().any(|b| types_out(b.kind)).then(|| std::rc::Rc::new(VecModel::from(typed_blocks(&full, 0))));
    if let Some(shown) = &shown {
        row.blocks = ModelRc::from(shown.clone());
    }
    list.push(row);
    if let Some(shown) = shown {
        let total = full.iter().map(typed_len).sum::<usize>();
        let step = total.div_ceil(TYPING_MAX_TICKS);
        let weak = ui.as_weak();
        slint::Timer::single_shot(TYPING_AFTER, move || type_out(weak, shown, full, step, step));
    }
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(1), move || {
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_messages();
        let found = (0..model.row_count()).rev().take(50).find_map(|i| model.row_data(i).filter(|r| r.id == id).map(|r| (i, r)));
        if let Some((i, mut row)) = found {
            row.fresh = false;
            model.set_row_data(i, row);
        }
    });
}

/// 0xRRGGBB as a colour.
fn rgb_color(c: u32) -> slint::Color {
    slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Puts text on the Windows clipboard (through clip.exe, no window flashes).
fn copy_text(text: &str) {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = std::process::Command::new("clip").stdin(std::process::Stdio::piped()).creation_flags(CREATE_NO_WINDOW).spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// The row of a message on screen, by its id.
fn message_row(ui: &AppWindow, id: &str) -> Option<usize> {
    let model = ui.get_messages();
    (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|m| m.id.as_str() == id))
}

fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect()
}

impl Guild {
    fn item(&self) -> GuildItem {
        GuildItem {
            id: self.id.clone().into(),
            name: self.name.clone().into(),
            initials: initials(&self.name).into(),
            icon: Image::default(),
            has_icon: false,
            unread: self.badge.unread,
            mentions: self.badge.mentions as i32,
            voice: 0,
        }
    }
}

impl Channel {
    fn item(&self) -> ChannelItem {
        ChannelItem {
            id: self.id.clone().into(),
            name: self.name.clone().into(),
            kind: self.kind,
            clickable: self.clickable,
            avatar: Image::default(),
            has_avatar: self.has_avatar,
            people: ModelRc::new(VecModel::<VoiceMember>::default()),
            subtitle: self.subtitle.clone().into(),
            status: self.status,
            badge: self.badge,
            unread: self.unread,
            muted: self.muted,
            folded: self.folded,
        }
    }
}

impl Msg {
    fn item(&self) -> MessageItem {
        let (w, h) = self.picture.unwrap_or_default();
        let blocks: Vec<Block> = self
            .blocks
            .iter()
            .map(block_item)
            .collect();
        MessageItem {
            fresh: false,
            id: self.id.clone().into(),
            edited: self.edited,
            deleted: false,
            reactions: reaction_items(&self.reactions),
            author_id: self.author_id.clone().into(),
            author: self.author.clone().into(),
            avatar: Image::default(),
            decoration: Image::default(),
            has_decoration: false,
            time: self.time.clone().into(),
            short_time: self.short_time.clone().into(),
            blocks: ModelRc::new(VecModel::from(blocks)),
            grouped: self.grouped,
            system: self.system,
            system_icon: self.system_line.as_ref().map_or(0, |l| l.icon),
            system_what: self.system_line.as_ref().map_or(String::new(), |l| l.what.clone()).into(),
            system_whom: self.system_line.as_ref().map_or(String::new(), |l| l.whom.clone()).into(),
            system_rest: self.system_line.as_ref().map_or(String::new(), |l| l.rest.clone()).into(),
            system_call: self.system_line.as_ref().is_some_and(|l| l.call_open),
            picture: Image::default(),
            has_picture: self.picture.is_some(),
            picture_full: self.picture_links.full.clone().into(),
            picture_original: self.picture_links.original.clone().into(),
            picture_width: w,
            picture_height: h,
            highlight: self.highlight,
            has_author_color: self.color.is_some(),
            mention: self.mention,
            pinned: self.pinned,
            cards: ModelRc::new(VecModel::from(self.cards.iter().map(card_item).collect::<Vec<_>>())),
            author_color: self.color.map(rgb_color).unwrap_or_default(),
            emojis: ModelRc::new(VecModel::from(self.emojis.iter().map(|p| emoji_pic(&p.key, &p.text)).collect::<Vec<_>>())),
            sticker_kind: match &self.sticker {
                None => 0,
                Some(s) if s.url.is_some() => 1,
                Some(_) => 2,
            },
            sticker: Image::default(),
            sticker_name: self.sticker.as_ref().map(|s| s.name.clone()).unwrap_or_default().into(),
            has_poll: self.poll.is_some(),
            poll: self.poll.as_ref().map(poll_item).unwrap_or_default(),
            has_voice: self.voice.is_some(),
            voice: self.voice.as_ref().map(clip_item).unwrap_or_default(),
            forwarded: self.forwarded,
            invites: ModelRc::new(VecModel::from(self.invites.iter().map(|c| invite_item(c)).collect::<Vec<_>>())),
            components: component_rows(&self.components, &HashMap::new()),
            ephemeral: self.ephemeral,
        }
    }
}

// ---------- UI helpers ----------

fn on_ui(ui: &Weak<AppWindow>, f: impl FnOnce(AppWindow) + Send + 'static) {
    let _ = ui.upgrade_in_event_loop(f);
}

/// Turns the "sharing" mark of the window on or off. While something is shown, Nimbus's own window can be kept out of screen captures (a setting).
pub fn set_sharing(ui: &AppWindow, on: bool) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    ui.set_sharing(on);
    ui.global::<Appearance>().set_sharing(on);
    sync_streamer(ui);
    let hwnd = ui
        .window()
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        })
        .flatten();
    if let Some(hwnd) = hwnd {
        system::exclude_from_capture(hwnd, on && ui.global::<Appearance>().get_hide_from_capture());
    }
}
fn notice(ui: &Weak<AppWindow>, text: impl Into<String>) {
    let text = text.into();
    on_ui(ui, move |ui| ui.set_notice(text.into()));
}

/// The list's height is only known after layout, so scroll a moment later.
fn scroll_soon(ui: &AppWindow) {
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(40), move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_scroll_to_bottom();
        }
    });
}

/// Shows a row in the middle of the list; row heights vary, so the place is
/// estimated from the row's share of the list, and redone once heights settle.
fn scroll_to_row_soon(ui: &AppWindow, row: usize) {
    let total = ui.get_messages().row_count().max(1);
    let share = (row as f32 + 0.5) / total as f32;
    for ms in [60, 250, 700] {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_scroll_to_share(share);
            }
        });
    }
}

/// Changes one row of a list on screen, if the list is still a VecModel.
fn update_row<T: Clone + 'static>(model: ModelRc<T>, row: usize, change: impl FnOnce(&mut T)) {
    if let Some(list) = model.as_any().downcast_ref::<VecModel<T>>() {
        if let Some(mut item) = list.row_data(row) {
            change(&mut item);
            list.set_row_data(row, item);
        }
    }
}

/// Under the channel we're in, people in the Nimbus room are shown once:
/// their Discord voice entry (they're in both now) is left out. Then the
/// call screen's tiles are rebuilt from both lists.
pub fn hide_doubles(ui: &AppWindow) {
    let current = ui.get_voice_channel();
    if current.is_empty() {
        ui.set_call_tiles(ModelRc::new(VecModel::<CallTile>::default()));
        return;
    }
    let members: Vec<VoiceMember> = ui.get_voice_members().iter().collect();
    let here: HashSet<String> = members.iter().map(|m| m.identity.to_string()).collect();
    let model = ui.get_channels();
    // A DM call has no row in the channel list: its Discord people come on their own.
    let mut discord_people: Vec<VoiceMember> = ui.get_call_people().iter().filter(|p| !here.contains(p.identity.as_str())).collect();
    for row in 0..model.row_count() {
        let Some(item) = model.row_data(row) else { continue };
        // Only a voice channel's row has people (a DM's row in the DM list has the same id as its call).
        if item.id != current || item.kind != 1 {
            continue;
        }
        let people: Vec<VoiceMember> = item.people.iter().filter(|p| !here.contains(p.identity.as_str())).collect();
        if people.len() != item.people.row_count() {
            update_row(model.clone(), row, |c| c.people = ModelRc::new(VecModel::from(people.clone())));
        }
        discord_people = people;
    }
    // Streams first, like Discord, then everyone: Nimbus people, then Discord's.
    let everyone: Vec<VoiceMember> = members.into_iter().chain(discord_people).collect();
    let previews = STREAM_PREVIEWS.lock().unwrap().clone();
    // A stream being watched keeps its picture when the tiles are made anew.
    let playing: HashMap<slint::SharedString, CallTile> = ui.get_call_tiles().iter().filter(|t| t.stream && t.watched).map(|t| (t.identity.clone(), t)).collect();
    let tile = |m: &VoiceMember, stream: bool| {
        let preview = stream.then(|| previews.get(m.identity.as_str()).cloned()).flatten();
        let mut made = CallTile {
            has_preview: preview.is_some(),
            preview: preview.map(Image::from_rgba8).unwrap_or_default(),
            ..plain_tile(m, stream)
        };
        if let Some(was) = playing.get(&m.identity).filter(|_| stream) {
            made.video = was.video.clone();
            made.has_video = was.has_video;
            made.quality = was.quality.clone();
        }
        made
    };
    let mut tiles: Vec<CallTile> = everyone.iter().filter(|m| m.streaming).map(|m| tile(m, true)).collect();
    tiles.extend(everyone.iter().map(|m| tile(m, false)));
    ui.set_call_tiles(ModelRc::new(VecModel::from(tiles)));
}

/// (we are in voice there, we show a screen, someone else is in voice, someone else shows a screen)
/// from a server's voice states.
fn voice_summary(states: &HashMap<String, Value>, me: &str) -> (bool, bool, bool, bool) {
    let mut s = (false, false, false, false);
    for (uid, vs) in states {
        if vs["channel_id"].is_null() {
            continue;
        }
        let streaming = vs["self_stream"].as_bool().unwrap_or(false);
        if uid == me {
            s.0 = true;
            s.1 = streaming;
        } else {
            s.2 = true;
            s.3 |= streaming;
        }
    }
    s
}

/// The badge number for `GuildItem::voice`; `sharing` is our own show, which counts at once,
/// before Discord says so. Only our own voice is shown: Discord stops sending voice updates of servers we
/// are not looking at, so who else is in voice there would go stale and show people who left long ago.
fn badge_of(s: (bool, bool, bool, bool), sharing: bool) -> i32 {
    match s {
        (true, streaming, _, _) => {
            if streaming || sharing {
                4
            } else {
                3
            }
        }
        _ => 0,
    }
}

/// What the user card says under the name: the first game or activity, and how many more ("Rust +2"); "Online" when none.
fn activity_line(activities: &[Value]) -> String {
    let first = home::activity_text(&Value::Array(activities.to_vec()));
    if first.is_empty() {
        return i18n::t("В сети");
    }
    match activities.len() {
        0 | 1 => first,
        n => format!("{first} +{}", n - 1),
    }
}

/// Characters that channel names use as separators or padding (· | ︱ ・ blank fillers): the interface font has no
/// glyph for many of them and would draw empty boxes. They become a middle dot or a space.
fn tidy_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{200B}'..='\u{200D}' | '\u{2060}'))
        .map(|c| match c {
            '\u{3021}'..='\u{3029}' | '\u{30FB}' | '\u{FF65}' | '\u{2022}' | '\u{2219}' | '\u{22C5}' | '\u{2027}' | '\u{FF5C}' | '\u{2502}' | '\u{2503}' | '\u{2506}' | '\u{2507}'
            | '\u{254E}' | '\u{254F}' | '\u{2551}' | '\u{3163}' | '\u{4E28}' | '\u{FE31}'..='\u{FE34}' | '\u{2016}' | '\u{2225}' | '\u{23AE}' | '\u{239C}'
            | '\u{239F}' | '\u{FFE4}' | '\u{FFE8}' | '\u{2758}'..='\u{275A}' | '\u{2223}' | '\u{00A6}' | '\u{01C0}' | '\u{23D0}' | '\u{2595}' | '\u{258F}' => '·',
            '\u{3000}' | '\u{2800}' | '\u{3164}' | '\u{115F}' | '\u{1160}' | '\u{FFA0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes down what the channel names are made of where it is not plain letters (the first few names with
/// anything odd in them, every character that is not ASCII as its code), so what is still drawn as a box can be named.
fn log_odd_chars(names: impl Iterator<Item = String>) {
    let odd = |c: char| !c.is_alphanumeric() && !c.is_ascii() && (c as u32) < 0x1F000;
    let samples: Vec<String> = names
        .filter(|n| n.chars().any(odd))
        .take(6)
        .map(|n| {
            let codes: Vec<String> = n.chars().filter(|c| !c.is_ascii()).map(|c| format!("U+{:04X}", c as u32)).collect();
            format!("{} [{}]", n.chars().filter(|c| c.is_ascii()).collect::<String>(), codes.join(" "))
        })
        .collect();
    if !samples.is_empty() {
        log_line(&format!("channel names with odd characters: {}", samples.join(" | ")));
    }
}
/// Discord streams' still previews, by streamer id (filled by `Worker::refresh_previews`).
static STREAM_PREVIEWS: std::sync::LazyLock<Mutex<HashMap<String, Pixels>>> = std::sync::LazyLock::new(Mutex::default);
/// How many watch each stream, as Discord last told.
static STREAM_VIEWERS: std::sync::LazyLock<Mutex<HashMap<String, i32>>> = std::sync::LazyLock::new(Mutex::default);
/// How often a stream's preview is fetched again; Discord itself renews them every few minutes.
const PREVIEW_EVERY: Duration = Duration::from_secs(60);

/// Someone in our DM call in Discord's voice; becomes a `VoiceMember` on the UI thread.
struct DmPerson {
    id: String,
    name: String,
    muted: bool,
    deafened: bool,
    avatar: Option<Pixels>,
    frame: Option<Pixels>,
}

impl DmPerson {
    fn member(self) -> VoiceMember {
        VoiceMember {
            initial: self.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
            speaking: DISCORD_TALKING.lock().unwrap().contains(&self.id),
            has_avatar: self.avatar.is_some(),
            avatar: self.avatar.map(Image::from_rgba8).unwrap_or_default(),
            has_decoration: self.frame.is_some(),
            decoration: self.frame.map(Image::from_rgba8).unwrap_or_default(),
            identity: self.id.into(),
            name: self.name.into(),
            muted: self.muted,
            deafened: self.deafened,
            streaming: false,
            discord: true,
        }
    }
}

/// The streams being watched, by whose they are.
static WATCHED_STREAMS: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(Mutex::default);

fn stream_row(ui: &AppWindow, id: &str) -> Option<usize> {
    ui.get_call_tiles().iter().position(|t| t.stream && t.identity == id)
}

/// A stream is received or no longer; its tile says so.
pub fn set_stream_watched(ui: &AppWindow, id: &str, on: bool) {
    let mut watched = WATCHED_STREAMS.lock().unwrap();
    if on {
        watched.insert(id.to_string());
    } else {
        watched.remove(id);
    }
    drop(watched);
    // Closed and opened again, it is heard again.
    if !on {
        golive::set_stream_sound_off(id, false);
    }
    let Some(row) = stream_row(ui, id) else { return };
    update_row(ui.get_call_tiles(), row, |t| {
        t.watched = on;
        t.sound_off = false;
        if !on {
            t.video = Image::default();
            t.has_video = false;
            t.quality = "".into();
        }
    });
}

/// About how wide a stream's picture is on screen, in real pixels: no wider than the window
/// up on the stage, and its share of the window on a tile.
pub fn stream_shown_width(ui: &AppWindow, id: &str) -> u32 {
    let window = ui.window().size().width;
    if ui.get_watched_identity() == id && !ui.get_call_grid() {
        return window;
    }
    window / (ui.get_call_tiles().row_count().max(1) as f32).sqrt().ceil() as u32
}

/// The newest picture of a stream: on its tile, and on the stage while it is up there.
pub fn show_stream_picture(ui: &AppWindow, id: &str, picture: Image) {
    if ui.get_watched_identity() == id {
        ui.set_stream_frame(picture.clone());
    }
    let Some(row) = stream_row(ui, id) else { return };
    update_row(ui.get_call_tiles(), row, |t| {
        t.video = picture;
        t.has_video = true;
    });
}

pub fn show_stream_quality(ui: &AppWindow, id: &str, text: String) {
    if ui.get_watched_identity() == id {
        ui.set_stream_quality(text.as_str().into());
    }
    let Some(row) = stream_row(ui, id) else { return };
    update_row(ui.get_call_tiles(), row, |t| t.quality = text.into());
}

/// The sound button of a stream's tile: heard or silent, whatever the others do.
fn toggle_stream_sound(ui: &AppWindow, id: &str) {
    let off = !golive::stream_sound_off(id);
    golive::set_stream_sound_off(id, off);
    let Some(row) = stream_row(ui, id) else { return };
    update_row(ui.get_call_tiles(), row, |t| t.sound_off = off);
}

/// Brings a stream being watched up on stage.
pub fn focus_stream(ui: &AppWindow, id: &str) {
    let tile = stream_row(ui, id).and_then(|row| ui.get_call_tiles().row_data(row));
    ui.set_watched_identity(id.into());
    ui.set_watching(true);
    ui.set_stream_frame(tile.as_ref().map(|t| t.video.clone()).unwrap_or_default());
    ui.set_stream_quality(tile.as_ref().map(|t| t.quality.clone()).unwrap_or_default());
    ui.set_stream_name(tile.map(|t| t.name).unwrap_or_default());
    ui.set_stream_volume(audio::user_volume(&format!("stream:{id}")) as f32);
    ui.set_stream_viewers(STREAM_VIEWERS.lock().unwrap().get(id).copied().unwrap_or(0));
}

fn clear_stream_stage(ui: &AppWindow) {
    ui.set_watching(false);
    ui.set_watched_identity("".into());
    ui.set_stream_name("".into());
    ui.set_stream_frame(Image::default());
    ui.set_stream_quality("".into());
}

/// A stream ended or was closed: the next one being watched goes up on stage in its place.
pub fn stream_gone(ui: &AppWindow, id: &str) {
    let was_up = ui.get_watched_identity() == id || ui.get_watched_identity().is_empty();
    set_stream_watched(ui, id, false);
    if !was_up {
        return;
    }
    match ui.get_call_tiles().iter().find(|t| t.stream && t.watched) {
        Some(next) => focus_stream(ui, next.identity.as_str()),
        None => clear_stream_stage(ui),
    }
}

pub fn stop_all_streams(ui: &AppWindow) {
    let ids: Vec<String> = WATCHED_STREAMS.lock().unwrap().iter().cloned().collect();
    for id in ids {
        set_stream_watched(ui, &id, false);
    }
    clear_stream_stage(ui);
}

fn plain_tile(m: &VoiceMember, stream: bool) -> CallTile {
    let video = CAMERA_FRAMES.lock().unwrap().get(m.identity.as_str()).cloned();
    CallTile {
        identity: m.identity.clone(),
        name: m.name.clone(),
        initial: m.initial.clone(),
        avatar: m.avatar.clone(),
        has_avatar: m.has_avatar,
        muted: m.muted,
        deafened: m.deafened,
        speaking: m.speaking,
        discord: m.discord,
        stream,
        tint: tile_tint(&m.identity),
        preview: Image::default(),
        has_preview: false,
        decoration: m.decoration.clone(),
        has_decoration: m.has_decoration,
        watched: stream && WATCHED_STREAMS.lock().unwrap().contains(m.identity.as_str()),
        sound_off: stream && golive::stream_sound_off(m.identity.as_str()),
        quality: "".into(),
        has_video: !stream && video.is_some(),
        video: if stream { Image::default() } else { video.map(Image::from_rgba8).unwrap_or_default() },
    }
}

/// The newest camera picture of each person in the call (ours mirrored), by identity.
static CAMERA_FRAMES: std::sync::LazyLock<Mutex<HashMap<String, Pixels>>> = std::sync::LazyLock::new(Mutex::default);
/// People whose newest picture is on its way to the window: a slow window takes the
/// newest one when it gets to it instead of a queue of old ones.
static CAMERA_QUEUED: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(Mutex::default);

/// Shows someone's camera picture on their tile, or (None) their avatar again.
pub fn set_camera_frame(ui: &Weak<AppWindow>, identity: String, pixels: Option<Pixels>) {
    // "Отключить видео": their camera stays a plain tile for us.
    match pixels.filter(|_| !usermenu::video_hidden(&identity)) {
        Some(pixels) => {
            CAMERA_FRAMES.lock().unwrap().insert(identity.clone(), pixels);
        }
        None => {
            CAMERA_FRAMES.lock().unwrap().remove(&identity);
        }
    }
    if !CAMERA_QUEUED.lock().unwrap().insert(identity.clone()) {
        return;
    }
    on_ui(ui, move |ui| {
        CAMERA_QUEUED.lock().unwrap().remove(&identity);
        let picture = CAMERA_FRAMES.lock().unwrap().get(&identity).cloned();
        let tiles = ui.get_call_tiles();
        for row in 0..tiles.row_count() {
            if tiles.row_data(row).is_some_and(|t| !t.stream && t.identity == identity.as_str()) {
                update_row(tiles.clone(), row, |t| {
                    t.has_video = picture.is_some();
                    t.video = picture.clone().map(Image::from_rgba8).unwrap_or_default();
                });
            }
        }
    });
}

/// People talking in our Discord call right now (Discord user ids), so the
/// channel list keeps their green ring when it's rebuilt from voice states.
static DISCORD_TALKING: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(Mutex::default);

/// Lights the speaking ring of Discord people under our channel and on their tiles.
pub fn set_discord_speaking(ui: &Weak<AppWindow>, talking: HashSet<String>) {
    *DISCORD_TALKING.lock().unwrap() = talking.clone();
    on_ui(ui, move |ui| {
        let model = ui.get_channels();
        for row in 0..model.row_count() {
            let Some(item) = model.row_data(row) else { continue };
            if item.kind != 1 || !item.people.iter().any(|p| p.speaking != talking.contains(p.identity.as_str())) {
                continue;
            }
            let people: Vec<VoiceMember> =
                item.people.iter().map(|p| VoiceMember { speaking: talking.contains(p.identity.as_str()), ..p }).collect();
            update_row(model.clone(), row, |c| c.people = ModelRc::new(VecModel::from(people)));
        }
        let dm_people = ui.get_call_people();
        for row in 0..dm_people.row_count() {
            update_row(dm_people.clone(), row, |p| p.speaking = talking.contains(p.identity.as_str()));
        }
        hide_doubles(&ui);
    });
}

/// Tile colours made from avatars, by user id (filled when an avatar arrives).
static AVATAR_TINTS: std::sync::LazyLock<Mutex<HashMap<String, (u8, u8, u8)>>> = std::sync::LazyLock::new(Mutex::default);

/// A dark, calm colour from a picture: its average, dimmed, never darker than a quiet grey.
fn tint_of(px: &Pixels) -> (u8, u8, u8) {
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for p in px.as_slice().iter().filter(|p| p.a > 128) {
        sum[0] += p.r as u64;
        sum[1] += p.g as u64;
        sum[2] += p.b as u64;
        n += 1;
    }
    if n == 0 {
        return (44, 44, 48);
    }
    let avg = [sum[0] / n, sum[1] / n, sum[2] / n];
    let grey = (avg[0] + avg[1] + avg[2]) / 3;
    let dim = |c: u64| (((c * 3 + grey) / 4) as f32 * 0.34).clamp(36.0, 96.0) as u8;
    (dim(avg[0]), dim(avg[1]), dim(avg[2]))
}

/// The background of someone's call tile: their avatar's colour, or a quiet grey until it is known.
fn tile_tint(identity: &str) -> slint::Color {
    let (r, g, b) = AVATAR_TINTS.lock().unwrap().get(identity).copied().unwrap_or((44, 44, 48));
    slint::Color::from_rgb_u8(r, g, b)
}

#[cfg(test)]
mod tint_tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8) -> Pixels {
        let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(4, 4);
        for p in buf.make_mut_slice() {
            *p = slint::Rgba8Pixel { r, g, b, a: 255 };
        }
        buf
    }

    #[test]
    fn tiles_follow_the_avatar_but_stay_dark() {
        let (r, g, b) = tint_of(&solid(250, 240, 230));
        assert!(r <= 96 && g <= 96 && b <= 96, "light avatar: dim tile");
        let (r, g, b) = tint_of(&solid(0, 0, 0));
        assert!(r >= 36 && g >= 36 && b >= 36, "black avatar: not black");
        let (r, _, b) = tint_of(&solid(220, 40, 60));
        assert!(r > b, "red avatar keeps a warm tile");
    }
}

// ---------- Animated GIFs (UI thread only) ----------

struct Animation {
    channel: String,
    /// The message showing it; found by id, as older messages get added above.
    message: String,
    /// The message's sticker rather than its picture.
    sticker: bool,
    frames: Vec<(Image, u32)>,
    current: usize,
    next: Instant,
}

/// An animated custom emoji: one clock for every place it shows (reactions, big emoji, polls).
struct EmojiAnimation {
    frames: Vec<(Image, u32)>,
    current: usize,
    next: Instant,
}

/// A moving preview in the GIF panel, found by its tile's key.
struct TileAnimation {
    key: String,
    frames: Vec<(Image, u32)>,
    current: usize,
    next: Instant,
}

thread_local! {
    static GIF_TILES: RefCell<Vec<TileAnimation>> = const { RefCell::new(Vec::new()) };
    static ANIMATIONS: RefCell<Vec<Animation>> = const { RefCell::new(Vec::new()) };
    /// By emoji id.
    static EMOJI_ANIMATIONS: RefCell<HashMap<String, EmojiAnimation>> = RefCell::new(HashMap::new());
}

/// How long an animated emoji that nothing on screen shows waits before it is looked for again.
const IDLE_EMOJI_WAIT: Duration = Duration::from_secs(1);

/// Advances every GIF, sticker and emoji whose frame is due. Driven by a timer in `main`.
fn tick_animations(ui: &AppWindow) {
    let channel = ui.get_selected_channel();
    let now = Instant::now();
    ANIMATIONS.with(|anims| {
        let mut anims = anims.borrow_mut();
        anims.retain(|a| a.channel == channel.as_str());
        for anim in anims.iter_mut().filter(|a| a.next <= now) {
            anim.current = (anim.current + 1) % anim.frames.len();
            let (frame, delay) = anim.frames[anim.current].clone();
            anim.next = now + Duration::from_millis(delay as u64);
            if let Some(row) = message_row(ui, &anim.message) {
                let sticker = anim.sticker;
                update_row(ui.get_messages(), row, |m| if sticker { m.sticker = frame } else { m.picture = frame });
            }
        }
    });
    tick_emojis(ui, now);
    tick_gif_tiles(ui, now);
}

/// The GIF panel's previews: the next frame of each that is due.
fn tick_gif_tiles(ui: &AppWindow, now: Instant) {
    GIF_TILES.with(|tiles| {
        let mut tiles = tiles.borrow_mut();
        if tiles.is_empty() {
            return;
        }
        let model = ui.get_gif_items();
        for tile in tiles.iter_mut().filter(|t| t.next <= now) {
            tile.current = (tile.current + 1) % tile.frames.len();
            let (frame, delay) = tile.frames[tile.current].clone();
            tile.next = now + Duration::from_millis(delay as u64);
            if let Some(row) = (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|g| g.key.as_str() == tile.key)) {
                update_row(model.clone(), row, |g| g.image = frame);
            }
        }
    });
}

/// Animated emoji: the next frame of each that is due, put everywhere it shows.
fn tick_emojis(ui: &AppWindow, now: Instant) {
    let changed: HashMap<String, Image> = EMOJI_ANIMATIONS.with(|anims| {
        anims
            .borrow_mut()
            .iter_mut()
            .filter(|(_, a)| a.next <= now)
            .map(|(id, a)| {
                a.current = (a.current + 1) % a.frames.len();
                let (frame, delay) = a.frames[a.current].clone();
                a.next = now + Duration::from_millis(delay as u64);
                (id.clone(), frame)
            })
            .collect()
    });
    if changed.is_empty() {
        return;
    }
    EMOJIS.with(|e| e.borrow_mut().extend(changed.iter().map(|(id, img)| (id.clone(), img.clone()))));
    let used = show_emoji_images(ui, &changed);
    // Emoji nothing on screen shows are looked at only now and then, not on every frame.
    EMOJI_ANIMATIONS.with(|anims| {
        for (_, a) in anims.borrow_mut().iter_mut().filter(|(id, _)| changed.contains_key(*id) && !used.contains(*id)) {
            a.next = now + IDLE_EMOJI_WAIT;
        }
    });
}

/// Puts emoji pictures (by emoji id) into every message row showing them: reactions, big emoji, poll answers.
/// Returns the ids found on screen.
fn show_emoji_images(ui: &AppWindow, pictures: &HashMap<String, Image>) -> HashSet<String> {
    let mut used = HashSet::new();
    let mut picture = |key: &str| {
        let id = chat::emoji_id(key)?;
        let image = pictures.get(id)?;
        used.insert(id.to_string());
        Some(image.clone())
    };
    let model = ui.get_messages();
    for row in 0..model.row_count() {
        let Some(m) = model.row_data(row) else { continue };
        for i in 0..m.reactions.row_count() {
            let Some(image) = m.reactions.row_data(i).and_then(|r| picture(&r.key)) else { continue };
            update_row(m.reactions.clone(), i, |r| {
                r.image = image;
                r.has_image = true;
            });
        }
        for i in 0..m.emojis.row_count() {
            let Some(image) = m.emojis.row_data(i).and_then(|e| picture(&e.key)) else { continue };
            update_row(m.emojis.clone(), i, |e| {
                e.image = image;
                e.has_image = true;
            });
        }
        let answers = m.poll.answers.clone();
        for i in 0..answers.row_count() {
            let Some(image) = answers.row_data(i).and_then(|a| picture(&a.emoji.key)) else { continue };
            update_row(answers.clone(), i, |a| {
                a.emoji.image = image;
                a.emoji.has_image = true;
            });
        }
        for r in 0..m.components.row_count() {
            let Some(items) = m.components.row_data(r).map(|r| r.items) else { continue };
            for i in 0..items.row_count() {
                let Some(image) = items.row_data(i).and_then(|c| picture(&c.emoji.key)) else { continue };
                update_row(items.clone(), i, |c| {
                    c.emoji.image = image;
                    c.emoji.has_image = true;
                });
            }
        }
    }
    // An open select menu's options.
    let options = ui.get_select_options();
    for i in 0..options.row_count() {
        let Some(image) = options.row_data(i).and_then(|o| picture(&o.emoji.key)) else { continue };
        update_row(options.clone(), i, |o| {
            o.emoji.image = image;
            o.emoji.has_image = true;
        });
    }
    // The soundboard's tiles.
    let sounds = ui.get_sound_items();
    for i in 0..sounds.row_count() {
        let Some(image) = sounds.row_data(i).filter(|t| !t.emoji_key.is_empty()).and_then(|t| picture(&t.emoji_key)) else { continue };
        update_row(sounds.clone(), i, |t| {
            t.emoji_image = image;
            t.has_emoji_image = true;
        });
    }
    used
}

// ---------- Images ----------

struct Images {
    http: reqwest::Client,
    cache: Mutex<HashMap<String, Option<Pixels>>>,
    /// Animated pictures once decoded, so a chat opened again doesn't start them over.
    gifs: Mutex<gifcache::Kept<Vec<(Pixels, u32)>>>,
}

impl Images {
    /// Downloads and scales a picture to exactly `w`×`h`, cropping to fill.
    async fn get(&self, url: String, w: u32, h: u32) -> Option<Pixels> {
        let key = format!("{url}#{w}x{h}");
        let hit = self.cache.lock().unwrap().get(&key).cloned();
        if let Some(hit) = hit {
            return hit;
        }
        let mut pixels = self.decode(&url, w, h).await;
        // WebP avatars are much smaller; if one cannot be read, the same picture as PNG always can.
        if pixels.is_none() && url.contains(".webp?") {
            pixels = self.decode(&url.replacen(".webp?", ".png?", 1), w, h).await;
        }
        self.cache.lock().unwrap().insert(key, pixels.clone());
        pixels
    }

    async fn decode(&self, url: &str, w: u32, h: u32) -> Option<Pixels> {
        let bytes = self.download(url, MAX_STILL_BYTES).await?;
        tokio::task::spawn_blocking(move || decode_still(&bytes, w, h)).await.ok().flatten()
    }

    /// All frames of an animated GIF, scaled to `w`×`h`, with their delays in ms.
    /// `max_frames`: 0 for the usual limit.
    async fn frames(&self, url: String, w: u32, h: u32, max_frames: usize) -> Option<Vec<(Pixels, u32)>> {
        let max_frames = if max_frames == 0 { MAX_GIF_FRAMES } else { max_frames.min(MAX_GIF_FRAMES) };
        let key = format!("{}#{w}x{h}#{max_frames}", gifcache::key(&url));
        if let Some(hit) = self.gifs.lock().unwrap().get(&key) {
            return Some(hit);
        }
        let bytes = self.animated_file(&url).await?;
        let frames = tokio::task::spawn_blocking(move || decode_gif(&bytes, w, h, max_frames)).await.ok().flatten()?;
        let size = frames.iter().map(|(px, _)| px.width() as usize * px.height() as usize * 4).sum();
        self.gifs.lock().unwrap().put(key, frames.clone(), size);
        Some(frames)
    }

    /// An animated file: the copy on disk when there is one, else downloaded and kept.
    async fn animated_file(&self, url: &str) -> Option<bytes::Bytes> {
        let saved = gifcache::path_of(url, is_trusted_media_host(url));
        if let Some(path) = &saved {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if !bytes.is_empty() {
                    return Some(bytes::Bytes::from(bytes));
                }
            }
        }
        let bytes = self.download(url, MAX_GIF_BYTES).await?;
        if let Some(path) = saved {
            if let Some(dir) = path.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(path, &bytes).await;
        }
        Some(bytes)
    }

    async fn download(&self, url: &str, max_bytes: u64) -> Option<bytes::Bytes> {
        // Avatars and icons are named by their hash, so a saved copy never goes stale: next start shows them at once.
        let saved = picture_cache_path(url);
        if let Some(path) = &saved {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if !bytes.is_empty() {
                    return Some(bytes::Bytes::from(bytes));
                }
            }
        }
        // Nothing is fetched from this computer or a home network because a message said so.
        if reqwest::Url::parse(url).map_or(true, |u| discord::is_internal_url(&u)) {
            return None;
        }
        let mut res = self.http.get(url).timeout(Duration::from_secs(30)).send().await.ok()?.error_for_status().ok()?;
        if res.content_length().is_some_and(|len| len > max_bytes) {
            return None;
        }
        // The size limit holds for a body sent in pieces too, not only for what the header promised.
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let bytes = bytes::Bytes::from(body);
        if let Some(path) = saved {
            if let Some(dir) = path.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(path, &bytes).await;
        }
        Some(bytes)
    }
}

/// Where a copy of a picture is kept, for those that never change (avatars, server icons).
fn picture_cache_path(url: &str) -> Option<std::path::PathBuf> {
    let keep = ["/avatars/", "/icons/", "/embed/avatars/", "/banners/", "/avatar-decoration-presets/", "/assets/collectibles/"].iter().any(|part| url.contains(part));
    if !keep {
        return None;
    }
    // FNV-1a: the same name on every start.
    let name = url.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("Nimbus Native").join("cache").join(format!("{name:016x}.img")))
}

fn to_pixels(img: image::RgbaImage) -> Pixels {
    SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height())
}

fn decode_still(bytes: &[u8], w: u32, h: u32) -> Option<Pixels> {
    let img = image::load_from_memory(bytes).ok()?;
    Some(to_pixels(img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgba8()))
}

fn decode_gif(bytes: &[u8], w: u32, h: u32, max_frames: usize) -> Option<Vec<(Pixels, u32)>> {
    use image::AnimationDecoder;
    let all = if bytes.starts_with(b"RIFF") {
        image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?.into_frames()
    } else if bytes.starts_with(b"\x89PNG") {
        // Animated PNG (stickers); a still one is left to the still path.
        let png = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)).ok()?;
        if !png.is_apng().ok()? {
            return None;
        }
        png.apng().ok()?.into_frames()
    } else {
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?.into_frames()
    };
    let mut frames = Vec::new();
    for frame in all.take(max_frames) {
        let frame = frame.ok()?;
        let (num, den) = frame.delay().numer_denom_ms();
        // Browsers treat very short delays as 100 ms; so do we.
        let delay = if den == 0 || num / den < 20 { 100 } else { num / den };
        let img = image::DynamicImage::ImageRgba8(frame.into_buffer());
        frames.push((to_pixels(img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgba8()), delay));
    }
    (!frames.is_empty()).then_some(frames)
}

/// One picture to fetch for one row of a list.
#[derive(Default)]
struct ImageJob {
    row: usize,
    /// For a message's pictures: its id (rows move when older messages are added).
    message: String,
    url: String,
    /// Tried in order, as still pictures, when `url` doesn't work out.
    fallbacks: Vec<String>,
    w: u32,
    h: u32,
    /// Message rows have two pictures: the avatar and the attached image.
    is_picture: bool,
    animated: bool,
    /// The avatar frame instead of the avatar.
    deco: bool,
    /// The nameplate behind a member's name.
    plate: bool,
    /// With a nameplate: its colour scheme ("crimson"), painted behind the picture.
    palette: String,
    /// A message's sticker.
    sticker: bool,
    /// For an animated picture: the most frames kept (0: the usual limit).
    max_frames: usize,
}

/// Avatar frames are drawn 1.25 times the avatar; this is sharp enough for the biggest (40 px, high DPI).
const DECO_PX: u32 = 96;
/// A nameplate is a wide strip behind a member's row.
const PLATE_W: u32 = 480;
const PLATE_H: u32 = 96;

/// Appends a line to %TEMP%\nimbus-native.log, for problems worth looking at later.
fn log_line(text: &str) {
    use std::io::Write;
    if !logging() {
        return;
    }
    let path = std::env::temp_dir().join("nimbus-native.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {text}", Local::now().format("%F %T"));
    }
}

/// The log is written only when asked for, to look into a problem: the NIMBUS_LOG variable, or a file nimbus-native.log.on in the temp folder.
fn logging() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NIMBUS_LOG").is_some() || std::env::temp_dir().join("nimbus-native.log.on").exists());
    *ON
}

/// Notes pictures that couldn't be shown, so problem cases can be looked at.
fn log_failure(job: &ImageJob) {
    use std::io::Write;
    if !logging() {
        return;
    }
    let path = std::env::temp_dir().join("nimbus-native.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} picture failed: {} (fallbacks: {:?})", Local::now().format("%F %T"), job.url, job.fallbacks);
    }
}

enum Loaded {
    Still(Pixels),
    Frames(Vec<(Pixels, u32)>),
}

impl Loaded {
    /// The picture to show first.
    fn first(&self) -> Pixels {
        match self {
            Loaded::Still(px) => px.clone(),
            Loaded::Frames(frames) => frames[0].0.clone(),
        }
    }
}

/// How many pictures download at once; more only trips Discord's rate limits.
const PARALLEL_DOWNLOADS: usize = 16;

async fn load_one(images: &Images, job: &ImageJob) -> Option<Loaded> {
    if job.animated {
        match images.frames(job.url.clone(), job.w, job.h, job.max_frames).await {
            Some(frames) if frames.len() > 1 => return Some(Loaded::Frames(frames)),
            Some(mut frames) => return Some(Loaded::Still(frames.remove(0).0)),
            None => {}
        }
    } else if let Some(px) = images.get(job.url.clone(), job.w, job.h).await {
        return Some(Loaded::Still(px));
    }
    // Fallbacks may name the poster's own server (the original of an embed): only known media hosts are tried.
    for url in job.fallbacks.iter().filter(|u| is_trusted_media_host(u)) {
        if let Some(px) = images.get(url.clone(), job.w, job.h).await {
            return Some(Loaded::Still(px));
        }
    }
    log_failure(job);
    None
}

/// Fetches pictures a few at a time and hands each result to `apply` as soon
/// as it's known; `None` means every source failed.
fn load_images(images: Arc<Images>, jobs: Vec<ImageJob>, apply: impl Fn(&ImageJob, Option<Loaded>) + Send + 'static) {
    if jobs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut results = futures_util::stream::iter(jobs)
            .map(|job| {
                let images = images.clone();
                async move {
                    let loaded = load_one(&images, &job).await;
                    (job, loaded)
                }
            })
            .buffer_unordered(PARALLEL_DOWNLOADS);
        while let Some((job, loaded)) = results.next().await {
            apply(&job, loaded);
        }
    });
}

// ---------- Avatars of people in voice ----------

/// Small, but sharp on high-DPI screens.
const VOICE_AVATAR_PX: u32 = 128;

/// Avatars by user id, shared by Discord's voice lists and the Nimbus room
/// (voice.rs builds its rows without the worker).
struct VoiceAvatars {
    images: Arc<Images>,
    ui: Weak<AppWindow>,
    urls: Mutex<HashMap<String, String>>,
    /// `None`: loading, or it failed and the initial stays.
    pixels: Mutex<HashMap<String, Option<Pixels>>>,
    /// Avatar frames: where to get them, and the ones already here.
    frame_urls: Mutex<HashMap<String, String>>,
    frames: Mutex<HashMap<String, Pixels>>,
}

static VOICE_AVATARS: std::sync::OnceLock<VoiceAvatars> = std::sync::OnceLock::new();

/// Remembers where a user's avatar lives, for when they show up in voice.
fn note_user(u: &Value) {
    if let (Some(avatars), Some(id)) = (VOICE_AVATARS.get(), u["id"].as_str()) {
        avatars.urls.lock().unwrap().insert(id.to_string(), avatar_url(u));
        match decoration_url(u) {
            Some(url) => avatars.frame_urls.lock().unwrap().insert(id.to_string(), url),
            None => avatars.frame_urls.lock().unwrap().remove(id),
        };
    }
}

/// The avatar frame if it's here (it starts loading together with the avatar).
fn voice_frame(id: &str) -> Option<Pixels> {
    VOICE_AVATARS.get()?.frames.lock().unwrap().get(id).cloned()
}

/// The avatar if it's already here; otherwise starts loading it and puts it
/// into the lists when it arrives. Needs a tokio runtime.
fn voice_avatar(id: &str) -> Option<Pixels> {
    let avatars = VOICE_AVATARS.get()?;
    let mut pixels = avatars.pixels.lock().unwrap();
    if let Some(known) = pixels.get(id) {
        return known.clone();
    }
    let url = avatars.urls.lock().unwrap().get(id).cloned()?;
    pixels.insert(id.to_string(), None);
    let job = ImageJob { url, w: VOICE_AVATAR_PX, h: VOICE_AVATAR_PX, ..Default::default() };
    let id = id.to_string();
    if let Some(url) = avatars.frame_urls.lock().unwrap().get(&id).cloned() {
        let frame = ImageJob { url, w: DECO_PX, h: DECO_PX, deco: true, ..Default::default() };
        let id = id.clone();
        load_images(avatars.images.clone(), vec![frame], move |_, loaded| {
            let Some(px) = loaded.map(|l| l.first()) else { return };
            let avatars = VOICE_AVATARS.get().expect("set before loading");
            avatars.frames.lock().unwrap().insert(id.clone(), px.clone());
            let id = id.clone();
            on_ui(&avatars.ui, move |ui| show_voice_frame(&ui, &id, px));
        });
    }
    load_images(avatars.images.clone(), vec![job], move |_, loaded| {
        let Some(px) = loaded.map(|l| l.first()) else { return };
        let avatars = VOICE_AVATARS.get().expect("set before loading");
        AVATAR_TINTS.lock().unwrap().insert(id.clone(), tint_of(&px));
        avatars.pixels.lock().unwrap().insert(id.clone(), Some(px.clone()));
        let id = id.clone();
        on_ui(&avatars.ui, move |ui| show_voice_avatar(&ui, &id, px));
    });
    None
}

fn show_voice_frame(ui: &AppWindow, id: &str, px: Pixels) {
    let image = Image::from_rgba8(px);
    let fill = |model: ModelRc<VoiceMember>| {
        for row in 0..model.row_count() {
            if model.row_data(row).is_some_and(|m| m.identity == id) {
                update_row(model.clone(), row, |m| {
                    m.decoration = image.clone();
                    m.has_decoration = true;
                });
            }
        }
    };
    fill(ui.get_voice_members());
    fill(ui.get_call_people());
    let tiles = ui.get_call_tiles();
    for row in 0..tiles.row_count() {
        if tiles.row_data(row).is_some_and(|t| t.identity == id && !t.stream) {
            update_row(tiles.clone(), row, |t| {
                t.decoration = image.clone();
                t.has_decoration = true;
            });
        }
    }
    let channels = ui.get_channels();
    for row in 0..channels.row_count() {
        if let Some(channel) = channels.row_data(row).filter(|c| c.kind == 1) {
            fill(channel.people);
        }
    }
}

fn show_voice_avatar(ui: &AppWindow, id: &str, px: Pixels) {
    let image = Image::from_rgba8(px);
    let fill = |model: ModelRc<VoiceMember>| {
        for row in 0..model.row_count() {
            if model.row_data(row).is_some_and(|m| m.identity == id) {
                update_row(model.clone(), row, |m| {
                    m.avatar = image.clone();
                    m.has_avatar = true;
                });
            }
        }
    };
    fill(ui.get_voice_members());
    fill(ui.get_call_people());
    // The tile's own background follows the avatar.
    let tint = tile_tint(id);
    let tiles = ui.get_call_tiles();
    for row in 0..tiles.row_count() {
        if tiles.row_data(row).is_some_and(|t| t.identity == id && !t.stream) {
            update_row(tiles.clone(), row, |t| t.tint = tint);
        }
    }
    let channels = ui.get_channels();
    for row in 0..channels.row_count() {
        if let Some(channel) = channels.row_data(row).filter(|c| c.kind == 1) {
            fill(channel.people);
        }
    }
}

// ---------- Discord data to display text ----------

/// Fancy "letters" (𝐆𝐨𝐥𝐝, ｆｕｌｌｗｉｄｔｈ) become plain ones: the UI font has
/// no glyphs for them and would draw boxes. Everything else stays as is.
fn readable(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let fancy = matches!(c as u32, 0x1D400..=0x1D7FF | 0xFF01..=0xFF5E);
            let plain: Vec<char> = if fancy { c.to_string().nfkc().collect() } else { vec![c] };
            plain
        })
        .collect()
}

fn display_name(u: &Value) -> String {
    readable(u["global_name"].as_str().or(u["username"].as_str()).unwrap_or("?"))
}

/// Discord's CDN wants power-of-two sizes; we scale down ourselves afterwards.
fn avatar_url(u: &Value) -> String {
    let id = u["id"].as_str().unwrap_or("0");
    match u["avatar"].as_str() {
        Some(hash) => format!("{CDN}/avatars/{id}/{hash}.webp?size=128"),
        None => {
            let index = id.parse::<u64>().map(|n| (n >> 22) % 6).unwrap_or(0);
            format!("{CDN}/embed/avatars/{index}.png")
        }
    }
}

/// What a person bought for their profile besides the avatar: the avatar frame and the nameplate
/// (the picture behind the name in lists). The profile banner is not used.
#[derive(Debug, Default, PartialEq)]
struct ProfileLook {
    nameplate: Option<String>,
    palette: String,
    decoration: Option<String>,
}

fn profile_look(u: &Value) -> ProfileLook {
    ProfileLook { nameplate: nameplate_url(u), palette: nameplate_palette(u), decoration: decoration_url(u) }
}

/// The still picture of a user's nameplate (`collectibles.nameplate.asset` is a path like
/// "nameplates/nameplates/twilight/").
fn nameplate_url(u: &Value) -> Option<String> {
    let asset = u["collectibles"]["nameplate"]["asset"].as_str().filter(|a| !a.is_empty() && !a.contains(".."))?;
    Some(format!("{CDN}/assets/collectibles/{}/static.png", asset.trim_matches('/')))
}

/// The colour scheme of a user's nameplate ("crimson", "sky", ...): Discord paints it behind the picture,
/// which is see-through where the picture has nothing of its own.
fn nameplate_palette(u: &Value) -> String {
    u["collectibles"]["nameplate"]["palette"].as_str().unwrap_or_default().to_string()
}

/// The two ends of a nameplate palette's background, left to right. Crimson is measured from Discord's own card;
/// the others are estimates in the same spirit (dark on the left, the colour on the right).
fn palette_colors(name: &str) -> Option<((u8, u8, u8), (u8, u8, u8))> {
    Some(match name {
        "crimson" => ((0x1c, 0x08, 0x13), (0x4a, 0x16, 0x32)),
        "berry" => ((0x1b, 0x0a, 0x24), (0x4a, 0x1a, 0x5e)),
        "sky" => ((0x09, 0x1a, 0x2c), (0x1b, 0x4b, 0x78)),
        "teal" => ((0x06, 0x1e, 0x21), (0x11, 0x55, 0x5c)),
        "forest" => ((0x0a, 0x1c, 0x0e), (0x1e, 0x51, 0x28)),
        "bubble_gum" => ((0x2a, 0x0e, 0x21), (0x7a, 0x2a, 0x5c)),
        "violet" => ((0x13, 0x0b, 0x2d), (0x3b, 0x2a, 0x80)),
        "cobalt" => ((0x09, 0x10, 0x30), (0x1b, 0x3a, 0x8a)),
        "clover" => ((0x0f, 0x1c, 0x09), (0x3a, 0x61, 0x18)),
        _ => return None,
    })
}

/// The nameplate picture over its palette's gradient (left to right), as one opaque picture.
fn under_palette(mut pixels: Pixels, palette: &str) -> Pixels {
    let Some((from, to)) = palette_colors(palette) else { return pixels };
    let (w, h) = (pixels.width() as usize, pixels.height() as usize);
    if w == 0 || h == 0 {
        return pixels;
    }
    for (i, p) in pixels.make_mut_slice().iter_mut().enumerate() {
        let x = (i % w) as f32 / (w.max(2) - 1) as f32;
        let lerp = |a: u8, b: u8| a as f32 + (b as f32 - a as f32) * x;
        let a = p.a as f32 / 255.0;
        let mix = |top: u8, back: f32| (top as f32 * a + back * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        *p = slint::Rgba8Pixel { r: mix(p.r, lerp(from.0, to.0)), g: mix(p.g, lerp(from.1, to.1)), b: mix(p.b, lerp(from.2, to.2)), a: 255 };
    }
    let _ = h;
    pixels
}

/// The avatar frame (a still picture of it) a user has put on, if any.
fn decoration_url(u: &Value) -> Option<String> {
    u["avatar_decoration_data"]["asset"]
        .as_str()
        .filter(|a| !a.is_empty())
        .map(|a| format!("{CDN}/avatar-decoration-presets/{a}.png?size=128&passthrough=false"))
}

/// The colour of a name style ("nick effect"): `display_name_styles.colors` are numbers, and the
/// first one stands in for the gradient (text here has a single colour).
fn name_style_color(u: &Value) -> Option<u32> {
    u["display_name_styles"]["colors"][0].as_u64().map(|c| (c as u32) & 0x00ff_ffff)
}

fn parse_time(ts: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(ts).ok().map(|d| d.with_timezone(&Local))
}

fn stamp(t: &DateTime<Local>) -> String {
    clock::stamp(t)
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

/// The server address from a `[nimbus-stream] http://host:port` channel topic.
fn stream_server(topic: &str) -> Option<String> {
    static STREAM: OnceLock<Regex> = OnceLock::new();
    let caps = regex(&STREAM, r"\[nimbus-stream\]\s*(https?://\S+)?").captures(topic)?;
    // Only scheme, host and port are kept: a path or query in a topic is not part of the address.
    match caps.get(1) {
        Some(m) => trust::parse(m.as_str()).ok().map(|addr| addr.normal()),
        None => Some(DEFAULT_SERVER.to_string()),
    }
}

fn is_url(s: &str) -> bool {
    static URL: OnceLock<Regex> = OnceLock::new();
    regex(&URL, r"^https?://\S+$").is_match(s)
}

fn is_image(attachment: &Value) -> bool {
    attachment["content_type"].as_str().is_some_and(|t| t.starts_with("image/"))
}

/// Whether a picture may be fetched from this https address directly: Discord's own media hosts
/// and the two GIF services whose clips Discord embeds.
fn is_trusted_media_host(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else { return false };
    let Some(host) = url.host_str().map(str::to_lowercase) else { return false };
    url.scheme() == "https"
        && ["discordapp.com", "discordapp.net", "discord.com", "tenor.com", "giphy.com"]
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// The animated file behind a link preview: a .gif itself, or what Tenor and
/// Giphy keep next to the still picture and the mp4 they show in "gifv".
fn embed_gif(embed: &Value, picture: &str) -> Option<String> {
    let path = picture.split('?').next().unwrap_or(picture);
    // The poster picks this address, so it is used only on Discord's, Tenor's and Giphy's hosts;
    // anything else goes through the proxy copy as a still.
    if path.ends_with(".gif") && is_trusted_media_host(picture) {
        return Some(picture.to_string());
    }
    // Tenor: ids ending AAAAe are the png thumbnail, AAAAC the gif of the same clip.
    if path.starts_with("https://media.tenor.com/") && path.contains("AAAAe") {
        let gif = path.replacen("AAAAe", "AAAAC", 1);
        return Some(gif.rsplit_once('.').map(|(base, _)| format!("{base}.gif")).unwrap_or(gif));
    }
    // Giphy: .../media/<id>/giphy.mp4 has a giphy.gif beside it.
    let video = embed["video"]["url"].as_str().unwrap_or("");
    if let Some((base, _)) = video.split('?').next().and_then(|v| v.rsplit_once('/')).filter(|_| is_trusted_media_host(video)) {
        return Some(format!("{base}/giphy.gif"));
    }
    None
}

struct Picture {
    /// URL sized for the screen by Discord's media proxy.
    url: String,
    /// The same picture from the proxy at its own size, always in a format we can read.
    full: String,
    /// The full original file, for when the proxy can't help.
    original: String,
    /// The original file, when it's an animated GIF we can play.
    gif: Option<String>,
    w: f32,
    h: f32,
    /// The picture comes from a link preview rather than an attachment.
    from_embed: bool,
}

/// A sticker sent with a message: its picture, or (Lottie ones, which aren't drawn here) only its name.
#[derive(Clone, Debug, PartialEq)]
struct Sticker {
    name: String,
    url: Option<String>,
    animated: bool,
}

/// Stickers are drawn this big, as in Discord.
const STICKER_PX: u32 = 160;

/// Where a sticker's picture is, by its format (1 PNG, 2 animated PNG, 3 Lottie, 4 GIF), and whether it moves.
fn sticker_url(id: &str, format: u64) -> Option<(String, bool)> {
    let id = snowflake(id).ok()?;
    let png = format!("https://media.discordapp.net/stickers/{id}.png?size={STICKER_PX}");
    match format {
        1 => Some((png, false)),
        2 => Some((png, true)),
        4 => Some((format!("https://media.discordapp.net/stickers/{id}.gif?size={STICKER_PX}"), true)),
        _ => None,
    }
}

/// The first sticker of a message.
fn sticker_of(m: &Value) -> Option<Sticker> {
    let s = m["sticker_items"].as_array()?.first()?;
    let found = s["id"].as_str().and_then(|id| sticker_url(id, s["format_type"].as_u64().unwrap_or(1)));
    Some(Sticker { name: readable(s["name"].as_str().unwrap_or("")), animated: found.as_ref().is_some_and(|f| f.1), url: found.map(|f| f.0) })
}

/// The first picture of a message: an attached image, or a link preview image.
/// "Идёт звонок: Аня, Борис" for the people in a DM's call; empty when nobody is.
fn dm_call_line(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1..=3 => i18n::tf("Идёт звонок: {}", &[&names.join(", ")]),
        n => i18n::tf("Идёт звонок: {}, {} и ещё {}", &[&names[0], &names[1], &(n - 2)]),
    }
}

/// The player row of a voice message: where the sound is (in samples of `total`) and whether it plays.
fn show_clip_at(ui: &Weak<AppWindow>, message: &str, at: usize, total: usize, playing: bool) {
    let message = message.to_string();
    on_ui(ui, move |ui| {
        let Some(row) = message_row(&ui, &message) else { return };
        update_row(ui.get_messages(), row, |m| {
            m.voice.playing = playing;
            m.voice.progress = if total == 0 { 0.0 } else { (at as f32 / total as f32).min(1.0) };
            m.voice.elapsed = voicemsg::clock(at as f64 / audio::SAMPLE_RATE as f64).into();
        });
    });
}

/// Our own user id, known after sign-in; messages that name us are marked.
static ME_ID: OnceLock<String> = OnceLock::new();

/// Whether a message names us by @mention or @everyone.
fn names_me(m: &Value) -> bool {
    let Some(me) = ME_ID.get() else { return false };
    m["author"]["id"].as_str() != Some(me.as_str())
        && (m["mention_everyone"].as_bool() == Some(true) || m["mentions"].as_array().is_some_and(|l| l.iter().any(|u| u["id"].as_str() == Some(me.as_str()))))
}

/// A file or a link preview under a message.
struct Card {
    /// 0 a file, 1 a link preview.
    kind: i32,
    title: String,
    text: String,
    url: String,
    color: Option<u32>,
}

fn card_item(c: &Card) -> CardItem {
    CardItem {
        kind: c.kind,
        title: c.title.clone().into(),
        text: c.text.clone().into(),
        url: c.url.clone().into(),
        has_color: c.color.is_some(),
        color: c.color.map(rgb_color).unwrap_or_default(),
    }
}

/// "12,4 МБ", "340 КБ", "90 Б".
fn human_size(bytes: u64) -> String {
    let (value, unit) = match bytes {
        b if b >= 1 << 30 => (b as f64 / (1u64 << 30) as f64, i18n::t("ГБ")),
        b if b >= 1 << 20 => (b as f64 / (1u64 << 20) as f64, i18n::t("МБ")),
        b if b >= 1 << 10 => (b as f64 / 1024.0, i18n::t("КБ")),
        b => return i18n::tf("{} Б", &[&b]),
    };
    let text = if value >= 100.0 { format!("{value:.0} {unit}") } else { format!("{value:.1} {unit}") };
    // Russian writes the decimal point as a comma.
    if i18n::english() { text } else { text.replace('.', ",") }
}

/// Files (not pictures: those are shown as pictures) and link previews that say something.
fn cards_of(m: &Value) -> Vec<Card> {
    let mut cards = Vec::new();
    for a in m["attachments"].as_array().into_iter().flatten().filter(|a| !is_image(a) && !voicemsg::is_voice(a)) {
        let (Some(name), Some(url)) = (a["filename"].as_str(), a["url"].as_str()) else { continue };
        cards.push(Card { kind: 0, title: name.to_string(), text: human_size(a["size"].as_u64().unwrap_or(0)), url: url.to_string(), color: None });
    }
    for e in m["embeds"].as_array().into_iter().flatten() {
        // Pictures and gifs show as pictures; the rest needs a title or a description.
        if matches!(e["type"].as_str(), Some("image" | "gifv" | "video")) && e["description"].is_null() {
            continue;
        }
        let (title, description) = (e["title"].as_str().unwrap_or(""), e["description"].as_str().unwrap_or(""));
        if title.is_empty() && description.is_empty() {
            continue;
        }
        let provider = e["provider"]["name"].as_str().unwrap_or("");
        let text: String = description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(260).collect();
        let title = if title.is_empty() { provider } else { title };
        cards.push(Card { kind: 1, title: readable(title), text: readable(&text), url: e["url"].as_str().unwrap_or("").to_string(), color: e["color"].as_u64().map(|c| c as u32) });
    }
    cards
}

fn picture_of(m: &Value) -> Option<Picture> {    let attachment = m["attachments"].as_array().and_then(|list| list.iter().find(|a| is_image(a)));
    let (url, original, w, h, gif, from_embed) = if let Some(a) = attachment {
        let original = a["url"].as_str()?;
        // Animated WebP is played like a GIF; a still one just shows its first frame.
        let gif = matches!(a["content_type"].as_str(), Some("image/gif" | "image/webp")).then(|| original.to_string());
        let proxy = a["proxy_url"].as_str().unwrap_or(original);
        (proxy, original, a["width"].as_u64()?, a["height"].as_u64()?, gif, false)
    } else {
        m["embeds"].as_array()?.iter().find_map(|e| {
            let img = if e["image"].is_object() { &e["image"] } else { &e["thumbnail"] };
            let shows_picture = e["image"].is_object() || matches!(e["type"].as_str(), Some("image" | "gifv"));
            if !shows_picture {
                return None;
            }
            let original = img["url"].as_str()?;
            let gif = embed_gif(e, original);
            let proxy = img["proxy_url"].as_str().unwrap_or(original);
            Some((proxy, original, img["width"].as_u64()?, img["height"].as_u64()?, gif, true))
        })?
    };
    if w == 0 || h == 0 {
        return None;
    }
    let scale = (PICTURE_MAX_W / w as f64).min(PICTURE_MAX_H / h as f64).min(1.0);
    let (bw, bh) = ((w as f64 * scale).round().max(1.0), (h as f64 * scale).round().max(1.0));
    // Ask the media proxy for (twice) the display size, as WebP: that is
    // smaller than the original and always decodable, whatever the source
    // format (HEIC, AVIF, huge PNGs...).
    let sep = if url.contains('?') { '&' } else { '?' };
    let sized = format!("{url}{sep}format=webp&width={}&height={}", bw as u32 * 2, bh as u32 * 2);
    let full = format!("{url}{sep}format=webp");
    Some(Picture { url: sized, full, original: original.to_string(), gif, w: bw as f32, h: bh as f32, from_embed })
}

/// How long a profile asked of Discord is kept.
const USER_PROFILE_FOR: Duration = Duration::from_secs(5 * 60);
const CARD_AVATAR_PX: u32 = 176;
const CARD_DECORATION_PX: u32 = 232;
const CARD_BANNER: (u32, u32) = (680, 240);
// Big enough for the badges window too.
const CARD_BADGE_PX: u32 = 128;

/// Puts a card on screen, then its pictures as they come (if the same person's card is still there by then).
fn push_user_card(ui: &Weak<AppWindow>, images: &Arc<Images>, card: userprofile::Card, status: i32, me: bool, since: String, popup: bool) {
    fn slot(ui: &AppWindow, popup: bool) -> UserCardItem {
        if popup { ui.get_user_card() } else { ui.get_dm_card() }
    }
    fn put(ui: &AppWindow, popup: bool, item: UserCardItem) {
        if popup { ui.set_user_card(item) } else { ui.set_dm_card(item) }
    }
    let mutual = match (card.mutual_friends, card.mutual_servers) {
        (Some(f), Some(s)) => i18n::tf("Общие друзья: {} • Общие серверы: {}", &[&f, &s]),
        (Some(f), None) => i18n::tf("Общие друзья: {}", &[&f]),
        (None, Some(s)) => i18n::tf("Общие серверы: {}", &[&s]),
        (None, None) => String::new(),
    };
    let shown = card.clone();
    on_ui(ui, move |ui| {
        // Pictures already there stay while the same person's card is refreshed.
        let old = slot(&ui, popup);
        let same = old.id.as_str() == shown.id;
        put(&ui, popup, UserCardItem {
            id: shown.id.clone().into(),
            name: shown.name.clone().into(),
            username: shown.username.clone().into(),
            pronouns: shown.pronouns.clone().into(),
            bio: shown.bio.clone().into(),
            tag: shown.tag.clone().into(),
            mutual: mutual.into(),
            since: since.into(),
            status,
            me,
            has_color: shown.color.is_some(),
            banner_color: shown.color.map(rgb_color).unwrap_or_default(),
            avatar: if same { old.avatar } else { Image::default() },
            banner: if same { old.banner } else { Image::default() },
            has_banner: same && old.has_banner,
            decoration: if same { old.decoration } else { Image::default() },
            has_decoration: same && old.has_decoration,
            badges: if same { old.badges } else { ModelRc::default() },
        });
    });
    let (images, ui) = (images.clone(), ui.clone());
    tokio::spawn(async move {
        let id = card.id.clone();
        let update = move |ui: &Weak<AppWindow>, change: Box<dyn FnOnce(&mut UserCardItem) + Send>| {
            let id = id.clone();
            on_ui(ui, move |ui| {
                let mut item = slot(&ui, popup);
                if item.id.as_str() == id {
                    change(&mut item);
                    put(&ui, popup, item);
                }
            });
        };
        if let Some(px) = images.get(card.avatar.clone(), CARD_AVATAR_PX, CARD_AVATAR_PX).await {
            update(&ui, Box::new(move |c| c.avatar = Image::from_rgba8(px)));
        }
        if let Some(px) = match card.banner.clone() {
            Some(url) => images.get(url, CARD_BANNER.0, CARD_BANNER.1).await,
            None => None,
        } {
            update(&ui, Box::new(move |c| {
                c.banner = Image::from_rgba8(px);
                c.has_banner = true;
            }));
        }
        if let Some(px) = match card.decoration.clone() {
            Some(url) => images.get(url, CARD_DECORATION_PX, CARD_DECORATION_PX).await,
            None => None,
        } {
            update(&ui, Box::new(move |c| {
                c.decoration = Image::from_rgba8(px);
                c.has_decoration = true;
            }));
        }
        let mut badges: Vec<(Option<Pixels>, userprofile::Badge)> = Vec::new();
        for (url, info) in card.badges.into_iter().zip(card.badge_info) {
            badges.push((images.get(url, CARD_BADGE_PX, CARD_BADGE_PX).await, info));
        }
        if !badges.is_empty() {
            update(&ui, Box::new(move |c| {
                let list: Vec<BadgeItem> = badges.into_iter().map(|(px, info)| badge_item(px, info)).collect();
                c.badges = ModelRc::new(VecModel::from(list));
            }));
        }
    });
}

fn badge_item(picture: Option<Pixels>, info: userprofile::Badge) -> BadgeItem {
    let (heading, tiers) = userprofile::badge_tiers(&info.id).unwrap_or_default();
    let tiers: Vec<BadgeTier> = tiers.into_iter().map(|t| BadgeTier { name: t.name.into(), after: t.after.into(), reached: t.reached }).collect();
    BadgeItem {
        has_picture: picture.is_some(),
        picture: picture.map(Image::from_rgba8).unwrap_or_default(),
        heading: heading.into(),
        text: info.text.into(),
        link: info.link.into(),
        tiers: ModelRc::new(VecModel::from(tiers)),
    }
}

/// The most a viewed picture may weigh, and its longest side once decoded (screens are not bigger).
const VIEW_MAX_BYTES: u64 = 40 * 1024 * 1024;
const VIEW_MAX_SIDE: u32 = 3840;

/// Loads a picture at full size for the viewer; while it comes, the viewer shows the message's own copy.
fn view_image(images: Arc<Images>, ui: Weak<AppWindow>, url: String) {
    if !is_trusted_media_host(&url) {
        return;
    }
    tokio::spawn(async move {
        let Some(bytes) = images.download(&url, VIEW_MAX_BYTES).await else { return };
        let decoded = tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory(&bytes).ok()?;
            let img = if img.width().max(img.height()) > VIEW_MAX_SIDE { img.thumbnail(VIEW_MAX_SIDE, VIEW_MAX_SIDE) } else { img };
            let rgba = img.to_rgba8();
            Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
        });
        let Some(px) = decoded.await.ok().flatten() else { return };
        on_ui(&ui, move |ui| {
            // Another picture may be open by now.
            if ui.get_viewer_open() && ui.get_viewer_item().picture_full.as_str() == url {
                ui.set_viewer_image(Image::from_rgba8(px));
                ui.set_viewer_has_image(true);
            }
        });
    });
}

/// Asks where to put a picture, then saves it there.
fn save_image(images: Arc<Images>, ui: Weak<AppWindow>, url: String) {
    if !is_trusted_media_host(&url) {
        flash(&ui, i18n::t("Эту картинку нельзя сохранить отсюда"));
        return;
    }
    tokio::spawn(async move {
        let name = userprofile::file_name_of(&url);
        let picked = tokio::task::spawn_blocking(move || rfd::FileDialog::new().set_title(i18n::t("Сохранить картинку")).set_file_name(name).save_file()).await.ok().flatten();
        let Some(path) = picked else { return };
        let saved = match images.download(&url, VIEW_MAX_BYTES).await {
            Some(bytes) => tokio::fs::write(&path, &bytes).await.is_ok(),
            None => false,
        };
        flash(&ui, if saved { i18n::t("Картинка сохранена") } else { i18n::t("Не удалось сохранить картинку") });
    });
}

/// Message content as plain text: mentions and emoji become readable names,
/// non-image files and link previews get a short line each.
/// Markdown's own characters in a name, escaped so the name is drawn as written.
fn md_escape(s: &str) -> String {
    s.chars().flat_map(|c| if "*_~`|\\[]".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

/// Mentions of people, roles and channels written out by name, coloured like Discord's
/// (the colour travels as a mark until the styled text draws it).
fn with_mention_names(content: &str, mentions: Option<&Vec<Value>>) -> String {
    static MENTION: OnceLock<Regex> = OnceLock::new();
    static ROLE: OnceLock<Regex> = OnceLock::new();
    static CHANNEL: OnceLock<Regex> = OnceLock::new();
    static EMOJI: OnceLock<Regex> = OnceLock::new();
    let text = regex(&MENTION, r"<@!?(\d+)>").replace_all(content, |c: &regex::Captures| {
        let name = mentions
            .and_then(|ms| ms.iter().find(|u| u["id"].as_str() == Some(&c[1])))
            .map(display_name)
            .unwrap_or_else(|| i18n::t("пользователь"));
        names::mark_user(&format!("@{}", md_escape(&name)), names::mention_color(), &c[1])
    });
    let text = regex(&ROLE, r"<@&(\d+)>").replace_all(&text, |c: &regex::Captures| match names::role(&c[1]) {
        Some((name, color)) => names::mark(&format!("@{}", md_escape(&readable(&name))), if color == 0 { names::mention_color() } else { color }),
        None => names::mark(&i18n::t("@роль"), names::mention_color()),
    });
    let text = regex(&CHANNEL, r"<#(\d+)>").replace_all(&text, |c: &regex::Captures| match names::channel(&c[1]) {
        Some(name) => names::mark(&format!("#{}", md_escape(&readable(&name))), names::mention_color()),
        None => names::mark(&i18n::t("#канал"), names::mention_color()),
    });
    let text = regex(&EMOJI, r"<a?:(\w+):\d+>").replace_all(&text, ":$1:");
    clock::discord_stamps(&text)
}

fn message_text(m: &Value, hide_content: bool) -> String {
    let content = if hide_content { "" } else { m["content"].as_str().unwrap_or("") };
    let mut text = with_mention_names(content, m["mentions"].as_array());

    let mut extra = Vec::new();
    for a in m["attachments"].as_array().into_iter().flatten().filter(|a| !is_image(a) && !voicemsg::is_voice(a)) {
        extra.push(i18n::tf("[файл] {}", &[&a["filename"].as_str().map_or_else(|| i18n::t("вложение"), str::to_string)]));
    }
    if text.trim().is_empty() && picture_of(m).is_none() {
        for e in m["embeds"].as_array().into_iter().flatten() {
            if let Some(t) = e["title"].as_str().or(e["url"].as_str()) {
                extra.push(i18n::tf("[ссылка] {}", &[&t]));
            }
        }
    }
    if let Some(reply) = m["referenced_message"].as_object() {
        let who = display_name(&reply["author"]);
        let named = names::unmark(&with_mention_names(reply["content"].as_str().unwrap_or(""), reply["mentions"].as_array()));
        let snippet: String = named.chars().take(80).collect();
        text = format!("> {}\n{text}", i18n::tf("в ответ {}: {}", &[&who, &snippet]));
    }
    if !extra.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&extra.join("\n"));
    }
    readable(&text)
}

/// Discord's inline markers (**, __, ~~, `, *) dropped, keeping their text.
fn strip_inline(s: &str) -> String {
    static INLINE: OnceLock<Regex> = OnceLock::new();
    regex(&INLINE, r"\*\*(.+?)\*\*|__(.+?)__|~~(.+?)~~|`([^`\n]+)`|\*(\S[^*\n]*?)\*").replace_all(&names::unmark(s), "$1$2$3$4$5").into_owned()
}

/// Discord's markdown as the CommonMark that styled text reads: its underline
/// (__x__) becomes <u>, stray HTML is escaped, links in the text become links,
/// and line breaks stay line breaks.
fn discord_to_commonmark(md: &str) -> String {
    static UNDERLINE: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let escaped = names::marks_to_font(&md.replace('<', "\\<"));
    let underlined = regex(&UNDERLINE, r"__(.+?)__").replace_all(&escaped, "<u>$1</u>");
    let linked = regex(&URL, r"(^|\s)(https?://[^\s<>)]+)").replace_all(&underlined, "$1<$2>");
    linked.replace('\n', "  \n")
}

/// Styled text of a message block; what can't be read as markdown stays plain.
fn styled_text(md: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(&discord_to_commonmark(md)).unwrap_or_else(|_| slint::StyledText::from_plain_text(&strip_inline(md)))
}

/// One block of a message for the UI (on the UI thread: styled text isn't sent between threads).
fn block_item(b: &Blk) -> Block {
    let styled = matches!(b.kind, TEXT | QUOTE);
    Block {
        kind: b.kind,
        text: (if b.kind == CODE { names::unmark(&b.text) } else { strip_inline(&b.text) }).into(),
        url: b.url.clone().into(),
        styled: if styled { styled_text(&b.text) } else { Default::default() },
    }
}

/// Splits Discord markdown into blocks the UI can draw: code blocks, quotes,/// headings, links on their own line and spoilers. Inline markers (**, ~~, `)
/// are dropped, keeping their text.
fn blocks_of(text: &str) -> Vec<Blk> {
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let _ = &INLINE;
    // The markers stay: styled text draws them, and `block_item` strips them where it can't.
    let strip = |s: &str| s.to_string();

    let mut out: Vec<Blk> = Vec::new();
    // Consecutive text lines (or quote lines) join into one block.
    let push = |out: &mut Vec<Blk>, kind: i32, text: String, url: String| {
        if let Some(last) = out.last_mut() {
            if last.kind == kind && (kind == TEXT || kind == QUOTE) {
                last.text.push('\n');
                last.text.push_str(&text);
                return;
            }
        }
        out.push(Blk { kind, text, url });
    };

    for (i, part) in text.split("```").enumerate() {
        if i % 2 == 1 {
            // The first line of a code block may name its language; drop it.
            let code = match part.split_once('\n') {
                Some((lang, rest)) if !lang.trim().is_empty() && !lang.contains(' ') => rest,
                _ => part,
            };
            out.push(Blk { kind: CODE, text: code.trim_matches('\n').to_string(), url: String::new() });
            continue;
        }
        for line in part.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if out.last().is_some_and(|b| b.kind == TEXT) {
                    push(&mut out, TEXT, String::new(), String::new());
                }
            } else if let Some(rest) = line.strip_prefix("> ").or_else(|| (line == ">").then_some("")) {
                push(&mut out, QUOTE, strip(rest), String::new());
            } else if let Some(rest) = ["### ", "## ", "# "].iter().find_map(|p| line.strip_prefix(p)) {
                push(&mut out, HEADING, strip(rest), String::new());
            } else if is_url(trimmed) {
                push(&mut out, LINK, trimmed.to_string(), trimmed.to_string());
            } else if line.contains("||") {
                for (j, piece) in line.split("||").enumerate() {
                    if piece.trim().is_empty() {
                        continue;
                    }
                    let kind = if j % 2 == 1 { SPOILER } else { TEXT };
                    out.push(Blk { kind, text: strip(piece.trim()), url: String::new() });
                }
            } else {
                let text = line.strip_prefix("-# ").unwrap_or(line);
                push(&mut out, TEXT, strip(text), String::new());
            }
        }
    }
    // Blank lines at the very end of a text block add nothing.
    for b in out.iter_mut().filter(|b| b.kind == TEXT) {
        b.text.truncate(b.text.trim_end().len());
    }
    out.retain(|b| !(b.kind == TEXT && b.text.is_empty()));
    out
}

/// A line like "X added Y to the group", in its parts: the words before the name, the name, the words after;
/// its mark (1 came, 2 went, 3 a call, 4 a pin, 5 anything else) and whether the call can still be joined.
#[derive(Clone, Default)]
struct SystemLine {
    icon: i32,
    what: String,
    whom: String,
    rest: String,
    call_open: bool,
}

fn system_line(m: &Value, kind: u64, action: String) -> SystemLine {
    let author = m["author"]["id"].as_str();
    let named = m["mentions"].as_array().and_then(|l| l.first());
    let template = match (kind, named) {
        // Someone who removes themself has left.
        (2, Some(u)) if u["id"].as_str() == author => Some(i18n::t("покинул(а) группу")),
        (1, Some(_)) => Some(i18n::t("добавил(а) {} в группу")),
        (2, Some(_)) => Some(i18n::t("убрал(а) {} из группы")),
        _ => None,
    };
    let icon = match kind {
        1 | 7 => 1,
        2 => 2,
        3 => 3,
        6 => 4,
        _ => 5,
    };
    let call_open = kind == 3 && m["call"].is_object() && m["call"]["ended_timestamp"].is_null();
    let Some(template) = template else { return SystemLine { icon, what: action, call_open, ..Default::default() } };
    match (template.split_once("{}"), named) {
        (Some((what, rest)), Some(u)) => SystemLine { icon, what: what.trim().to_string(), whom: display_name(u), rest: rest.trim().to_string(), call_open },
        _ => SystemLine { icon, what: template, call_open, ..Default::default() },
    }
}

fn system_action(kind: u64) -> Option<String> {
    match kind {
        0 | 19 | 20 | 23 => None,
        1 => Some(i18n::t("добавил(а) участника в группу")),
        2 => Some(i18n::t("убрал(а) участника из группы")),
        3 => Some(i18n::t("начал(а) звонок")),
        6 => Some(i18n::t("закрепил(а) сообщение")),
        7 => Some(i18n::t("присоединился(-ась) к серверу")),
        8 => Some(i18n::t("забустил(а) сервер")),
        _ => Some(i18n::t("системное сообщение")),
    }
}

/// How many times a chat or server was clicked, and the chat clicked last (empty after a server).
static CHAT_CLICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_CHAT_CLICK: Mutex<String> = Mutex::new(String::new());
static CHAT_CLICKED: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn note_chat_click(channel: &str) {
    *LAST_CHAT_CLICK.lock().unwrap() = channel.to_string();
    CHAT_CLICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    CHAT_CLICKED.notify_waiters();
}

fn chat_clicks() -> u64 {
    CHAT_CLICKS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Ends once something was clicked after the count `seen`: what is still loading isn't wanted any more.
async fn clicked_since(seen: u64) {
    loop {
        let clicked = CHAT_CLICKED.notified();
        if chat_clicks() != seen {
            return;
        }
        clicked.await;
    }
}

/// A click still in the queue when another chat has been clicked since.
fn click_is_stale(channel: &str) -> bool {
    let last = LAST_CHAT_CLICK.lock().unwrap();
    !last.is_empty() && *last != channel
}

/// The client has what it needs on screen, so the start window may go.
static CLIENT_READY: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// How long the start window waits for that after the update check.
const CLIENT_READY_WAIT: Duration = Duration::from_secs(2);
/// A page at a time and not quickly: to Discord this is someone opening a few chats.
const PRELOAD_AFTER: Duration = Duration::from_millis(1500);
const PRELOAD_GAP: Duration = Duration::from_millis(350);

/// Loads the newest page of these chats ahead of the clicks, into memory only.
fn preload_chats(api: Api, pages: Arc<Mutex<pages::Pages>>, chats: Vec<String>) {
    if chats.is_empty() {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(PRELOAD_AFTER).await;
        let started = Instant::now();
        let mut loaded = 0;
        for channel in chats {
            // Signed out in the meantime: nobody holds the pages any more.
            if Arc::strong_count(&pages) == 1 {
                return;
            }
            if pages.lock().unwrap().get(&channel).is_some() {
                continue;
            }
            let Ok(id) = snowflake(&channel) else { continue };
            match api.get(&format!("/channels/{id}/messages?limit={PAGE}")).await {
                Ok(page) => {
                    pages.lock().unwrap().put(&channel, page);
                    loaded += 1;
                }
                // Whatever is wrong (no access, too many requests), it isn't asked any further.
                Err(e) => {
                    log_line(&format!("preload stopped: {e}"));
                    break;
                }
            }
            tokio::time::sleep(PRELOAD_GAP).await;
        }
        log_line(&format!("preload: {loaded} chats in {} ms", started.elapsed().as_millis()));
    });
}

/// How many channels' member lists are kept for coming back; how long an answer is waited for, and how many more times it is asked.
const MEMBER_LISTS_KEPT: usize = 20;
const MEMBER_ASK_AFTER: Duration = Duration::from_secs(3);
const MEMBER_ASKS_AGAIN: u8 = 3;

/// A row of the right panel: kind, id, name, subtitle, status, colour, avatar url, avatar frame url, nameplate url, nameplate palette.
type MemberRow = (i32, String, String, String, i32, Option<u32>, String, Option<String>, Option<String>, String);

/// What a channel list shows, to tell whether it changed.
fn rows_signature(rows: &[Channel]) -> Vec<String> {
    rows.iter().map(|r| format!("{}|{}|{}", r.id, r.name, r.kind)).collect()
}

/// The server clicked last.
static LAST_GUILD_CLICK: Mutex<String> = Mutex::new(String::new());

fn message_cache_path(channel: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("nimbus-native").join("messages").join(format!("{channel}.json"))
}

/// How long a kept page may be shown in place of a fresh one, and how many pages are kept.
/// The pages are readable text of private chats, so they are few and short-lived.
const CACHE_MAX_AGE: Duration = Duration::from_secs(60 * 60);
const CACHE_MAX_FILES: usize = 20;

/// Keeps the newest page of a channel in the temp folder.
fn save_cached_messages(channel: &str, list: &Value) {
    let path = message_cache_path(channel);
    let Some(dir) = path.parent().map(|d| d.to_path_buf()) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, list.to_string());
    // The oldest pages beyond the limit go.
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let extra = files.len().saturating_sub(CACHE_MAX_FILES);
    for (_, old) in files.into_iter().take(extra) {
        let _ = std::fs::remove_file(old);
    }
}

/// The kept page of a channel, when it is recent enough.
fn load_cached_messages(channel: &str) -> Option<Value> {
    let path = message_cache_path(channel);
    let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > CACHE_MAX_AGE {
        return None;
    }
    let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    v.is_array().then_some(v)
}

/// Deletes every kept page (signing out), or only the stale ones (at start).
fn clear_message_cache(only_stale: bool) {
    let Some(dir) = message_cache_path("x").parent().map(|d| d.to_path_buf()) else { return };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let stale = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_none_or(|age| age > CACHE_MAX_AGE);
        if !only_stale || stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A page as Discord gives it, newest first, in the order it is shown.
fn oldest_first(page: &Value) -> Vec<Value> {
    let mut list = page.as_array().cloned().unwrap_or_default();
    list.reverse();
    list
}

/// Rows that stay when a page is replaced keep their pictures, instead of blinking until they load again.
fn keep_pictures(old: &ModelRc<MessageItem>, items: &mut [MessageItem]) {
    let shown: HashMap<slint::SharedString, MessageItem> = old.iter().map(|m| (m.id.clone(), m)).collect();
    for item in items.iter_mut() {
        let Some(was) = shown.get(&item.id) else { continue };
        item.avatar = was.avatar.clone();
        item.picture = was.picture.clone();
        item.sticker = was.sticker.clone();
        item.decoration = was.decoration.clone();
        item.has_decoration = was.has_decoration;
    }
}

/// Converts a message and lists the pictures its row needs.
/// `prev` is the author and time of the message above, for grouping.
fn to_msg(m: &Value, prev: &mut Option<(String, i64)>, jobs: &mut Vec<ImageJob>) -> Msg {
    let id = m["id"].as_str().unwrap_or("").to_string();
    let author_id = m["author"]["id"].as_str().unwrap_or("").to_string();
    let author = display_name(&m["author"]);
    let time = m["timestamp"].as_str().and_then(parse_time);
    let secs = time.map(|t| t.timestamp()).unwrap_or(0);
    let (time_text, short) = time.map(|t| (stamp(&t), clock::short(&t))).unwrap_or_default();
    let kind = m["type"].as_u64().unwrap_or(0);

    if let Some(action) = system_action(kind) {
        let line = system_line(m, kind, action);
        *prev = None;
        return Msg {
            system_line: Some(line),
            id,
            author_id,
            author,
            time: time_text,
            short_time: short,
            blocks: Vec::new(),
            grouped: false,
            system: true,
            picture_links: PictureLinks::default(),
            picture: None,
            edited: false,
            reactions: chat::reactions_of(m),
            highlight: false,
            color: None,
            cards: cards_of(m),
            mention: names_me(m),
            pinned: m["pinned"].as_bool().unwrap_or(false),
            emojis: Vec::new(),
            sticker: None,
            poll: None,
            voice: None,
            forwarded: false,
            invites: Vec::new(),
            components: Vec::new(),
            target: components::Target::default(),
            ephemeral: false,
        };
    }
    let grouped = kind != 19 && prev.as_ref().is_some_and(|(a, t)| *a == author_id && secs - t < GROUP_WINDOW_SECS);
    *prev = Some((author_id.clone(), secs));

    if !grouped {
        jobs.push(ImageJob { message: id.clone(), url: avatar_url(&m["author"]), w: AVATAR_PX, h: AVATAR_PX, ..Default::default() });
        if let Some(url) = decoration_url(&m["author"]) {
            jobs.push(ImageJob { message: id.clone(), url, w: DECO_PX, h: DECO_PX, deco: true, ..Default::default() });
        }
    }
    // A forwarded message shows the original instead (its text as a quote, its picture, files and sticker).
    let outer = m;
    let snapshot = forward::snapshot(outer);
    let m = snapshot.unwrap_or(outer);
    let picture = picture_of(m);
    // Like Discord: a message that is just a link to a picture shows only the picture.
    let content = m["content"].as_str().unwrap_or("").trim();
    let hide_content = picture.as_ref().is_some_and(|p| p.from_embed) && is_url(content);
    let picture_links = picture.as_ref().map(|p| PictureLinks { full: p.full.clone(), original: p.original.clone() }).unwrap_or_default();
    let size = picture.map(|p| {
        let job = match p.gif {
            Some(gif) => ImageJob { message: id.clone(), url: gif, fallbacks: vec![p.url, p.original], w: p.w as u32, h: p.h as u32, is_picture: true, animated: true, ..Default::default() },
            None => ImageJob { message: id.clone(), url: p.url, fallbacks: vec![p.original], w: p.w as u32 * 2, h: p.h as u32 * 2, is_picture: true, ..Default::default() },
        };
        jobs.push(job);
        (p.w, p.h)
    });
    let sticker = sticker_of(m);
    if let Some(Sticker { url: Some(url), animated, .. }) = &sticker {
        // A moving sticker that turns out to be still is drawn from the same file as a still.
        let fallbacks = if *animated { vec![url.clone()] } else { Vec::new() };
        jobs.push(ImageJob { message: id.clone(), url: url.clone(), fallbacks, w: STICKER_PX, h: STICKER_PX, animated: *animated, sticker: true, ..Default::default() });
    }
    // Like Discord: a message of emoji only shows them big instead of as text.
    let emojis = if hide_content { None } else { chat::jumbo(content) }.unwrap_or_default();
    let mut blocks = blocks_of(&message_text(m, hide_content || !emojis.is_empty()));
    if snapshot.is_some() {
        quote_blocks(&mut blocks);
    }
    // Components v2 carry the text in text displays.
    let parsed = components::parse(outer);
    if !parsed.texts.is_empty() {
        blocks.extend(blocks_of(&readable(&parsed.texts.join("\n"))));
    }
    let edited = outer["edited_timestamp"].is_string();
    Msg {
        id,
        author_id,
        author,
        time: time_text,
        short_time: short,
        blocks,
        grouped,
        system: false,
        system_line: None,
        picture_links,
        picture: size,
        edited,
        reactions: chat::reactions_of(outer),
        highlight: false,
        color: name_style_color(&outer["author"]),
        cards: cards_of(m),
        mention: names_me(outer),
        pinned: outer["pinned"].as_bool().unwrap_or(false),
        emojis,
        sticker,
        poll: poll::parse(m),
        voice: voicemsg::clip_of(m),
        forwarded: snapshot.is_some(),
        invites: invite::codes_in(content),
        components: parsed.rows,
        target: components::target_of(outer),
        ephemeral: components::flags(outer) & components::EPHEMERAL != 0,
    }
}

/// The original of a forwarded message is drawn as a quote: its plain text lines become quote lines.
fn quote_blocks(blocks: &mut [Blk]) {
    for b in blocks.iter_mut().filter(|b| b.kind == TEXT) {
        b.kind = QUOTE;
    }
}

/// The custom emoji pictures messages show: in reactions, as big emoji, in poll answers.
fn emoji_urls<'a>(msgs: impl IntoIterator<Item = &'a Msg>) -> Vec<String> {
    let mut urls = Vec::new();
    for m in msgs {
        urls.extend(m.reactions.iter().filter_map(|r| r.image.clone()));
        urls.extend(m.emojis.iter().filter_map(|p| p.image.clone()));
        urls.extend(m.poll.iter().flat_map(|p| p.answers.iter().filter_map(|a| a.emoji_image.clone())));
        for c in m.components.iter().flatten() {
            urls.extend(c.emoji_image.clone());
            urls.extend(c.options.iter().filter_map(|o| o.emoji_image.clone()));
        }
    }
    urls
}

/// Open threads grouped under the channel they belong to: (parent channel id, thread), newest first.
fn threads_by_parent(threads: &[Value]) -> Vec<(String, &Value)> {
    let mut list: Vec<(String, &Value)> = threads
        .iter()
        .filter(|t| matches!(t["type"].as_u64(), Some(10 | 11 | 12)))
        .filter_map(|t| Some((t["parent_id"].as_str()?.to_string(), t)))
        .collect();
    list.sort_by_key(|(_, t)| std::cmp::Reverse(t["last_message_id"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)));
    list
}

/// Server channels in display order: loose channels, then each category
/// with its text channels before voice channels.
fn channel_rows(list: &[Value]) -> Vec<Channel> {
    let kind = |c: &Value| c["type"].as_u64().unwrap_or(99);
    let is_text = |k: u64| matches!(k, 0 | 5 | 15);
    let is_voice = |k: u64| matches!(k, 2 | 13);
    let pos = |c: &Value| c["position"].as_i64().unwrap_or(0);
    let name = |c: &Value| tidy_name(&readable(c["name"].as_str().unwrap_or("")));

    let children = |parent: Option<&str>| {
        let mut v: Vec<&Value> = list
            .iter()
            .filter(|c| (is_text(kind(c)) || is_voice(kind(c))) && c["parent_id"].as_str() == parent)
            .collect();
        v.sort_by_key(|c| (is_voice(kind(c)), pos(c)));
        v
    };
    let row = |c: &Value| {
        let voice = is_voice(kind(c));
        Channel {
            id: c["id"].as_str().unwrap_or("").into(),
            name: name(c),
            kind: if voice { 1 } else { 0 },
            clickable: true,
            ..Default::default()
        }
    };

    let mut categories: Vec<&Value> = list.iter().filter(|c| kind(c) == 4).collect();
    categories.sort_by_key(|c| pos(c));

    let mut rows: Vec<Channel> = children(None).into_iter().map(row).collect();
    for cat in categories {
        let kids = children(cat["id"].as_str());
        if kids.is_empty() {
            continue;
        }
        rows.push(Channel {
            id: cat["id"].as_str().unwrap_or("").into(),
            name: name(cat),
            kind: 2,
            clickable: true,
            ..Default::default()
        });
        rows.extend(kids.into_iter().map(row));
    }
    rows
}

// ---------- Network worker ----------

struct ChannelInfo {
    name: String,
    topic: String,
    dm: bool,
    /// A forum (or media) channel: a list of posts, each its own thread.
    forum: bool,
}

/// The most rows the message list keeps; beyond it the far end is dropped (pictures take the memory).
const MAX_ROWS: usize = 500;

/// What the UI side's trimming of the list tells the network side: the new newest
/// and the new oldest message on screen, when a end was cut off.
static TRIM: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));

/// Messages a request brings, the most Discord gives at once.
const PAGE: usize = 50;
/// A member's avatar in the list, in pixels (shown at 32).
const MEMBER_AVATAR_PX: u32 = 64;

/// Text for a URL query: everything but letters, digits and -._~ as %XX.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// A forum post's thumbnail, in pixels (shown at 56).
const FORUM_THUMB_PX: u32 = 112;

/// How far back the open chat is loaded.
struct History {
    /// The oldest message on screen; the next page is the one before it.
    oldest: Option<String>,
    /// Reached the channel's first message.
    done: bool,
    opened: Instant,
    busy: bool,
    /// Opened around an old message: the newest ones aren't below, so new ones aren't added.
    detached: bool,
    /// The newest message on screen, from where newer ones are loaded when detached.
    newest: Option<String>,
}

impl Default for History {
    fn default() -> Self {
        Self { oldest: None, done: true, opened: Instant::now(), busy: false, detached: false, newest: None }
    }
}

struct Worker {
    ui: Weak<AppWindow>,
    api: Api,
    images: Arc<Images>,
    ready: Value,
    guild: Option<String>,
    guild_names: HashMap<String, String>,
    channel: Option<String>,
    channels: HashMap<String, ChannelInfo>,
    /// Discord's own voice: guild -> user -> voice state (official-client users).
    discord_voice: HashMap<String, HashMap<String, Value>>,
    /// Profiles asked of Discord lately, so a card opened again asks nothing.
    user_profiles: HashMap<String, (Instant, Value)>,
    /// Users seen in READY, voice updates and lookups, by id.
    users: HashMap<String, Value>,
    /// Author and time of the newest message on screen, for grouping new ones.
    last: Option<(String, i64)>,
    /// Rows in the message list, so pictures of new messages find their row.
    message_rows: usize,
    history: History,
    /// The posts of the open forum, in the order shown.
    forum_posts: Vec<String>,
    /// Voice channels of each server seen, in order, for "move to".
    voice_rooms: HashMap<String, Vec<(String, String)>>,
    /// The server the open user menu acts in.
    menu_guild: Option<String>,
    /// What each message on screen says as Discord has it (mentions and all), by id,
    /// for copying and editing.
    raw: HashMap<String, String>,
    /// To the Discord gateway (the member list subscription).
    gateway: mpsc::UnboundedSender<Value>,
    /// The member list on the right, and the server it belongs to.
    members: members::List,
    members_guild: Option<String>,
    /// Our earlier gateway sessions in this run.
    past_sessions: Vec<String>,
    /// The channel whose list we are subscribed to: (server, channel).
    members_channel: Option<(String, String)>,
    /// The lists of channels seen lately, shown at once on coming back while the fresh one is on its way.
    members_kept: HashMap<String, members::List>,
    /// When the list was asked for and how many more times since, while nothing has come.
    members_asked: Option<(Instant, u8)>,
    /// The group whose people are in the right panel, and those rows.
    group_shown: Option<(String, Vec<MemberRow>)>,
    /// How many chunks of 100 the member list was asked for.
    member_chunks: usize,
    /// The emoji of servers seen (id, name, animated), by server.
    guild_emojis: HashMap<String, Vec<(String, String, bool)>>,
    /// The message box as last seen, the suggestions for its @ or # word, and what was picked.
    input_text: String,
    suggest_at: usize,
    suggestions: Vec<mentions::Suggestion>,
    picked_mentions: Vec<(String, String)>,
    /// The account's Nitro kind (0 none), for the upload limit.
    premium_type: u64,
    /// Who is in a call in a DM or group, by user: the DM's channel id, muted, deafened.
    dm_call_of: HashMap<String, (String, bool, bool)>,
    /// DMs whose call line was declined here; it comes back when that call ends.
    dm_call_declined: HashSet<String>,
    /// The DM whose others get rung once our own voice state shows we are in its call.
    ring_pending: Option<String>,
    /// The voice channel joined last, to go back to after another client took it over.
    last_join: Option<LastJoin>,
    /// Threads and forum posts seen (they open as chats but have no member list).
    thread_ids: HashSet<String>,
    /// Role ids of people in the open server, from the member list and their messages.
    role_of: HashMap<String, Vec<String>>,
    /// Roles of each server seen (name, colour, rank), for the member list.
    role_tables: HashMap<String, HashMap<String, members::Role>>,
    voice: mpsc::UnboundedSender<voice::VoiceCmd>,
    me_id: String,
    me_name: String,
    /// The Nimbus server of the open guild, from its stream channel's topic.
    guild_server: String,
    /// The voice channel we asked to join, for others' join/leave sounds.
    voice_channel: Option<String>,
    /// Friends' presences (status, what they play), by user id.
    presences: HashMap<String, home::Presence>,
    /// The page shown instead of the chat: 0 none, 1 friends, 2 message requests.
    people_page: i32,
    /// The way back and forward through chats.
    trail: fold::Trail,
    friends_tab: i32,
    /// The DM list needs redrawing (presences changed); done at most once a second.
    home_dirty: bool,
    /// What the search box shows, in order, so a pick by index finds it.
    search_results: Vec<search::Entry>,
    /// What was last opened from the search box (keys, newest first).
    recent_picks: Vec<String>,
    /// Unread channels and mentions, for the pills and red counts.
    unread: unread::Unread,
    typing: chat::Typing,
    /// The typing line on screen now, to skip redrawing the same text.
    typing_shown: String,
    /// When we last told Discord we're typing, and where.
    typing_sent: Option<(String, Instant)>,
    /// Reactions of the open channel's messages, by message id.
    reactions: HashMap<String, Vec<chat::Reaction>>,
    /// Custom emoji pictures already being loaded.
    emoji_asked: HashSet<String>,
    /// Polls of the open channel's messages, by message id.
    polls: HashMap<String, poll::Poll>,
    /// Bots' buttons and menus of the open channel's messages, by message id; what we picked in their menus
    /// (by message, then custom id); the interactions sent and not answered yet, by nonce (message, custom id).
    components: HashMap<String, (components::Target, Vec<Vec<components::Component>>)>,
    select_chosen: HashMap<String, HashMap<String, Vec<String>>>,
    interactions: HashMap<String, (String, String)>,
    /// The files waiting in the message box (pasted), sent with the next message.
    pending_files: Vec<clipboard::Pending>,
    /// Scheduled events of servers as last asked (by server), and the ones we said we're interested in.
    events: HashMap<String, (Instant, Vec<events::ScheduledEvent>)>,
    events_interested: HashSet<String>,
    /// The creator avatars of events already loaded, by user id.
    event_avatars: HashMap<String, Option<Pixels>>,
    /// Voice messages of the open channel by message id, the sound of those played, and where paused ones stopped.
    clips: HashMap<String, voicemsg::Clip>,
    clip_sound: HashMap<String, Arc<Vec<i16>>>,
    clip_paused: HashMap<String, usize>,
    /// The voice message playing (or played to its end): its message, the sound.
    playing: Option<(String, audio::Playback)>,
    /// Changes whenever playback is stopped by a click, so the progress of the old one stops moving.
    clip_generation: Arc<std::sync::atomic::AtomicU64>,
    /// When each Discord stream's preview was last fetched, by streamer id.
    preview_at: HashMap<String, Instant>,
    /// A server's channels and our roles in it, as last asked: opening the server again doesn't ask again
    /// (the gateway says when they change). By server id: when, the channels, our permissions.
    guild_cache: HashMap<String, (Instant, Value, Option<perms::Me>)>,
    /// The open threads of a server, when last asked.
    active_threads: HashMap<String, (Instant, Value)>,
    /// The newest page of chats opened lately, shown at once when going back to them.
    pages: Arc<Mutex<pages::Pages>>,
    /// The chats of the last run were loaded ahead already (READY comes again after a reconnect).
    preloaded: bool,
    /// The GIF panel: what the last request brought, and how many of them are on screen.
    gifs: Vec<gifs::Gif>,
    gifs_shown: usize,
    /// The message being forwarded (its id, channel, server), and the places it can go.
    forward_from: Option<(String, String, Option<String>)>,
    forward_targets: Vec<forward::Target>,
    /// Our notes about people as last loaded, by user id: a note is saved only when it changed.
    notes: HashMap<String, String>,
    /// The voice message being recorded: the chat it goes to, the recorder.
    recording: Option<(String, audio::Recorder)>,
    /// Slash commands of each chat as last asked (by `slash_key`), the panel's rows, the commands used
    /// lately (keys), the bots whose icons were asked for, and whether the panel or the hint is on screen.
    slash: HashMap<String, (Instant, Arc<slash::Index>)>,
    /// What people listen to on Spotify: the track and when it started.
    spotify: HashMap<String, (String, i64)>,
    slash_rows: Vec<slash::Row>,
    slash_recent: Vec<String>,
    slash_icons_asked: HashSet<String>,
    slash_shown: bool,
    /// Changes when a recording ends, so its timer stops.
    rec_generation: Arc<std::sync::atomic::AtomicU64>,
    /// The soundboard: Discord's own sounds (asked once), each server's as last asked, the ones played
    /// here lately (ids), what's typed in its search, the sounds others played (decoded, the last few),
    /// and whether those are heard at all.
    sound_defaults: Option<Vec<soundboard::Sound>>,
    sound_servers: HashMap<String, (Instant, Vec<soundboard::Sound>)>,
    sound_recent: Vec<String>,
    sound_query: String,
    sounds_kept: Arc<Mutex<soundboard::Kept<Arc<Vec<i16>>>>>,
    sounds_muted: bool,
}

impl Worker {
    async fn on_event(&mut self, event: Event) {
        match event {
            Event::Ready(data) => self.on_ready(data).await,
            Event::MessageCreate(m) => self.on_message(m),
            Event::VoiceState(state) => self.on_voice_state(state).await,
            Event::VoiceServer(data) => {
                let _ = self.voice.send(voice::VoiceCmd::DiscordServer(data));
            }
            Event::Stream(kind, data) if kind == "STREAM_UPDATE" => {
                let owner = data["stream_key"].as_str().and_then(|k| k.rsplit(':').next()).map(String::from);
                let viewers = data["viewer_ids"].as_array().map_or(0, Vec::len) as i32;
                if let Some(owner) = owner {
                    STREAM_VIEWERS.lock().unwrap().insert(owner.clone(), viewers);
                    on_ui(&self.ui, move |ui| {
                        if ui.get_watched_identity().as_str() == owner {
                            ui.set_stream_viewers(viewers);
                        }
                    });
                }
            }
            Event::Stream(kind, data) => {
                let _ = self.voice.send(voice::VoiceCmd::DiscordStream(kind, data));
            }
            Event::Home(kind, data) if kind == "GUILD_CREATE" => self.guild_appeared(data),
            // Only a server we are out of: "unavailable" is an outage at Discord.
            Event::Home(kind, data) if kind == "GUILD_DELETE" && data["unavailable"].as_bool() != Some(true) => {
                if let Some(id) = data["id"].as_str() {
                    let _ = self.guild_gone(id.to_string()).await;
                }
            }
            Event::Home(kind, data) => self.on_home_event(&kind, data),
            Event::Chat(kind, data) => self.on_chat_event(&kind, data),
            Event::Resumed => {
                netcheck::set_down(&self.ui, false);
                on_ui(&self.ui, |ui| {
                    ui.set_status("".into());
                    ui.set_online(true);
                })
            }
            Event::Offline => netcheck::set_down(&self.ui, true),
            Event::Status(text) => on_ui(&self.ui, move |ui| {
                ui.set_status(text.into());
                ui.set_online(false);
            }),
        }
    }

    async fn on_cmd(&mut self, cmd: Cmd) {
        let result = match cmd {
            // A click still in the queue when another server has been clicked since.
            Cmd::SelectGuild(id) if *LAST_GUILD_CLICK.lock().unwrap() != id => Ok(()),
            Cmd::SelectGuild(id) => match self.select_guild(id.clone()).await {
                Ok(()) => self.open_left_chat(id).await,
                failed => failed,
            },
            Cmd::SelectChannel(id) if click_is_stale(&id) => Ok(()),
            Cmd::SelectChannel(id) => self.select_channel(id).await,
            Cmd::GoBack => {
                let to = self.trail.go_back();
                self.walk(to).await
            }
            Cmd::GoAhead => {
                let to = self.trail.go_ahead();
                self.walk(to).await
            }
            Cmd::Send(text) => self.send(text).await,
            // Only meaningful on the sign-in screen, which is gone once we're here.
            Cmd::Login(_) => Ok(()),
            Cmd::JoinVoice(id) => self.join_voice(id),
            Cmd::VoiceReconnect => self.rejoin_voice(),
            Cmd::DmCall { ring } => self.join_dm_call(ring),
            Cmd::DmCallDecline => {
                self.dm_call_declined.extend(self.channel.clone());
                self.show_dm_call();
                Ok(())
            }
            Cmd::LeaveVoice => {
                self.voice_channel = None;
                self.ring_pending = None;
                self.show_dm_call();
                self.voice.send(voice::VoiceCmd::Leave).map_err(|e| e.to_string())
            }
            Cmd::Voice(cmd) => self.voice.send(cmd).map_err(|e| e.to_string()),
            Cmd::VoiceSwitches(muted, deafened) => {
                self.voice.send(voice::VoiceCmd::Switches { muted, deafened }).map_err(|e| e.to_string())
            }
            Cmd::PersonOpen(id) => self.person_open(id).await,
            Cmd::PersonAccept(id) => self.api.accept_friend(&snowflake(&id).unwrap_or_default().to_string()).await.map(|_| ()),
            Cmd::PersonRemove(id) => self.api.remove_friend(&snowflake(&id).unwrap_or_default().to_string()).await.map(|_| ()),
            Cmd::AddFriend(name) => {
                let name = name.trim().trim_start_matches('@').to_string();
                let result = if name.is_empty() {
                    String::new()
                } else {
                    match self.api.add_friend(&name).await {
                        Ok(_) => i18n::tf("Заявка отправлена: {}", &[&name]),
                        Err(e) => home::add_friend_error(&e),
                    }
                };
                on_ui(&self.ui, move |ui| ui.set_add_friend_result(result.into()));
                Ok(())
            }
            Cmd::Search(text) => {
                self.search(&text);
                Ok(())
            }
            Cmd::SearchPick(index) => match self.search_results.get(index).cloned() {
                Some(entry) => {
                    self.remember_pick(&entry.target);
                    self.open_target(entry.target).await
                }
                None => Ok(()),
            },
            Cmd::FriendsTab(tab) => {
                self.friends_tab = tab;
                self.show_people();
                Ok(())
            }
            Cmd::Typing => {
                self.send_typing().await;
                Ok(())
            }
            Cmd::Privacy(key, value) => match privacy_body(&key, value) {
                Some(body) => self.change_privacy(body).await,
                None => Ok(()),
            },
            Cmd::CustomStatus(text) => {
                let text: String = text.trim().chars().take(128).collect();
                let status = if text.is_empty() { Value::Null } else { json!({ "text": text }) };
                self.change_privacy(json!({ "custom_status": status })).await
            }
            Cmd::ProfilePicture(kind) => self.profile_picture(&kind).await,
            Cmd::ProfileName(text) => {
                let name: String = text.trim().chars().take(32).collect();
                let name = if name.is_empty() { Value::Null } else { Value::String(name) };
                self.change_profile("/users/@me", json!({ "global_name": name })).await
            }
            Cmd::ProfilePronouns(text) => {
                let pronouns: String = text.trim().chars().take(40).collect();
                self.change_profile("/users/@me/profile", json!({ "pronouns": pronouns })).await
            }
            Cmd::Bio(text) => {
                let text: String = text.trim().chars().take(190).collect();
                self.api.patch("/users/@me/profile", json!({ "bio": text })).await.map(|_| {
                    notice(&self.ui, i18n::t("«О себе» сохранено"));
                })
            }
            Cmd::FriendSources(all, friends, guilds) => {
                self.change_privacy(json!({ "friend_source_flags": { "all": all, "mutual_friends": friends, "mutual_guilds": guilds } })).await
            }
            Cmd::EmojiQuery(query) => self.emoji_query(query).await,
            Cmd::MoreMembers => {
                self.more_members();
                Ok(())
            }
            Cmd::InputChanged(text) => {
                self.suggest(text.clone());
                self.slash_input(&text).await;
                Ok(())
            }
            Cmd::SlashPick(i) => {
                self.slash_pick(i);
                Ok(())
            }
            Cmd::SoundboardOpen => {
                self.sound_query.clear();
                self.load_sounds().await;
                self.show_sounds();
                Ok(())
            }
            Cmd::SoundQuery(text) => {
                self.sound_query = text;
                self.show_sounds();
                Ok(())
            }
            Cmd::SoundPlay(id, guild) => self.play_sound(id, guild).await,
            Cmd::SoundsMuted(muted) => {
                self.sounds_muted = muted;
                prefs::write("sounds_muted", json!(muted));
                Ok(())
            }
            Cmd::PickSuggest(i) => {
                self.pick_suggestion(i);
                Ok(())
            }
            Cmd::React(message, key) => self.react(message, key).await,
            Cmd::PollVote(message, answer) => self.poll_vote(message, answer).await,
            Cmd::ComponentClick(message, custom) => self.component_click(message, custom).await,
            Cmd::SelectOpen(message, custom) => {
                self.select_open(&message, &custom);
                Ok(())
            }
            Cmd::ComponentSelect(message, custom, values) => self.component_select(message, custom, values).await,
            Cmd::Paste(list) => self.paste(list).await,
            Cmd::PendingRemove(i) => {
                if i < self.pending_files.len() {
                    self.pending_files.remove(i);
                }
                self.show_pending();
                Ok(())
            }
            Cmd::EventsOpen => self.events_open().await,
            Cmd::EventCreateOpen => {
                self.event_create_open();
                Ok(())
            }
            Cmd::EventInterest(id, on) => self.event_interest(id, on).await,
            Cmd::EventDelete(id) => self.event_delete(id).await,
            Cmd::EventCreate(draft) => self.event_create(draft).await,
            Cmd::VoicePlay(message) => self.voice_play(message).await,
            Cmd::CopyMessage(id) => {
                if let Some(text) = self.raw.get(&id) {
                    copy_text(text);
                }
                Ok(())
            }
            Cmd::Reply(id, text) => self.reply(id, text).await,
            Cmd::ShowPins => self.show_pins().await,
            Cmd::ShowMentions => self.show_mentions().await,
            Cmd::LoadNewer => self.load_newer().await,
            Cmd::ReturnLatest => self.return_latest().await,
            Cmd::AttachFile => self.attach_file().await,
            Cmd::BeginEdit(id) => {
                let text = self.raw.get(&id).cloned().unwrap_or_default();
                on_ui(&self.ui, move |ui| ui.invoke_start_edit(id.into(), text.into()));
                Ok(())
            }
            Cmd::EditMessage(id, text) => match self.channel.clone() {
                Some(channel) => self.api.edit_message(&channel, &id, &text).await.map(|_| ()),
                None => Ok(()),
            },
            Cmd::CopyLink(id) => {
                if let Some(channel) = self.channel.clone() {
                    let place = match self.guild.as_deref() {
                        Some(g) if g != HOME => g.to_string(),
                        _ => HOME.to_string(),
                    };
                    copy_text(&format!("https://discord.com/channels/{place}/{channel}/{id}"));
                }
                Ok(())
            }
            Cmd::LoadOlder => self.load_older().await,
            Cmd::FindMessages(text) => self.find_messages(text).await,
            Cmd::JumpTo(channel, message) => self.jump_to(channel, message).await,
            Cmd::UserMenu(id) => self.user_menu(id).await,
            Cmd::DmMenu(channel) => self.dm_menu(channel).await,
            Cmd::FullProfile(id) => self.full_profile(id).await,
            Cmd::UserCard(id, popup) => self.user_card(id, popup).await,
            Cmd::OpenChat(guild, channel) => {
                let target = if guild.is_empty() { search::Target::Dm(channel) } else { search::Target::Channel(guild, channel) };
                self.open_target(target).await
            }
            Cmd::CardMessage(id, text) => {
                self.people_page = 0;
                match self.person_open(id).await {
                    Ok(()) => self.send(text).await,
                    Err(e) => Err(e),
                }
            }
            Cmd::ViewImage(url) => {
                view_image(self.images.clone(), self.ui.clone(), url);
                Ok(())
            }
            Cmd::SaveImage(url) => {
                save_image(self.images.clone(), self.ui.clone(), url);
                Ok(())
            }
            Cmd::UserAction(action, id) => self.user_action(&action, id).await,
            Cmd::UserVolume(id, percent) => {
                audio::set_user_volume(&id, percent);
                Ok(())
            }
            Cmd::SaveNote(user, text) => self.save_note(user, text).await,
            Cmd::GifQuery(query) => self.gif_query(query).await,
            Cmd::GifMore => {
                self.show_more_gifs();
                Ok(())
            }
            Cmd::SendGif(url) => self.send_gif(url).await,
            Cmd::ForwardOpen(id) => {
                self.forward_open(id);
                Ok(())
            }
            Cmd::ForwardQuery(text) => {
                self.show_forward_targets(&text);
                Ok(())
            }
            Cmd::ForwardTo(dest) => self.forward_to(dest).await,
            Cmd::NotifyMenu(guild, channel) => {
                self.notify_menu(guild, channel);
                Ok(())
            }
            Cmd::NotifyChange(guild, channel, what) => self.notify_change(guild, channel, &what).await,
            Cmd::InviteLookup(text) => {
                self.invite_lookup(&text).await;
                Ok(())
            }
            Cmd::InviteJoin(code) => {
                self.invite_join(&code).await;
                Ok(())
            }
            Cmd::CreateInvite(channel) => {
                self.create_invite(&channel).await;
                Ok(())
            }
            Cmd::InviteToGuild(guild) => {
                self.invite_window(&guild).await;
                Ok(())
            }
            Cmd::InviteFriend(user, link) => {
                let sent = self.send_link(&user, &link).await;
                let kind = if sent.is_ok() { 2 } else { 0 };
                on_ui(&self.ui, move |ui| change_invite_person(&ui, &user, |p| p.kind = kind));
                if let Err(e) = sent {
                    notice(&self.ui, discord::refusal(&e));
                }
                Ok(())
            }
            Cmd::CreateThread(message, name) => {
                self.create_thread(&message, &name).await;
                Ok(())
            }
            Cmd::CreatePost(forum, title, text) => {
                self.create_post(&forum, &title, &text).await;
                Ok(())
            }
            Cmd::RecordStart => {
                self.record_start();
                Ok(())
            }
            Cmd::RecordSend => {
                self.record_send().await;
                Ok(())
            }
            Cmd::RecordCancel => {
                self.record_stop();
                Ok(())
            }
        };
        if let Err(e) = result {
            on_ui(&self.ui, move |ui| ui.set_status(i18n::tf("Ошибка: {}", &[&discord::friendly(&e)]).into()));
        }
    }

    async fn on_ready(&mut self, data: Value) {
        let guilds = data["guilds"].as_array().cloned().unwrap_or_default();
        for g in &guilds {
            names::note_roles(g["roles"].as_array().map(Vec::as_slice).unwrap_or_default());
            names::note_channels(g["channels"].as_array().map(Vec::as_slice).unwrap_or_default());
        }
        let named = guilds.iter().filter(|g| g["name"].is_string() || g["properties"]["name"].is_string()).count();
        log_line(&format!(
            "ready: {} servers ({} with a name), {} direct chats, {} friends",
            guilds.len(),
            named,
            data["private_channels"].as_array().map_or(0, |a| a.len()),
            data["relationships"].as_array().map_or(0, |a| a.len())
        ));
        self.index_voice(&data);
        self.presences.extend(home::presences_in(&data));
                self.note_spotify(home::spotify_in(&data));
        self.unread = unread::Unread::from_ready(&data);
        for r in data["relationships"].as_array().into_iter().flatten() {
            if let Some(id) = r["user"]["id"].as_str() {
                note_user(&r["user"]);
                self.users.insert(id.to_string(), r["user"].clone());
            }
        }
        // Voice states may still name the session this replaces: those are not another client.
        if let Some(old) = self.ready["session_id"].as_str() {
            self.past_sessions.push(old.to_string());
        }
        self.ready = data;

        let mut list = vec![Guild { id: HOME.into(), name: i18n::t("Личные сообщения"), ..Default::default() }];
        let mut jobs = Vec::new();
        for g in &guilds {
            // Newer READY payloads nest guild metadata under `properties`.
            let p = if g["properties"].is_object() { &g["properties"] } else { g };
            let id = g["id"].as_str().unwrap_or("").to_string();
            if let Some(hash) = p["icon"].as_str() {
                let url = format!("{CDN}/icons/{id}/{hash}.png?size=128");
                jobs.push(ImageJob { row: list.len(), url, w: GUILD_ICON_PX, h: GUILD_ICON_PX, ..Default::default() });
            }
            let name = readable(p["name"].as_str().unwrap_or(""));
            self.guild_names.insert(id.clone(), name.clone());
            let badge = self.unread.guild(&id);
            list.push(Guild { id, name, badge });
        }
        list[0].badge = self.unread.dms(&self.dm_ids());

        on_ui(&self.ui, move |ui| {
            let items: Vec<GuildItem> = list.iter().map(Guild::item).collect();
            ui.set_guilds(ModelRc::new(VecModel::from(items)));
            sync_rail(&ui);
            ui.set_status(i18n::t("В сети").into());
            ui.set_online(true);
            ui.set_net_down(false);
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let row = job.row;
            on_ui(&ui, move |ui| {
                update_row(ui.get_guilds(), row, |g| {
                    g.icon = Image::from_rgba8(pixels);
                    g.has_icon = true;
                });
                sync_rail(&ui);
            });
        });

        let sessions = self.ready["sessions"].clone();
        self.show_my_activity(&sessions);
        if self.guild.is_none() {
            self.open_last_chat().await;
        }
        if !self.preloaded {
            self.preloaded = true;
            preload_chats(self.api.clone(), self.pages.clone(), self.chats_to_preload());
        }
        // The start window may give way to the client now.
        CLIENT_READY.notify_one();
    }

    /// Where the last run ended: that chat again, as long as it is still there.
    async fn open_last_chat(&mut self) {
        let last = pages::recent().into_iter().next().filter(|(guild, _)| guild == HOME || self.guild_names.contains_key(guild));
        let Some((guild, channel)) = last else {
            let _ = self.select_guild(HOME.into()).await;
            return;
        };
        if self.select_guild(guild.clone()).await.is_err() {
            let _ = self.select_guild(HOME.into()).await;
            return;
        }
        if self.channels.contains_key(&channel) {
            if let Err(e) = self.walk(Some((guild, channel))).await {
                log_line(&format!("last chat: {e}"));
            }
        }
    }

    /// The chats opened lately that are still among ours, other than the one open now.
    fn chats_to_preload(&self) -> Vec<String> {
        let known = |channel: &str| {
            let has = |list: &Value| list.as_array().is_some_and(|l| l.iter().any(|c| c["id"].as_str() == Some(channel)));
            has(&self.ready["private_channels"]) || self.ready["guilds"].as_array().is_some_and(|g| g.iter().any(|g| has(&g["channels"])))
        };
        pages::recent().into_iter().map(|(_, channel)| channel).filter(|c| Some(c) != self.channel.as_ref() && known(c)).collect()
    }

    async fn select_guild(&mut self, id: String) -> Result<(), String> {
        self.guild = Some(id.clone());
        self.people_page = 0;
        // Another server's members arrive once one of its text channels opens.
        self.watch_members("", false).await;
        if id == HOME {
            let mut jobs = Vec::new();
            let rows = self.dm_rows(&mut jobs);
            self.load_banner(&id);
            self.show_sidebar(&id, i18n::t("Личные сообщения"), rows, jobs);
            self.push_discord_voice().await;
            return Ok(());
        }
        let opening = Instant::now();
        let seen = chat_clicks();
        // What is known already goes on screen at once: kept from an earlier visit, or from READY.
        let cached = self.guild_cache.get(&id).map(|(at, list, me)| (at.elapsed() < GUILD_CACHE_FOR && me.is_some(), list.clone(), me.clone()));
        let (channels_fresh, kept_list, kept_me) = match cached {
            Some((fresh, list, me)) => (fresh, Some(list), me),
            None => (false, self.ready_channels(&id), None),
        };
        let kept_me = kept_me.or_else(|| self.ready_perms(&id));
        let threads_fresh = self.active_threads.get(&id).is_some_and(|(at, _)| at.elapsed() < THREADS_CACHE_FOR);
        let kept_threads = self.active_threads.get(&id).map(|(_, found)| found.clone());
        let mut shown = None;
        if let (Some(list), Some(me)) = (&kept_list, &kept_me) {
            let rows = self.guild_rows(&id, list, Some(me), kept_threads.as_ref());
            shown = Some(rows_signature(&rows));
            let title = self.guild_names.get(&id).cloned().unwrap_or_default();
            self.show_sidebar(&id, title, rows, Vec::new());
            self.load_banner(&id);
            log_line(&format!("server open: kept channels on screen after {} ms", opening.elapsed().as_millis()));
            tokio::select! {
                _ = self.push_discord_voice() => {}
                _ = clicked_since(seen) => return Ok(()),
            }
        }
        // Then what Discord says now, all asked at once; a click elsewhere drops it.
        if !(channels_fresh && threads_fresh) {
            let channels_path = format!("/guilds/{}/channels", snowflake(&id)?);
            let threads_path = format!("/guilds/{}/threads/active", snowflake(&id)?);
            let fetched = {
                let this = &*self;
                let asked = async {
                    tokio::join!(
                        async {
                            if channels_fresh {
                                None
                            } else {
                                Some(this.api.get(&channels_path).await)
                            }
                        },
                        async {
                            if channels_fresh {
                                None
                            } else {
                                this.my_perms(&id).await
                            }
                        },
                        async {
                            if threads_fresh {
                                None
                            } else {
                                this.api.get(&threads_path).await.ok()
                            }
                        },
                    )
                };
                tokio::select! {
                    answers = asked => Some(answers),
                    _ = clicked_since(seen) => None,
                }
            };
            let Some((channels, me, threads)) = fetched else { return Ok(()) };
            log_line(&format!("server open: Discord answered after {} ms", opening.elapsed().as_millis()));
            if self.guild.as_deref() != Some(id.as_str()) {
                return Ok(()); // user clicked elsewhere meanwhile
            }
            let (list, me) = match channels {
                Some(Ok(list)) => {
                    names::note_channels(list.as_array().map(Vec::as_slice).unwrap_or_default());
                    let me = me.or(kept_me);
                    self.guild_cache.insert(id.clone(), (Instant::now(), list.clone(), me.clone()));
                    (list, me)
                }
                Some(Err(e)) => {
                    let Some(list) = kept_list.filter(|_| shown.is_some()) else { return Err(e) };
                    log_line(&format!("channels of {id}: {e}"));
                    (list, kept_me)
                }
                None => (kept_list.unwrap_or_default(), kept_me),
            };
            if let Some(found) = &threads {
                self.active_threads.insert(id.clone(), (Instant::now(), found.clone()));
            }
            let rows = self.guild_rows(&id, &list, me.as_ref(), threads.as_ref().or(kept_threads.as_ref()));
            // A list that didn't change isn't drawn again.
            if shown.as_ref() != Some(&rows_signature(&rows)) {
                let first = shown.is_none();
                let title = self.guild_names.get(&id).cloned().unwrap_or_default();
                self.show_sidebar(&id, title, rows, Vec::new());
                if first {
                    self.load_banner(&id);
                }
                tokio::select! {
                    _ = self.push_discord_voice() => {}
                    _ = clicked_since(seen) => return Ok(()),
                }
            }
        }
        tokio::select! {
            _ = self.refresh_events(&id, false) => {}
            _ = clicked_since(seen) => {}
        }
        Ok(())
    }

    /// A server's channels as READY told them, before anything is asked.
    fn ready_channels(&self, gid: &str) -> Option<Value> {
        let guilds = self.ready["guilds"].as_array()?;
        let channels = &guilds.iter().find(|g| g["id"].as_str() == Some(gid))?["channels"];
        channels.as_array().is_some_and(|list| !list.is_empty()).then(|| channels.clone())
    }

    /// Our permissions in a server as READY tells them, good until the server's own word arrives.
    fn ready_perms(&self, gid: &str) -> Option<perms::Me> {
        let guilds = self.ready["guilds"].as_array()?;
        let index = guilds.iter().position(|g| g["id"].as_str() == Some(gid))?;
        let g = &guilds[index];
        let me = self.me_id.as_str();
        let owner = g["owner_id"].as_str().or(g["properties"]["owner_id"].as_str()) == Some(me);
        let roles = g["roles"].as_array().filter(|r| !r.is_empty())?;
        let member = self.ready["merged_members"][index]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["user_id"].as_str() == Some(me)))
            .or_else(|| g["members"].as_array()?.iter().find(|m| m["user"]["id"].as_str() == Some(me)))?;
        let mine: Vec<String> = member["roles"].as_array()?.iter().filter_map(|r| r.as_str().map(String::from)).collect();
        Some(perms::Me::new(gid, me, owner, roles, mine))
    }

    /// The rows of a server's channel list, and what is noted about its channels on the way.
    fn guild_rows(&mut self, id: &str, list: &Value, me: Option<&perms::Me>, threads: Option<&Value>) -> Vec<Channel> {
        let mut list = list.as_array().cloned().unwrap_or_default();
        // Hide what we may not see, as Discord does; without our roles, show everything.
        let all = list.len();
        if let Some(me) = me {
            let visible: Vec<Value> = list.iter().filter(|c| me.can_view(c)).cloned().collect();
            if visible.is_empty() && all > 0 {
                // A server where we see nothing at all is far more likely our mistake than the server's: show the list, Discord refuses what is closed.
                log_line(&format!("channels of {id}: none of {all} visible by our reckoning ({} roles of ours): showing all", me.roles.len()));
            } else {
                list = visible;
                log_line(&format!("channels of {id}: {} of {all} visible ({} roles of ours)", list.len(), me.roles.len()));
            }
        } else {
            log_line(&format!("channels of {id}: all {all} shown (our roles are not known)"));
        }
        log_odd_chars(list.iter().filter_map(|c| c["name"].as_str().map(String::from)));
        let mut rooms: Vec<&Value> = list.iter().filter(|c| matches!(c["type"].as_u64(), Some(2 | 13))).collect();
        rooms.sort_by_key(|c| c["position"].as_i64().unwrap_or(0));
        let rooms = rooms.iter().map(|c| (c["id"].as_str().unwrap_or("").to_string(), tidy_name(&readable(c["name"].as_str().unwrap_or(""))))).collect();
        self.voice_rooms.insert(id.to_string(), rooms);
        self.guild_server = list
            .iter()
            .find_map(|c| stream_server(c["topic"].as_str().unwrap_or("")))
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());
        for c in &list {
            if let Some(cid) = c["id"].as_str() {
                let info = ChannelInfo {
                    name: tidy_name(&readable(c["name"].as_str().unwrap_or(""))),
                    topic: readable(c["topic"].as_str().unwrap_or("")),
                    dm: false,
                    forum: matches!(c["type"].as_u64(), Some(15 | 16)),
                };
                self.channels.insert(cid.into(), info);
            }
        }
        for c in &list {
            let last = c["last_message_id"].as_str().and_then(|s| s.parse().ok());
            if let (Some(cid), Some(last)) = (c["id"].as_str(), last) {
                self.unread.note_last(cid, id, last);
            }
        }
        let mut rows = channel_rows(&list);
        // Threads that are open now, under the channel they belong to (a quiet server may have none).
        if let Some(found) = threads {
            let threads: Vec<Value> = found["threads"].as_array().cloned().unwrap_or_default();
            for (parent, thread) in threads_by_parent(&threads) {
                let Some(at) = rows.iter().position(|r| r.id == parent) else { continue };
                let after = at + 1 + rows[at + 1..].iter().take_while(|r| r.is_thread).count();
                let (Some(tid), Some(name)) = (thread["id"].as_str(), thread["name"].as_str()) else { continue };
                self.channels.insert(tid.into(), ChannelInfo { name: readable(name), topic: String::new(), dm: false, forum: false });
                self.thread_ids.insert(tid.to_string());
                // Forum posts live in the forum's own list.
                if rows[at].kind == 0 {
                    rows.insert(after, Channel { id: tid.into(), name: format!("↳ {}", readable(name)), kind: 0, clickable: true, is_thread: true, ..Default::default() });
                }
            }
        }
        for r in rows.iter_mut().filter(|r| r.kind == 0) {
            r.unread = self.unread.is_unread(&r.id);
            r.badge = self.unread.mentions(&r.id) as i32;
            r.muted = self.unread.settings.muted(&r.id, Some(id));
        }
        let folded = fold::marks(rows.iter().map(|r| (r.kind, r.id.as_str())), &fold::load());
        for (r, f) in rows.iter_mut().zip(folded) {
            r.folded = f;
        }
        rows
    }

    /// The channel list on screen; `jobs` are its pictures, loaded after it.
    fn show_sidebar(&self, id: &str, title: String, rows: Vec<Channel>, jobs: Vec<ImageJob>) {
        let (shown, selected) = (id.to_string(), id.to_string());
        on_ui(&self.ui, move |ui| {
            let items: Vec<ChannelItem> = rows.iter().map(Channel::item).collect();
            ui.set_selected_guild(selected.into());
            ui.set_sidebar_title(title.into());
            ui.set_channels(ModelRc::new(VecModel::from(items)));
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let (row, shown) = (job.row, shown.clone());
            on_ui(&ui, move |ui| {
                if ui.get_selected_guild().as_str() == shown {
                    update_row(ui.get_channels(), row, |c| c.avatar = Image::from_rgba8(pixels));
                }
            });
        });
    }

    /// Our permissions in a server, kept with its channels.
    async fn guild_perms(&self, gid: &str) -> Option<perms::Me> {
        match self.guild_cache.get(gid) {
            Some((at, _, Some(me))) if at.elapsed() < GUILD_CACHE_FOR => Some(me.clone()),
            _ => self.my_perms(gid).await,
        }
    }

    /// Our roles and base permissions in a server: from READY when it has them,
    /// else asked from Discord. None when neither works.
    async fn my_perms(&self, gid: &str) -> Option<perms::Me> {
        let guilds = self.ready["guilds"].as_array()?;
        let index = guilds.iter().position(|g| g["id"].as_str() == Some(gid))?;
        let g = &guilds[index];
        let owner = g["owner_id"].as_str().or(g["properties"]["owner_id"].as_str()) == Some(self.me_id.as_str());

        let roles = match g["roles"].as_array() {
            Some(r) if !r.is_empty() => r.clone(),
            _ => self.api.get(&format!("/guilds/{gid}/roles")).await.ok()?.as_array()?.clone(),
        };
        names::note_roles(&roles);
        let me = self.me_id.as_str();
        // Our own member, as the server itself tells it: READY lists members per server in an order that is easy to
        // misread, and wrong roles hide channels we may see.
        let from_server = self.api.get(&format!("/users/@me/guilds/{gid}/member")).await.ok().filter(|m| m["roles"].is_array());
        let from_ready = self.ready["merged_members"][index]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["user_id"].as_str() == Some(me)).cloned())
            .or_else(|| g["members"].as_array()?.iter().find(|m| m["user"]["id"].as_str() == Some(me)).cloned());
        let member = match (from_server, from_ready) {
            (Some(m), ready) => {
                let (a, b) = (m["roles"].as_array().map_or(0, Vec::len), ready.as_ref().and_then(|r| r["roles"].as_array()).map_or(0, Vec::len));
                if a != b {
                    log_line(&format!("roles in {gid}: {a} from the server, {b} from READY (the server is right)"));
                }
                m
            }
            (None, Some(m)) => m,
            (None, None) => self.api.get(&format!("/guilds/{gid}/members/{me}")).await.ok()?,
        };
        let mine: Vec<String> = member["roles"].as_array()?.iter().filter_map(|r| r.as_str().map(String::from)).collect();
        Some(perms::Me::new(gid, me, owner, &roles, mine))
    }

    /// Ids of the DMs and group DMs (not message requests).
    fn dm_ids(&self) -> Vec<String> {
        let private = self.ready["private_channels"].as_array().map(Vec::as_slice).unwrap_or_default();
        private.iter().filter(|c| !home::is_request(c)).filter_map(|c| c["id"].as_str().map(String::from)).collect()
    }

    /// Brings every unread pill and mention count on screen up to date:
    /// server icons, and the channels or DMs of the open list.
    /// The chats waiting for us, on the rail under the DM button, newest first. Muted ones wait quietly in the list.
    fn refresh_dm_alerts(&self) {
        let open = self.dm_ids();
        let waiting: Vec<&String> = open.iter().filter(|c| self.unread.mentions(c) > 0 && !self.unread.muted(c)).collect();
        let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        let alerts: Vec<(String, String, i32, Option<String>)> = if waiting.is_empty() {
            Vec::new()
        } else {
            home::dm_rows(&private, &self.presences, &self.me_name, &look)
                .into_iter()
                .filter(|dm| waiting.contains(&&dm.id))
                .map(|dm| {
                    let mentions = self.unread.mentions(&dm.id) as i32;
                    (dm.id, readable(&dm.name), mentions, dm.avatar_url)
                })
                .collect()
        };
        let pictures: Vec<(String, String)> = alerts.iter().filter_map(|(id, _, _, url)| Some((id.clone(), url.clone()?))).collect();
        on_ui(&self.ui, move |ui| {
            let old = ui.get_dm_alerts();
            let kept = |id: &str| old.iter().find(|a| a.id.as_str() == id);
            let items: Vec<GuildItem> = alerts
                .into_iter()
                .map(|(id, name, mentions, _)| {
                    let was = kept(&id);
                    GuildItem {
                        initials: name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                        id: id.into(),
                        name: name.into(),
                        icon: was.as_ref().map(|w| w.icon.clone()).unwrap_or_default(),
                        has_icon: was.as_ref().is_some_and(|w| w.has_icon),
                        unread: true,
                        mentions,
                        ..Default::default()
                    }
                })
                .collect();
            ui.set_dm_alerts(ModelRc::new(VecModel::from(items)));
        });
        let (images, ui) = (self.images.clone(), self.ui.clone());
        tokio::spawn(async move {
            for (id, url) in pictures {
                let Some(px) = images.get(url, GUILD_ICON_PX, GUILD_ICON_PX).await else { continue };
                on_ui(&ui, move |ui| {
                    let model = ui.get_dm_alerts();
                    if let Some(row) = model.iter().position(|a| a.id.as_str() == id && !a.has_icon) {
                        update_row(model, row, |a| {
                            a.icon = Image::from_rgba8(px);
                            a.has_icon = true;
                        });
                    }
                });
            }
        });
    }

    fn refresh_badges(&self) {
        self.refresh_dm_alerts();
        let mut badges: HashMap<String, unread::Badge> = HashMap::new();
        badges.insert(HOME.into(), self.unread.dms(&self.dm_ids()));
        for gid in self.guild_names.keys() {
            badges.insert(gid.clone(), self.unread.guild(gid));
        }
        let channels: HashMap<String, (bool, i32, bool)> = self
            .unread_channels()
            .into_iter()
            .map(|cid| (cid.clone(), (self.unread.is_unread(&cid), self.unread.mentions(&cid) as i32, self.unread.muted(&cid))))
            .collect();
        // The window's title carries the count of mentions, so it shows in the taskbar and Alt+Tab.
        let total: u32 = badges.values().map(|b| b.mentions).sum();
        on_ui(&self.ui, move |ui| {
            ui.set_title_prefix(if total > 0 { format!("({total}) ") } else { String::new() }.into());
            // The same number on the taskbar button, redrawn only when it changes.
            static SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
            if SHOWN.swap(total, std::sync::atomic::Ordering::Relaxed) != total {
                use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
                use slint::winit_030::WinitWindowAccessor;
                let hwnd = ui
                    .window()
                    .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
                        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
                        _ => None,
                    })
                    .flatten();
                if let Some(hwnd) = hwnd {
                    system::set_overlay(hwnd, total);
                }
            }
            let guilds = ui.get_guilds();
            for row in 0..guilds.row_count() {
                let Some(g) = guilds.row_data(row) else { continue };
                let badge = badges.get(g.id.as_str()).copied().unwrap_or_default();
                if g.unread != badge.unread || g.mentions != badge.mentions as i32 {
                    update_row(guilds.clone(), row, |g| {
                        g.unread = badge.unread;
                        g.mentions = badge.mentions as i32;
                    });
                }
            }
            sync_rail(&ui);
            let list = ui.get_channels();
            for row in 0..list.row_count() {
                let Some(c) = list.row_data(row) else { continue };
                // Only channels and DMs: page rows keep their own counts.
                let Some(&(unread, mentions, muted)) = channels.get(c.id.as_str()) else { continue };
                if c.kind == 0 && (c.unread != unread || c.badge != mentions || c.muted != muted) {
                    update_row(list.clone(), row, |c| {
                        c.unread = unread;
                        c.badge = mentions;
                        c.muted = muted;
                    });
                }
            }
        });
    }

    /// The channels of the list on screen whose badges may change.
    fn unread_channels(&self) -> Vec<String> {
        if self.guild.as_deref() == Some(HOME) {
            return self.dm_ids();
        }
        self.channels.iter().filter(|(_, info)| !info.dm).map(|(id, _)| id.clone()).collect()
    }

    /// We read `channel` up to its newest message: here, and for Discord
    /// (so the phone and the official app agree), as any client does on opening it.
    async fn read_channel(&mut self, channel: &str, newest: u64) {
        let had = self.unread.is_unread(channel) || self.unread.mentions(channel) > 0;
        self.unread.mark_read(channel, newest);
        self.refresh_badges();
        let newest = self.unread.last_message(channel).max(newest);
        if had && newest > 0 {
            let (api, path) = (self.api.clone(), format!("/channels/{channel}/messages/{newest}/ack"));
            tokio::spawn(async move {
                let _ = api.post(&path, json!({ "token": null })).await;
            });
        }
    }

    /// The server's banner over its channel list (none on the DM side or without one).
    fn load_banner(&self, guild: &str) {
        on_ui(&self.ui, |ui| ui.set_has_guild_banner(false));
        let entry = self.ready["guilds"].as_array().into_iter().flatten().find(|g| g["id"].as_str() == Some(guild));
        let hash = entry.and_then(|g| if g["properties"].is_object() { g["properties"]["banner"].as_str() } else { g["banner"].as_str() });
        let Some(hash) = hash else { return };
        // Twice the sidebar's 248×136, for sharp high-DPI screens.
        let url = format!("{CDN}/banners/{guild}/{hash}.png?size=512");
        let job = ImageJob { url, w: 496, h: 272, ..Default::default() };
        let (ui, guild) = (self.ui.clone(), guild.to_string());
        load_images(self.images.clone(), vec![job], move |_, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let guild = guild.clone();
            on_ui(&ui, move |ui| {
                if ui.get_selected_guild().as_str() == guild {
                    ui.set_guild_banner(Image::from_rgba8(pixels));
                    ui.set_has_guild_banner(true);
                }
            });
        });
    }

    /// Fills the Discord voice and user caches from READY.
    fn index_voice(&mut self, data: &Value) {
        for u in data["users"].as_array().into_iter().flatten() {
            if let Some(id) = u["id"].as_str() {
                note_user(u);
                self.users.insert(id.to_string(), u.clone());
            }
        }
        for g in data["guilds"].as_array().into_iter().flatten() {
            let Some(gid) = g["id"].as_str() else { continue };
            let mut nicks = HashMap::new();
            for m in g["members"].as_array().into_iter().flatten() {
                if let Some(uid) = m["user"]["id"].as_str() {
                    note_user(&m["user"]);
                    self.users.insert(uid.to_string(), m["user"].clone());
                    if let Some(nick) = m["nick"].as_str() {
                        nicks.insert(uid.to_string(), nick.to_string());
                    }
                }
            }
            let states = self.discord_voice.entry(gid.to_string()).or_default();
            for vs in g["voice_states"].as_array().into_iter().flatten() {
                let Some(uid) = vs["user_id"].as_str() else { continue };
                let mut vs = vs.clone();
                if let Some(nick) = nicks.get(uid) {
                    vs["nick"] = nick.as_str().into();
                }
                states.insert(uid.to_string(), vs);
            }
        }
    }

    async fn on_voice_state(&mut self, data: Value) {
        // We joined voice from another client, say the official app: only one can hold it, so this one leaves.
        let session = self.ready["session_id"].as_str().unwrap_or("");
        let ours_before = data["session_id"].as_str().is_some_and(|s| self.past_sessions.iter().any(|old| old == s));
        if self.voice_channel.is_some() && !ours_before && discord_voice::taken_elsewhere(&data, &self.me_id, session) {
            log_line("voice: another client of this account joined voice, leaving here");
            self.voice_channel = None;
            self.ring_pending = None;
            self.show_dm_call();
            let _ = self.voice.send(voice::VoiceCmd::TakenElsewhere);
            on_ui(&self.ui, |ui| ui.set_voice_taken(true));
        }
        // Without a guild it's a call in a DM or group: who is in it is kept, and shown above that chat.
        if data["guild_id"].is_null() {
            if let Some(uid) = data["user_id"].as_str() {
                self.on_dm_voice_state(uid.to_string(), &data).await;
            }
            return;
        }
        let (Some(gid), Some(uid)) = (data["guild_id"].as_str(), data["user_id"].as_str()) else { return };
        let (gid, uid) = (gid.to_string(), uid.to_string());
        // Ours carries the session id our own Discord voice call needs.
        if uid == self.me_id {
            let _ = self.voice.send(voice::VoiceCmd::DiscordState(data.clone()));
        }
        if data["member"]["user"].is_object() {
            note_user(&data["member"]["user"]);
            self.users.insert(uid.clone(), data["member"]["user"].clone());
        }
        let states = self.discord_voice.entry(gid.clone()).or_default();
        // Someone else coming into or leaving our channel: their sound in the call.
        if uid != self.me_id {
            if let Some(ours) = self.voice_channel.as_deref() {
                let before = states.get(&uid).and_then(|s| s["channel_id"].as_str()) == Some(ours);
                let now = data["channel_id"].as_str() == Some(ours);
                if before != now {
                    let _ = self.voice.send(voice::VoiceCmd::DiscordPeer { user_id: uid.clone(), joined: now });
                }
            }
        }
        if data["channel_id"].is_null() {
            states.remove(&uid);
        } else {
            let mut vs = data.clone();
            if let Some(nick) = data["member"]["nick"].as_str() {
                vs["nick"] = nick.into();
            }
            states.insert(uid, vs);
        }
        self.push_call_people();
        if self.guild.as_deref() == Some(gid.as_str()) {
            self.push_discord_voice().await;
        }
    }

    /// The voice badges on the server icons: who is in voice on each server, and who is showing a screen.
    fn push_guild_voice(&self) {
        let me = self.me_id.clone();
        // (we are in voice there, we show a screen, someone else is in voice, someone else shows a screen)
        let summary: HashMap<String, (bool, bool, bool, bool)> = self
            .discord_voice
            .iter()
            .map(|(gid, states)| (gid.clone(), voice_summary(states, &me)))
            .collect();
        on_ui(&self.ui, move |ui| {
            let sharing = ui.get_sharing();
            let model = ui.get_guilds();
            for row in 0..model.row_count() {
                let Some(g) = model.row_data(row) else { continue };
                let voice = badge_of(summary.get(g.id.as_str()).copied().unwrap_or_default(), sharing);
                if g.voice != voice {
                    update_row(model.clone(), row, |g| g.voice = voice);
                }
            }
            sync_rail(&ui);
        });
    }

    /// Fetches still previews of Discord streams in our voice channel, each
    /// at most once a minute, for their tiles on the call screen.
    fn refresh_previews(&mut self) {
        let Some(ours) = self.voice_channel.clone() else {
            STREAM_PREVIEWS.lock().unwrap().clear();
            self.preview_at.clear();
            return;
        };
        let mut streaming = HashSet::new();
        for (gid, states) in &self.discord_voice {
            for (uid, vs) in states {
                if vs["channel_id"].as_str() != Some(ours.as_str()) || vs["self_stream"].as_bool() != Some(true) || *uid == self.me_id {
                    continue;
                }
                streaming.insert(uid.clone());
                if self.preview_at.get(uid).is_some_and(|at| at.elapsed() < PREVIEW_EVERY) {
                    continue;
                }
                self.preview_at.insert(uid.clone(), Instant::now());
                let (api, images, ui) = (self.api.clone(), self.images.clone(), self.ui.clone());
                let (key, uid) = (format!("guild:{gid}:{ours}:{uid}"), uid.clone());
                tokio::spawn(async move {
                    // 404 until Discord has made the first preview; then just wait for the next round.
                    let Ok(answer) = api.get(&format!("/streams/{key}/preview")).await else { return };
                    let Some(url) = answer["url"].as_str().map(str::to_string) else { return };
                    let job = ImageJob { url, w: 640, h: 360, ..Default::default() };
                    load_images(images, vec![job], move |_, loaded| {
                        let Some(pixels) = loaded.map(|l| l.first()) else { return };
                        STREAM_PREVIEWS.lock().unwrap().insert(uid.clone(), pixels);
                        on_ui(&ui, |ui| hide_doubles(&ui));
                    });
                });
            }
        }
        self.preview_at.retain(|uid, _| streaming.contains(uid));
        let mut previews = STREAM_PREVIEWS.lock().unwrap();
        if previews.keys().any(|uid| !streaming.contains(uid)) {
            previews.retain(|uid, _| streaming.contains(uid));
            on_ui(&self.ui, |ui| hide_doubles(&ui));
        }
    }

    /// Puts who sits in Discord's own voice under each voice channel of the open guild.
    async fn push_discord_voice(&mut self) {
        let Some(gid) = self.guild.clone().filter(|g| g != HOME) else { return };
        let states: Vec<Value> = self.discord_voice.get(&gid).map(|m| m.values().cloned().collect()).unwrap_or_default();
        // Look up anyone READY didn't include; failures are cached as a bare id.
        for vs in &states {
            let Some(uid) = vs["user_id"].as_str() else { continue };
            if !self.users.contains_key(uid) {
                let user = match snowflake(uid) {
                    Ok(id) => self.api.get(&format!("/users/{id}")).await.unwrap_or_else(|_| json!({ "id": uid })),
                    Err(_) => json!({ "id": uid }),
                };
                note_user(&user);
                self.users.insert(uid.to_string(), user);
            }
        }
        let mut by_channel: HashMap<String, Vec<(String, String, bool, bool, bool)>> = HashMap::new();
        for vs in &states {
            let (Some(cid), Some(uid)) = (vs["channel_id"].as_str(), vs["user_id"].as_str()) else { continue };
            let name = match vs["nick"].as_str() {
                Some(nick) => readable(nick),
                None => self.users.get(uid).map(display_name).unwrap_or_else(|| uid.to_string()),
            };
            let flag = |k: &str| vs[k].as_bool().unwrap_or(false);
            let entry = (uid.to_string(), name, flag("mute") || flag("self_mute"), flag("deaf") || flag("self_deaf"), flag("self_stream"));
            by_channel.entry(cid.to_string()).or_default().push(entry);
        }
        let avatars: HashMap<String, Pixels> = states
            .iter()
            .filter_map(|vs| vs["user_id"].as_str())
            .filter_map(|uid| voice_avatar(uid).map(|px| (uid.to_string(), px)))
            .collect();
        on_ui(&self.ui, move |ui| {
            if ui.get_selected_guild().as_str() != gid {
                return;
            }
            let model = ui.get_channels();
            let talking = DISCORD_TALKING.lock().unwrap().clone();
            for row in 0..model.row_count() {
                let Some(item) = model.row_data(row) else { continue };
                if item.kind != 1 {
                    continue;
                }
                let people: Vec<VoiceMember> = by_channel
                    .get(item.id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|(id, name, muted, deafened, streaming)| VoiceMember {
                        identity: id.clone().into(),
                        name: name.clone().into(),
                        initial: name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                        avatar: avatars.get(id).map(|px| Image::from_rgba8(px.clone())).unwrap_or_default(),
                        has_avatar: avatars.contains_key(id),
                        muted: *muted,
                        deafened: *deafened,
                        speaking: talking.contains(id),
                        streaming: *streaming,
                        discord: true,
                        decoration: voice_frame(id).map(Image::from_rgba8).unwrap_or_default(),
                        has_decoration: voice_frame(id).is_some(),
                    })
                    .collect();
                update_row(model.clone(), row, |c| c.people = ModelRc::new(VecModel::from(people)));
            }
            hide_doubles(&ui);
        });
    }

    /// The DM side: Friends and Message requests on top, then the DMs, newest first.
    fn dm_rows(&mut self, jobs: &mut Vec<ImageJob>) -> Vec<Channel> {
        let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
        let requests = private.iter().filter(|c| home::is_request(c)).count() as i32;
        let mut rows = vec![
            Channel { id: "friends".into(), name: i18n::t("Друзья"), kind: 3, clickable: true, ..Default::default() },
            Channel { id: "requests".into(), name: i18n::t("Запросы общения"), kind: 3, clickable: true, badge: requests, ..Default::default() },
        ];
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        // Requests are opened from their own page, but by id like any DM.
        for c in private.iter().filter(|c| home::is_request(c)) {
            let row = home::dm_row(c, &self.presences, &self.me_name, &look);
            self.channels.insert(row.id, ChannelInfo { name: readable(&row.name), topic: String::new(), dm: true, forum: false });
        }
        let (favorites, others) = usermenu::split_favorites(usermenu::pinned_first(home::dm_rows(&private, &self.presences, &self.me_name, &look), |d| d.id.as_str()), |d| d.id.as_str());
        if !favorites.is_empty() {
            rows.push(Channel { id: "fav-header".into(), name: i18n::t("Избранное"), kind: 2, ..Default::default() });
        }
        let heading_at = favorites.len();
        for (at, dm) in favorites.into_iter().chain(others).enumerate() {
            if at == heading_at {
                rows.push(Channel { id: "dm-header".into(), name: i18n::t("Личные сообщения"), kind: 2, ..Default::default() });
            }
            let name = readable(&dm.name);
            self.channels.insert(dm.id.clone(), ChannelInfo { name: name.clone(), topic: String::new(), dm: true, forum: false });
            if let Some(url) = dm.avatar_url {
                jobs.push(ImageJob { row: rows.len(), url, w: DM_AVATAR_PX, h: DM_AVATAR_PX, ..Default::default() });
            }
            let (unread, badge) = (self.unread.is_unread(&dm.id), self.unread.mentions(&dm.id) as i32);
            let muted = self.unread.muted(&dm.id);
            rows.push(Channel { id: dm.id, name, kind: 0, clickable: true, has_avatar: true, subtitle: dm.subtitle, status: dm.status, badge, unread, muted, ..Default::default() });
        }
        // Nothing but favourites, or no chats at all: the heading still stands.
        if !rows.iter().any(|r| r.id == "dm-header") {
            rows.push(Channel { id: "dm-header".into(), name: i18n::t("Личные сообщения"), kind: 2, ..Default::default() });
        }
        rows
    }

    /// Redraws the DM list if it's on screen (new DM, new message, presences).
    fn refresh_home(&mut self) {
        if self.guild.as_deref() != Some(HOME) {
            return;
        }
        if let Some(open) = self.channel.clone() {
            self.show_group_members(&open);
        }
        let mut jobs = Vec::new();
        let rows = self.dm_rows(&mut jobs);
        on_ui(&self.ui, move |ui| {
            if ui.get_selected_guild().as_str() != HOME {
                return;
            }
            let model = ui.get_channels();
            let items: Vec<ChannelItem> = rows.iter().map(Channel::item).collect();
            // Same rows in the same order: update them in place (keeps avatars and scroll).
            let same_order = model.row_count() == items.len() && (0..items.len()).all(|i| model.row_data(i).is_some_and(|old| old.id == items[i].id));
            if same_order {
                for (i, mut item) in items.into_iter().enumerate() {
                    if let Some(old) = model.row_data(i) {
                        item.avatar = old.avatar;
                    }
                    model.set_row_data(i, item);
                }
            } else {
                // Keep the avatars we already have for rows that only moved.
                let old: HashMap<String, Image> = (0..model.row_count()).filter_map(|i| model.row_data(i)).map(|c| (c.id.to_string(), c.avatar)).collect();
                let items: Vec<ChannelItem> = items
                    .into_iter()
                    .map(|mut item| {
                        if let Some(avatar) = old.get(item.id.as_str()) {
                            item.avatar = avatar.clone();
                        }
                        item
                    })
                    .collect();
                ui.set_channels(ModelRc::new(VecModel::from(items)));
            }
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let row = job.row;
            on_ui(&ui, move |ui| {
                if ui.get_selected_guild().as_str() == HOME {
                    update_row(ui.get_channels(), row, |c| c.avatar = Image::from_rgba8(pixels));
                }
            });
        });
        if self.people_page > 0 {
            self.show_people();
        }
    }

    /// The friends page (for the current tab) or the message-requests page.
    fn show_people(&mut self) {
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        let rows: Vec<home::PersonRow> = if self.people_page == 1 {
            let relationships = self.ready["relationships"].as_array().cloned().unwrap_or_default();
            home::friend_rows(&relationships, &self.users, &self.presences, self.friends_tab, &look)
        } else {
            let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
            private
                .iter()
                .filter(|c| home::is_request(c))
                .map(|c| {
                    let dm = home::dm_row(c, &self.presences, &self.me_name, &look);
                    let spam = c["is_spam"].as_bool() == Some(true);
                    home::PersonRow {
                        id: dm.id,
                        name: dm.name,
                        subtitle: if spam { i18n::t("Возможно, спам") } else { i18n::t("Хочет написать вам") },
                        status: dm.status,
                        kind: 3,
                        avatar_url: dm.avatar_url.unwrap_or_default(),
                    }
                })
                .collect()
        };
        let jobs: Vec<ImageJob> = rows
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.avatar_url.is_empty())
            .map(|(row, p)| ImageJob { row, url: p.avatar_url.clone(), w: DM_AVATAR_PX, h: DM_AVATAR_PX, ..Default::default() })
            .collect();
        let (page, tab) = (self.people_page, self.friends_tab);
        on_ui(&self.ui, move |ui| {
            let items: Vec<PersonItem> = rows
                .iter()
                .map(|p| PersonItem {
                    id: p.id.clone().into(),
                    name: readable(&p.name).into(),
                    initial: p.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                    subtitle: p.subtitle.clone().into(),
                    avatar: Image::default(),
                    has_avatar: false,
                    status: if page == 1 && p.kind == 0 { p.status } else { 0 },
                    kind: p.kind,
                })
                .collect();
            ui.set_people(ModelRc::new(VecModel::from(items)));
            ui.set_people_page(page);
            ui.set_friends_tab(tab);
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let row = job.row;
            on_ui(&ui, move |ui| {
                update_row(ui.get_people(), row, |p| {
                    p.avatar = Image::from_rgba8(pixels);
                    p.has_avatar = true;
                });
            });
        });
    }

    /// Opens the DM with someone on the friends page (creating it if there's
    /// none yet: that sends nothing), or a message request's channel.
    async fn person_open(&mut self, id: String) -> Result<(), String> {
        let channel = if self.people_page == 2 {
            id
        } else {
            let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
            let existing = private.iter().find(|c| {
                c["type"].as_u64() == Some(1) && c["recipients"].as_array().is_some_and(|r| r.iter().any(|u| u["id"].as_str() == Some(id.as_str())))
            });
            match existing.and_then(|c| c["id"].as_str()) {
                Some(cid) => cid.to_string(),
                None => {
                    let created = self.api.post("/users/@me/channels", json!({ "recipient_id": id })).await?;
                    let cid = created["id"].as_str().ok_or_else(|| i18n::t("Discord не открыл переписку"))?.to_string();
                    self.on_home_event("CHANNEL_CREATE", created);
                    cid
                }
            }
        };
        self.people_page = 0;
        let shown = channel.clone();
        on_ui(&self.ui, move |ui| {
            ui.set_people_page(0);
            ui.set_selected_channel(shown.into());
        });
        self.select_channel(channel).await
    }

    /// Everything the search box can find: DMs, friends without a DM, servers
    /// and their text channels.
    fn search_entries(&self) -> Vec<search::Entry> {
        let mut entries = Vec::new();
        let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        let mut with_dm = std::collections::HashSet::new();
        for c in private.iter().filter(|c| !home::is_request(c)) {
            let row = home::dm_row(c, &self.presences, &self.me_name, &look);
            let recipients = c["recipients"].as_array().cloned().unwrap_or_default();
            let usernames: Vec<String> = recipients.iter().filter_map(|u| u["username"].as_str().map(String::from)).collect();
            if let Some(r) = &row.recipient {
                with_dm.insert(r.clone());
            }
            let weight = c["last_message_id"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
            let detail = if row.recipient.is_some() { usernames.first().cloned().unwrap_or_default() } else { row.subtitle.clone() };
            entries.push(search::Entry { target: search::Target::Dm(row.id), name: readable(&row.name), detail, aliases: usernames, weight });
        }
        for r in self.ready["relationships"].as_array().into_iter().flatten() {
            let Some(id) = r["id"].as_str() else { continue };
            if r["type"].as_u64() != Some(1) || with_dm.contains(id) {
                continue;
            }
            let user = if r["user"].is_object() { r["user"].clone() } else { self.users.get(id).cloned().unwrap_or_default() };
            let username = user["username"].as_str().unwrap_or("").to_string();
            let name = readable(&display_name(&user));
            entries.push(search::Entry { target: search::Target::Friend(id.into()), name, detail: username.clone(), aliases: vec![username], weight: 0 });
        }
        // People of the open server (from its member list) open their DM.
        let known: HashSet<String> = entries.iter().filter_map(|e| if let search::Target::Friend(id) = &e.target { Some(id.clone()) } else { None }).collect();
        if let Some(guild) = self.members_guild.as_ref().and_then(|g| self.guild_names.get(g)) {
            for item in self.members.items() {
                let members::Item::Member { id, user, nick, .. } = item else { continue };
                if id == self.me_id || known.contains(&id) || with_dm.contains(&id) {
                    continue;
                }
                let username = user["username"].as_str().unwrap_or("").to_string();
                let name = nick.map(|n| readable(&n)).unwrap_or_else(|| readable(&display_name(&user)));
                entries.push(search::Entry { target: search::Target::Friend(id), name, detail: format!("{username} · {guild}"), aliases: vec![username], weight: 0 });
            }
        }
        for g in self.ready["guilds"].as_array().into_iter().flatten() {
            let Some(gid) = g["id"].as_str() else { continue };
            let guild_name = self.guild_names.get(gid).cloned().unwrap_or_default();
            let here = self.guild.as_deref() == Some(gid);
            entries.push(search::Entry { target: search::Target::Guild(gid.into()), name: guild_name.clone(), detail: String::new(), aliases: Vec::new(), weight: 0 });
            for c in g["channels"].as_array().into_iter().flatten() {
                // Text channels only: voice ones are joined from the list.
                if !matches!(c["type"].as_u64(), Some(0 | 5 | 15)) {
                    continue;
                }
                let (Some(cid), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else { continue };
                let target = search::Target::Channel(gid.into(), cid.into());
                entries.push(search::Entry { target, name: readable(name), detail: guild_name.clone(), aliases: Vec::new(), weight: here as u64 });
            }
        }
        entries
    }

    fn search(&mut self, text: &str) {
        self.search_results = search::find_recent(&self.search_entries(), text, 10, &self.recent_picks);
        let items: Vec<(i32, String, String)> = self.search_results.iter().map(|e| (e.target.kind(), e.name.clone(), e.detail.clone())).collect();
        on_ui(&self.ui, move |ui| {
            let items: Vec<SearchItem> = items
                .into_iter()
                .map(|(kind, name, detail)| SearchItem {
                    kind,
                    initial: initials(&name).chars().take(2).collect::<String>().to_uppercase().into(),
                    name: name.into(),
                    detail: detail.into(),
                })
                .collect();
            ui.set_search_results(ModelRc::new(VecModel::from(items)));
        });
    }

    /// Opens what a search result points at, switching sides or servers as needed.
    async fn open_target(&mut self, target: search::Target) -> Result<(), String> {
        let (guild, channel) = match &target {
            search::Target::Dm(id) => (HOME.to_string(), Some(id.clone())),
            search::Target::Friend(_) => (HOME.to_string(), None),
            search::Target::Guild(gid) => (gid.clone(), None),
            search::Target::Channel(gid, cid) => (gid.clone(), Some(cid.clone())),
        };
        if self.guild.as_deref() != Some(guild.as_str()) {
            let shown = guild.clone();
            on_ui(&self.ui, move |ui| {
                ui.set_selected_guild(shown.into());
                ui.set_people_page(0);
            });
            self.select_guild(guild).await?;
        }
        if let search::Target::Friend(user) = target {
            self.people_page = 1;
            return self.person_open(user).await;
        }
        if let Some(channel) = channel {
            let shown = channel.clone();
            on_ui(&self.ui, move |ui| {
                ui.set_selected_channel(shown.into());
                ui.set_people_page(0);
                ui.set_show_call(false);
            });
            self.select_channel(channel).await?;
        }
        Ok(())
    }

    /// The line under our name in the user card: the game we are playing (as Discord shows it, "Rust +2"), else "Online".
    fn show_my_activity(&self, sessions: &Value) {
        let list = sessions.as_array().map(Vec::as_slice).unwrap_or_default();
        let mut seen = HashSet::new();
        let activities: Vec<Value> = list
            .iter()
            .flat_map(|s| s["activities"].as_array().into_iter().flatten())
            .filter(|a| a["type"].as_u64() != Some(4) && seen.insert(a["name"].as_str().unwrap_or_default().to_string()))
            .cloned()
            .collect();
        let line = activity_line(&activities);
        on_ui(&self.ui, move |ui| ui.set_status(line.into()));
    }

    /// DM-side gateway events: they change the DM list, friends or presences.
    fn on_home_event(&mut self, kind: &str, data: Value) {
        let list = |ready: &Value, key: &str| -> Vec<Value> { ready[key].as_array().cloned().unwrap_or_default() };
        match kind {
            "READY_SUPPLEMENTAL" => {
                self.presences.extend(home::presences_in(&data));
                self.note_spotify(home::spotify_in(&data));
                self.home_dirty = true;
            }
            // Our own sessions (this one, the phone, the official app) and what they are playing.
            "SESSIONS_REPLACE" => self.show_my_activity(&data),
            // Read elsewhere (the phone, the official app): the pill goes away here too.
            // A call began, changed or ended in a DM or group: who is in it.
            "CALL_CREATE" | "CALL_UPDATE" => {
                if let Some(cid) = data["channel_id"].as_str() {
                    for vs in data["voice_states"].as_array().into_iter().flatten() {
                        if let Some(uid) = vs["user_id"].as_str() {
                            self.note_dm_call_state(uid, Some(cid), vs);
                        }
                    }
                    self.show_dm_call();
                }
            }
            "CALL_DELETE" => {
                if let Some(cid) = data["channel_id"].as_str() {
                    self.dm_call_of.retain(|_, (c, _, _)| c != cid);
                    self.dm_call_declined.remove(cid);
                    self.show_dm_call();
                }
            }
            // Notification choices changed (here, on the phone, in the official app).
            "USER_GUILD_SETTINGS_UPDATE" => {
                self.unread.settings.apply(&data, chrono::Utc::now());
                self.refresh_badges();
            }
            "MESSAGE_ACK" => {
                let (Some(cid), Some(mid)) = (data["channel_id"].as_str(), data["message_id"].as_str()) else { return };
                self.unread.mark_read(cid, mid.parse().unwrap_or(0));
                self.refresh_badges();
            }
            "PRESENCE_UPDATE" => {
                self.note_spotify(vec![home::spotify_of(&data)]);
                if let Some((id, presence)) = home::presence_of(&data) {
                    if self.presences.get(&id) != Some(&presence) {
                        self.presences.insert(id, presence);
                        self.home_dirty = true;
                    }
                }
            }
            // Our roles or the server's roles changed: its kept channels and permissions are asked again.
            "GUILD_MEMBER_UPDATE" | "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE" => {
                if data["role"].is_object() {
                    names::note_role(&data["role"]);
                }
                let ours = kind != "GUILD_MEMBER_UPDATE" || data["user"]["id"].as_str() == Some(self.me_id.as_str());
                if let (true, Some(g)) = (ours, data["guild_id"].as_str()) {
                    self.guild_cache.remove(g);
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
                if let Some(g) = data["guild_id"].as_str() {
                    self.guild_cache.remove(g);
                    names::note_channels(std::slice::from_ref(&data));
                }
                // Only DMs and group DMs live here; guild channels have a guild id.
                if !data["guild_id"].is_null() || !matches!(data["type"].as_u64(), Some(1 | 3)) {
                    return;
                }
                let id = data["id"].as_str().unwrap_or("").to_string();
                let mut private = list(&self.ready, "private_channels");
                private.retain(|c| c["id"].as_str() != Some(id.as_str()));
                if kind != "CHANNEL_DELETE" {
                    private.push(data);
                }
                self.ready["private_channels"] = Value::Array(private);
                self.refresh_home();
            }
            "CHANNEL_RECIPIENT_ADD" | "CHANNEL_RECIPIENT_REMOVE" => {
                if data["user"].is_object() {
                    if let Some(id) = data["user"]["id"].as_str() {
                        self.users.insert(id.to_string(), data["user"].clone());
                    }
                }
                let channel = data["channel_id"].as_str();
                if let Some(group) = self.ready["private_channels"].as_array_mut().and_then(|l| l.iter_mut().find(|c| c["id"].as_str() == channel)) {
                    home::change_recipient(group, &data["user"], kind == "CHANNEL_RECIPIENT_ADD");
                }
                self.refresh_home();
            }
            "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" | "RELATIONSHIP_REMOVE" => {
                let id = data["id"].as_str().or(data["user"]["id"].as_str()).unwrap_or("").to_string();
                if data["user"].is_object() {
                    note_user(&data["user"]);
                    self.users.insert(id.clone(), data["user"].clone());
                }
                let mut relationships = list(&self.ready, "relationships");
                let old = relationships.iter().position(|r| r["id"].as_str() == Some(id.as_str()));
                match (kind, old) {
                    ("RELATIONSHIP_REMOVE", Some(i)) => {
                        relationships.remove(i);
                    }
                    ("RELATIONSHIP_REMOVE", None) => {}
                    (_, Some(i)) => {
                        // An update may carry only the changed fields.
                        if let (Some(old), Some(new)) = (relationships[i].as_object_mut(), data.as_object()) {
                            for (k, v) in new {
                                old.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    (_, None) => relationships.push(data),
                }
                self.ready["relationships"] = Value::Array(relationships);
                if self.people_page == 1 {
                    self.show_people();
                }
            }
            _ => {}
        }
    }

    async fn select_channel(&mut self, id: String) -> Result<(), String> {
        // The two pages on top of the DM list.
        if id == "friends" || id == "requests" {
            self.people_page = if id == "friends" { 1 } else { 2 };
            self.show_people();
            return Ok(());
        }
        self.people_page = 0;
        // Pasted files belong to the chat they were pasted in.
        if self.channel.as_deref() != Some(id.as_str()) && !self.pending_files.is_empty() {
            self.pending_files.clear();
            self.show_pending();
        }
        // A voice message is recorded for the chat it was started in: leaving it throws the recording away.
        if self.recording.as_ref().is_some_and(|(channel, _)| *channel != id) {
            self.record_stop();
        }
        if let Some(guild) = self.guild.clone() {
            self.trail.visit((guild, id.clone()));
            self.show_trail();
        }
        let seen = chat_clicks();
        let opened = self.open_messages(id.clone(), None).await;
        tokio::select! {
            _ = self.show_dm_card(&id) => {}
            _ = clicked_since(seen) => {}
        }
        opened
    }

    fn show_trail(&self) {
        let (back, ahead) = (self.trail.can_back(), self.trail.can_ahead());
        on_ui(&self.ui, move |ui| {
            ui.set_can_go_back(back);
            ui.set_can_go_ahead(ahead);
        });
    }

    /// The arrows: to a chat seen before, in its server, without adding to the trail.
    async fn walk(&mut self, to: Option<fold::Place>) -> Result<(), String> {
        self.show_trail();
        let Some((guild, channel)) = to else { return Ok(()) };
        if self.guild.as_deref() != Some(guild.as_str()) {
            self.select_guild(guild).await?;
        }
        self.people_page = 0;
        let shown = channel.clone();
        on_ui(&self.ui, move |ui| {
            ui.set_show_call(false);
            ui.set_forum_open(false);
            ui.set_people_page(0);
            ui.set_selected_channel(shown.into());
        });
        let opened = self.open_messages(channel.clone(), None).await;
        self.show_dm_card(&channel).await;
        opened
    }

    /// The panel beside a DM shows the other person; other chats have none.
    async fn show_dm_card(&mut self, channel: &str) {
        let private = self.ready["private_channels"].as_array();
        let dm = private.and_then(|list| list.iter().find(|c| c["id"].as_str() == Some(channel) && c["type"].as_u64() == Some(1)));
        let other = dm.and_then(|c| c["recipients"][0]["id"].as_str().or(c["recipient_ids"][0].as_str())).map(String::from);
        match other {
            Some(id) => {
                if let Err(e) = self.user_card(id, false).await {
                    log_line(&format!("dm card: {e}"));
                }
            }
            None => on_ui(&self.ui, |ui| ui.set_dm_card(UserCardItem::default())),
        }
    }

    /// A click on a server or on the DM button: the chat left open there; the first we can see in a server new to us,
    /// the friends page on the DM side when there is none.
    async fn open_left_chat(&mut self, guild: String) -> Result<(), String> {
        // Another place has been clicked since.
        if *LAST_GUILD_CLICK.lock().unwrap() != guild {
            return Ok(());
        }
        let left = pages::last_in(&guild);
        let target = if guild == HOME {
            let open = self.dm_ids();
            left.filter(|c| open.contains(c))
        } else {
            let list = self.guild_cache.get(&guild).map(|(_, list, _)| list.clone());
            let mine = self.guild_perms(&guild).await;
            let channels = list.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
            let usable = |id: &str| channels.iter().any(|c| c["id"].as_str() == Some(id) && matches!(c["type"].as_u64(), Some(0 | 5 | 15)) && mine.as_ref().is_none_or(|m| m.can_view(c)));
            left.filter(|c| usable(c) || self.thread_ids.contains(c)).or_else(|| mine.as_ref().and_then(|m| usermenu::invite_channel(&channels, m)))
        };
        let Some(channel) = target else {
            if guild == HOME {
                on_ui(&self.ui, |ui| {
                    ui.set_show_call(false);
                    ui.set_selected_channel("friends".into());
                    ui.set_people_page(1);
                });
                return self.select_channel("friends".into()).await;
            }
            return Ok(());
        };
        let shown = channel.clone();
        on_ui(&self.ui, move |ui| {
            ui.set_show_call(false);
            ui.set_forum_open(false);
            ui.set_people_page(0);
            ui.set_selected_channel(shown.into());
        });
        self.select_channel(channel).await
    }

    /// A channel's messages: the newest, or those around one message (a search hit).
    async fn open_messages(&mut self, id: String, around: Option<String>) -> Result<(), String> {
        self.channel = Some(id.clone());
        let plain_channel = self.channels.get(&id).is_some_and(|c| !c.dm && !c.forum) && !self.thread_ids.contains(&id);
        if plain_channel {
            self.watch_members(&id, true).await;
        }
        if self.channels.get(&id).is_some_and(|c| c.forum) {
            self.watch_members(&id, false).await;
            return self.open_forum(id).await;
        }
        if let Some(guild) = &self.guild {
            pages::note_recent(guild, &id);
            pages::note_last(guild, &id);
        }
        let opening = Instant::now();
        // The page below only replaces one already on screen, when it differs at all.
        let mut refresh = false;
        let list: Vec<Value> = match &around {
            // Around a message (a search hit): the ones before it and the ones
            // right around it, so the found message ends up on screen.
            Some(m) => {
                let hit = snowflake(m)?;
                let mut all: Vec<Value> = Vec::new();
                for query in [format!("before={hit}&limit=24"), format!("around={hit}&limit=3")] {
                    if let Ok(v) = self.api.get(&format!("/channels/{}/messages?{query}", snowflake(&id)?)).await {
                        all.extend(v.as_array().cloned().unwrap_or_default());
                    }
                }
                all.sort_by_key(|m| m["id"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0));
                all.dedup_by(|a, b| a["id"] == b["id"]);
                all
            }
            None => {
                let path = format!("/channels/{}/messages?limit={PAGE}", snowflake(&id)?);
                let seen = chat_clicks();
                // A page seen before goes on screen at once; the fresh one follows.
                let kept = self.pages.lock().unwrap().get(&id).cloned().or_else(|| load_cached_messages(&id));
                if let Some(kept) = &kept {
                    self.show_page(id.clone(), oldest_first(kept), None, false).await?;
                    log_line(&format!("chat open: kept page on screen after {} ms", opening.elapsed().as_millis()));
                    refresh = true;
                }
                let list = tokio::select! {
                    list = self.api.get(&path) => list,
                    _ = clicked_since(seen) => return Ok(()),
                };
                log_line(&format!("chat open: Discord answered after {} ms", opening.elapsed().as_millis()));
                if self.channel.as_deref() != Some(id.as_str()) {
                    return Ok(()); // user clicked elsewhere meanwhile
                }
                match list {
                    Ok(list) => {
                        save_cached_messages(&id, &list);
                        self.pages.lock().unwrap().put(&id, list.clone());
                        if kept.as_ref() == Some(&list) {
                            return Ok(()); // what is on screen is up to date
                        }
                        oldest_first(&list)
                    }
                    // Without a connection the kept page stays, and that is said.
                    // Not when Discord refuses the token: then nothing of the account is shown.
                    Err(e) if kept.is_some() && !is_unauthorized(&e) => {
                        log_line(&format!("chat open: {e}"));
                        notice(&self.ui, i18n::t("Нет связи: показаны сохранённые сообщения, они могли устареть"));
                        return Ok(());
                    }
                    Err(e) => {
                        // Never leave the previous channel's messages under this one's name.
                        let name = self.channels.get(&id).map(|c| c.name.clone()).unwrap_or_default();
                        let text = if e.contains(" 403 ") || e.contains("50001") || e.contains("50013") {
                            i18n::t("🔒 Нет доступа к этому каналу")
                        } else {
                            i18n::tf("Не удалось загрузить сообщения: {}", &[&e])
                        };
                        self.message_rows = 0;
                        self.last = None;
                        self.reactions.clear();
                        self.watch_members(&id, false).await;
                        on_ui(&self.ui, move |ui| {
                            ui.set_selected_channel(id.into());
                            ui.set_channel_title(name.clone().into());
                            ui.set_channel_topic("".into());
                            ui.set_placeholder(i18n::tf("Написать в #{}", &[&name]).into());
                            ui.set_window_title(format!("#{name} — Nimbus").into());
                            ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
                            ui.set_notice(text.into());
                        });
                        return Ok(());
                    }
                }
            }
        };
        self.show_page(id, list, around, refresh).await
    }

    /// Puts a page of messages on screen. `refresh`: the chat is open already and this page
    /// replaces the kept one, so the chat doesn't start over and pictures in place stay.
    async fn show_page(&mut self, id: String, list: Vec<Value>, around: Option<String>, refresh: bool) -> Result<(), String> {
        if self.channel.as_deref() != Some(id.as_str()) {
            return Ok(()); // user clicked elsewhere meanwhile
        }
        let dm = self.channels.get(&id).is_some_and(|c| c.dm);
        // A thread has no member list of its own: the one of its channel stays.
        if !refresh && !self.thread_ids.contains(&id) {
            self.watch_members(&id, !dm).await;
            if dm {
                self.show_group_members(&id);
            }
        }
        self.raw.clear();
        for m in &list {
            self.note_raw(m);
        }

        let mut prev = None;
        let mut jobs = Vec::new();
        let mut msgs: Vec<Msg> = list.iter().map(|m| to_msg(m, &mut prev, &mut jobs)).collect();
        self.paint(&mut msgs);
        // The message a search found: it's marked, so you see where you landed.
        if let Some(hit) = &around {
            for m in msgs.iter_mut() {
                if &m.id == hit {
                    m.highlight = true;
                }
            }
        }
        self.last = prev;
        self.message_rows = msgs.len();
        self.history = History {
            oldest: list.first().and_then(|m| m["id"].as_str()).map(String::from),
            done: around.is_none() && list.len() < PAGE,
            opened: Instant::now(),
            busy: false,
            detached: around.is_some(),
            newest: list.last().and_then(|m| m["id"].as_str()).map(String::from),
        };
        self.reactions = msgs.iter().filter(|m| !m.reactions.is_empty()).map(|m| (m.id.clone(), m.reactions.clone())).collect();
        if !refresh {
            self.stop_clip();
        }
        self.polls.clear();
        self.clips.clear();
        self.components.clear();
        self.select_chosen.clear();
        self.interactions.clear();
        self.note_extras(&msgs);
        self.load_emojis(emoji_urls(&msgs));
        self.show_typing();

        let (name, topic, dm) = self
            .channels
            .get(&id)
            .map(|c| (c.name.clone(), c.topic.clone(), c.dm))
            .unwrap_or_default();
        let guild = self.guild.as_ref().and_then(|g| self.guild_names.get(g)).cloned();
        let thread = self.thread_ids.contains(&id);
        let title = match (&guild, dm) {
            (Some(g), false) => format!("#{name} · {g} — Nimbus"),
            _ => format!("{name} — Nimbus"),
        };
        let shown = id.clone();
        let detached = around.is_some();
        let hit_row = around.as_ref().and_then(|hit| msgs.iter().position(|m| &m.id == hit));
        on_ui(&self.ui, move |ui| {
            let building = Instant::now();
            let mut items: Vec<MessageItem> = msgs.iter().map(Msg::item).collect();
            if refresh {
                keep_pictures(&ui.get_messages(), &mut items);
            }
            ANIMATIONS.with(|a| a.borrow_mut().clear());
            ui.set_selected_channel(id.into());
            ui.set_channel_title(name.clone().into());
            ui.set_channel_topic(topic.into());
            ui.set_channel_is_dm(dm);
            ui.set_channel_is_thread(thread);
            ui.set_placeholder(if dm { i18n::tf("Написать {}", &[&name]) } else { i18n::tf("Написать в #{}", &[&name]) }.into());
            ui.set_window_title(title.into());
            ui.set_messages(ModelRc::new(VecModel::from(items)));
            if !refresh {
                ui.invoke_chat_appear();
            }
            log_line(&format!("chat open: {} rows set in {} ms", ui.get_messages().row_count(), building.elapsed().as_millis()));
            ui.set_notice("".into());
            ui.set_forum_open(false);
            ui.set_dm_call("".into());
            ui.set_detached(detached);
            match hit_row {
                // A search hit: the list shows it, and its mark fades after a few seconds.
                Some(row) => scroll_to_row_soon(&ui, row),
                None => scroll_soon(&ui),
            }
        });
        if let Some(hit) = around.clone() {
            let ui = self.ui.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(4)).await;
                on_ui(&ui, move |ui| {
                    if let Some(row) = message_row(&ui, &hit) {
                        update_row(ui.get_messages(), row, |m| m.highlight = false);
                    }
                });
            });
        }
        let newest = list.last().and_then(|m| m["id"].as_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
        self.load_message_images(shown.clone(), jobs);
        self.show_dm_call();
        if around.is_none() {
            self.read_channel(&shown, newest).await;
        }
        Ok(())
    }

    /// Scrolled up to the top: the page of messages before the oldest shown,
    /// added above without moving what's on screen.
    async fn load_older(&mut self) -> Result<(), String> {
        self.apply_trim();
        let h = &self.history;
        // Right after opening the list sits at the top for a moment, before it scrolls down.
        if h.busy || h.done || h.opened.elapsed() < Duration::from_millis(1500) {
            return Ok(());
        }
        let (Some(channel), Some(oldest)) = (self.channel.clone(), h.oldest.clone()) else { return Ok(()) };
        self.history.busy = true;
        let list = self.api.get(&format!("/channels/{}/messages?limit={PAGE}&before={}", snowflake(&channel)?, snowflake(&oldest)?)).await;
        self.history.busy = false;
        let mut list = list?.as_array().cloned().unwrap_or_default();
        if self.channel.as_deref() != Some(channel.as_str()) {
            return Ok(());
        }
        list.reverse();
        self.history.done = list.len() < PAGE;
        if let Some(first) = list.first().and_then(|m| m["id"].as_str()) {
            self.history.oldest = Some(first.to_string());
        }
        for m in &list {
            self.note_raw(m);
        }
        let (mut prev, mut jobs) = (None, Vec::new());
        let mut msgs: Vec<Msg> = list.iter().map(|m| to_msg(m, &mut prev, &mut jobs)).collect();
        self.paint(&mut msgs);
        self.message_rows += msgs.len();
        for m in msgs.iter().filter(|m| !m.reactions.is_empty()) {
            self.reactions.insert(m.id.clone(), m.reactions.clone());
        }
        self.note_extras(&msgs);
        self.load_emojis(emoji_urls(&msgs));
        let shown = channel.clone();
        on_ui(&self.ui, move |ui| {
            if ui.get_selected_channel().as_str() != shown {
                return;
            }
            let model = ui.get_messages();
            let Some(rows) = model.as_any().downcast_ref::<VecModel<MessageItem>>() else { return };
            ui.invoke_remember_scroll();
            for (i, m) in msgs.iter().enumerate() {
                rows.insert(i, m.item());
            }
            // A long way up: the newest rows leave (their pictures with them); scrolling back down loads them again.
            let excess = rows.row_count().saturating_sub(MAX_ROWS);
            if excess > 0 {
                for _ in 0..excess {
                    rows.remove(rows.row_count() - 1);
                }
                if let Some(last) = rows.row_data(rows.row_count() - 1) {
                    TRIM.lock().unwrap().0 = Some(last.id.to_string());
                }
                ui.set_detached(true);
            }
            // Twice: the list measures new rows lazily, so its height settles a moment later.
            for ms in [30, 150] {
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(ms), move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.invoke_restore_scroll();
                    }
                });
            }
        });
        self.load_message_images(channel, jobs);
        Ok(())
    }

    /// Scrolled to the bottom of a list opened around an old message: the
    /// page after the newest shown, added below. At the end, new messages flow in again.
    async fn load_newer(&mut self) -> Result<(), String> {
        self.apply_trim();
        let h = &self.history;
        if !h.detached || h.busy || h.opened.elapsed() < Duration::from_millis(1500) {
            return Ok(());
        }
        let (Some(channel), Some(newest)) = (self.channel.clone(), h.newest.clone()) else { return Ok(()) };
        self.history.busy = true;
        let list = self.api.get(&format!("/channels/{}/messages?limit={PAGE}&after={}", snowflake(&channel)?, snowflake(&newest)?)).await;
        self.history.busy = false;
        let mut list = list?.as_array().cloned().unwrap_or_default();
        if self.channel.as_deref() != Some(channel.as_str()) {
            return Ok(());
        }
        // Oldest first, whichever order the API answers in.
        list.sort_by_key(|m| m["id"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0));
        let reached_end = list.len() < PAGE;
        if let Some(last) = list.last().and_then(|m| m["id"].as_str()) {
            self.history.newest = Some(last.to_string());
        }
        if reached_end {
            self.history.detached = false;
        }
        for m in &list {
            self.note_raw(m);
        }
        let mut jobs = Vec::new();
        let mut msgs: Vec<Msg> = list.iter().map(|m| to_msg(m, &mut self.last, &mut jobs)).collect();
        self.paint(&mut msgs);
        self.message_rows += msgs.len();
        for m in msgs.iter().filter(|m| !m.reactions.is_empty()) {
            self.reactions.insert(m.id.clone(), m.reactions.clone());
        }
        self.note_extras(&msgs);
        self.load_emojis(emoji_urls(&msgs));
        let shown = channel.clone();
        on_ui(&self.ui, move |ui| {
            if ui.get_selected_channel().as_str() != shown {
                return;
            }
            let model = ui.get_messages();
            if let Some(rows) = model.as_any().downcast_ref::<VecModel<MessageItem>>() {
                ui.invoke_remember_scroll();
                for m in &msgs {
                    rows.push(m.item());
                }
                // A long way down: the oldest rows leave; scrolling back up loads them again.
                let excess = rows.row_count().saturating_sub(MAX_ROWS);
                if excess > 0 {
                    for _ in 0..excess {
                        rows.remove(0);
                    }
                    if let Some(first) = rows.row_data(0) {
                        TRIM.lock().unwrap().1 = Some(first.id.to_string());
                    }
                    for ms in [30, 150] {
                        let weak = ui.as_weak();
                        slint::Timer::single_shot(Duration::from_millis(ms), move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_restore_scroll();
                            }
                        });
                    }
                }
            }
            if reached_end {
                ui.set_detached(false);
            }
        });
        self.load_message_images(channel, jobs);
        Ok(())
    }

    /// "Back to the latest": the channel again, from its newest messages.
    async fn return_latest(&mut self) -> Result<(), String> {
        match self.channel.clone() {
            Some(channel) => self.open_messages(channel, None).await,
            None => Ok(()),
        }
    }

    /// Shows the member list of the open server channel (asks Discord for it),
    /// or hides it where there is none (DMs, forums, no access).
    async fn watch_members(&mut self, channel: &str, show: bool) {
        let guild = self.guild.clone().filter(|g| g != HOME && show);
        // Already asked for, as the chat began to open.
        if show && guild.is_some() && self.members_channel.as_ref().is_some_and(|(g, c)| Some(g) == guild.as_ref() && c == channel) {
            return;
        }
        // The list we leave is kept, and Discord told to stop: it tells again when we come back.
        if let Some((old_guild, old_channel)) = self.members_channel.take() {
            let _ = self.gateway.send(members::unsubscribe(&old_guild, &old_channel));
            if self.members_kept.len() >= MEMBER_LISTS_KEPT {
                self.members_kept.clear();
            }
            self.members_kept.insert(old_channel, std::mem::take(&mut self.members));
        }
        self.members_asked = None;
        let Some(guild) = guild else {
            self.members.clear();
            self.members_guild = None;
            self.group_shown = None;
            on_ui(&self.ui, |ui| {
                ui.set_members_open(false);
                ui.set_members(ModelRc::new(VecModel::from(Vec::<MemberItem>::new())));
            });
            return;
        };
        if !self.role_tables.contains_key(&guild) {
            let roles = match self.ready["guilds"].as_array().and_then(|g| g.iter().find(|g| g["id"].as_str() == Some(guild.as_str()))) {
                Some(g) if g["roles"].as_array().is_some_and(|r| !r.is_empty()) => g["roles"].as_array().cloned().unwrap_or_default(),
                _ => match snowflake(&guild) {
                    Ok(id) => self.api.get(&format!("/guilds/{id}/roles")).await.ok().and_then(|r| r.as_array().cloned()).unwrap_or_default(),
                    Err(_) => Vec::new(),
                },
            };
            names::note_roles(&roles);
            self.role_tables.insert(guild.clone(), members::role_table(&roles));
        }
        self.members = self.members_kept.remove(channel).unwrap_or_default();
        self.member_chunks = members::FIRST_CHUNKS;
        if self.members_guild.as_deref() != Some(guild.as_str()) {
            self.role_of.clear();
        }
        self.members_guild = Some(guild.clone());
        self.members_channel = Some((guild.clone(), channel.to_string()));
        self.members_asked = Some((Instant::now(), 0));
        on_ui(&self.ui, |ui| ui.set_members_open(true));
        // What we had of this list goes up at once; the panel doesn't stand empty meanwhile.
        if self.members.progress().0 > 0 {
            self.show_members();
        } else {
            on_ui(&self.ui, |ui| ui.set_members(ModelRc::new(VecModel::from(Vec::<MemberItem>::new()))));
        }
        let _ = self.gateway.send(members::subscribe(&guild, channel, members::FIRST_CHUNKS));
    }

    /// Nothing came for the list: asked again from scratch a few times, so a lost answer doesn't leave it empty.
    fn ask_members_again(&mut self) {
        let Some((at, tries)) = self.members_asked else { return };
        if self.members.progress().0 > 0 || tries >= MEMBER_ASKS_AGAIN {
            self.members_asked = None;
            return;
        }
        if at.elapsed() < MEMBER_ASK_AFTER {
            return;
        }
        let Some((guild, channel)) = self.members_channel.clone() else { return };
        log_line(&format!("member list: nothing came, asking again ({})", tries + 1));
        let _ = self.gateway.send(members::unsubscribe(&guild, &channel));
        let _ = self.gateway.send(members::subscribe(&guild, &channel, members::FIRST_CHUNKS));
        self.members_asked = Some((Instant::now(), tries + 1));
    }

    /// A group's people beside it, as a server's are, those online first. Drawn again only when something in it changed.
    fn show_group_members(&mut self, channel: &str) {
        let private = self.ready["private_channels"].as_array();
        let Some(group) = private.and_then(|l| l.iter().find(|c| c["id"].as_str() == Some(channel) && c["type"].as_u64() == Some(3))).cloned() else { return };
        let me = self.users.get(&self.me_id).cloned().unwrap_or_else(|| self.ready["user"].clone());
        let owner = group["owner_id"].as_str().unwrap_or("");
        let mut people: Vec<MemberRow> = home::group_people(&group, &me, &self.users)
            .into_iter()
            .map(|(id, user)| {
                let presence = self.presences.get(&id);
                let status = if id == self.me_id { 1 } else { presence.map_or(home::OFFLINE, |p| p.status) };
                let activity = presence.map_or(String::new(), |p| readable(&p.activity));
                let subtitle = if activity.is_empty() && id == owner { i18n::t("Владелец") } else { activity };
                (1, id, display_name(&user), subtitle, status, name_style_color(&user), avatar_url(&user), decoration_url(&user), nameplate_url(&user), nameplate_palette(&user))
            })
            .collect();
        people.sort_by_key(|r| (r.4 == home::OFFLINE, r.2.to_lowercase()));
        let heading = (0, "group".to_string(), format!("{} — {}", i18n::t("Участники"), people.len()), String::new(), 0, None, String::new(), None, None, String::new());
        let rows: Vec<MemberRow> = std::iter::once(heading).chain(people).collect();
        let shown = (channel.to_string(), rows.clone());
        if self.group_shown.as_ref() == Some(&shown) {
            return;
        }
        self.group_shown = Some(shown);
        on_ui(&self.ui, |ui| ui.set_members_open(true));
        self.put_member_rows(rows, HashMap::new());
    }

    /// Asks Discord for the next chunks of a big server's list.
    fn more_members(&mut self) {
        let (Some(guild), Some(channel)) = (self.members_guild.clone(), self.channel.clone()) else { return };
        if !self.members.wants_more(self.member_chunks) {
            return;
        }
        self.member_chunks = (self.member_chunks + 2).min(members::MAX_CHUNKS);
        let _ = self.gateway.send(members::subscribe(&guild, &channel, self.member_chunks));
    }

    /// Redraws the member list from what Discord has sent so far.
    fn show_members(&mut self) {
        let Some(guild) = self.members_guild.clone() else { return };
        let table = self.role_tables.get(&guild).cloned().unwrap_or_default();
        // Their roles also colour the names in the chat.
        let mut colors: HashMap<String, u32> = HashMap::new();
        for item in self.members.items() {
            if let members::Item::Member { id, roles, .. } = item {
                if let Some(c) = members::name_color(&roles, &table) {
                    colors.insert(id.clone(), c);
                }
                self.role_of.insert(id, roles);
            }
        }
        let rows: Vec<MemberRow> = self
            .members
            .items()
            .into_iter()
            .map(|item| match item {
                members::Item::Group { id, count } => (0, id.clone(), format!("{} — {count}", members::group_name(&id, &table)), String::new(), 0, None, String::new(), None, None, String::new()),
                members::Item::Member { id, user, nick, roles, status, activity } => {
                    let name = nick.map(|n| readable(&n)).unwrap_or_else(|| display_name(&user));
                    // A name style the person chose wins over the colour of their role.
                    let color = name_style_color(&user).or_else(|| members::name_color(&roles, &table));
                    (1, id, name, readable(&activity), status as i32, color, avatar_url(&user), decoration_url(&user), nameplate_url(&user), nameplate_palette(&user))
                }
            })
            .collect();
        self.put_member_rows(rows, colors);
    }

    /// The right panel with these rows; `colors`: names already in the chat take these.
    fn put_member_rows(&self, rows: Vec<MemberRow>, colors: HashMap<String, u32>) {
        let jobs: Vec<ImageJob> = rows
            .iter()
            .filter(|r| r.0 == 1)
            .flat_map(|r| {
                let avatar = ImageJob { message: r.1.clone(), url: r.6.clone(), w: MEMBER_AVATAR_PX, h: MEMBER_AVATAR_PX, ..Default::default() };
                let frame = r.7.clone().map(|url| ImageJob { message: r.1.clone(), url, w: DECO_PX, h: DECO_PX, deco: true, ..Default::default() });
                let plate = r.8.clone().map(|url| ImageJob { message: r.1.clone(), url, w: PLATE_W, h: PLATE_H, plate: true, palette: r.9.clone(), ..Default::default() });
                std::iter::once(avatar).chain(frame).chain(plate)
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            let items: Vec<MemberItem> = rows
                .into_iter()
                .map(|(kind, id, name, subtitle, status, color, _, _, _, _)| MemberItem {
                    kind,
                    id: id.into(),
                    name: name.into(),
                    subtitle: subtitle.into(),
                    status,
                    has_color: color.is_some(),
                    color: color
                        .map(|c| slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8))
                        .unwrap_or_default(),
                    avatar: Image::default(),
                    has_avatar: false,
                    decoration: Image::default(),
                    has_decoration: false,
                    plate: Image::default(),
                    has_plate: false,
                })
                .collect();
            ui.set_members(ModelRc::new(VecModel::from(items)));
            // Names already on screen take their colours.
            let model = ui.get_messages();
            for row in 0..model.row_count() {
                let Some(m) = model.row_data(row) else { continue };
                if let Some(&c) = colors.get(m.author_id.as_str()) {
                    if !m.has_author_color {
                        update_row(model.clone(), row, |m| {
                            m.has_author_color = true;
                            m.author_color = rgb_color(c);
                        });
                    }
                }
            }
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let (id, deco, plate) = (job.message.clone(), job.deco, job.plate);
            // A nameplate sits on its palette's colour.
            let pixels = if plate { under_palette(pixels, &job.palette) } else { pixels };
            on_ui(&ui, move |ui| {
                let model = ui.get_members();
                for row in 0..model.row_count() {
                    if model.row_data(row).is_some_and(|m| m.kind == 1 && m.id.as_str() == id) {
                        update_row(model.clone(), row, |m| {
                            if deco {
                                m.decoration = Image::from_rgba8(pixels.clone());
                                m.has_decoration = true;
                            } else if plate {
                                m.plate = Image::from_rgba8(pixels.clone());
                                m.has_plate = true;
                            } else {
                                m.avatar = Image::from_rgba8(pixels.clone());
                                m.has_avatar = true;
                            }
                        });
                    }
                }
            });
        });
    }

    /// Keeps the last things opened from the search box, for the empty box.
    fn remember_pick(&mut self, target: &search::Target) {
        let key = target.key();
        self.recent_picks.retain(|k| *k != key);
        self.recent_picks.insert(0, key);
        self.recent_picks.truncate(8);
        prefs::write("recent_search", json!(self.recent_picks));
    }

    /// Searches messages: the whole server, or the DM we're in.
    async fn find_messages(&mut self, text: String) -> Result<(), String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            on_ui(&self.ui, |ui| ui.set_found(ModelRc::new(VecModel::from(Vec::<FoundItem>::new()))));
            return Ok(());
        }
        let scope = match (&self.guild, &self.channel) {
            (Some(g), _) if g != HOME => format!("/guilds/{}", snowflake(g)?),
            (_, Some(c)) => format!("/channels/{}", snowflake(c)?),
            _ => return Ok(()),
        };
        let path = format!("{scope}/messages/search?content={}", percent_encode(&text));
        let mut answer = Value::Null;
        // Discord indexes on first use and says to come back in a moment.
        for _ in 0..4 {
            answer = self.api.get(&path).await?;
            match answer["retry_after"].as_f64() {
                Some(wait) if answer["messages"].is_null() => tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.3, 3.0))).await,
                _ => break,
            }
        }
        let total = answer["total_results"].as_u64().unwrap_or(0);
        let hits: Vec<(String, String, String, String, String)> = answer["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| {
                let group = group.as_array()?;
                let m = group.iter().find(|m| m["hit"].as_bool() == Some(true)).or(group.first())?;
                let cid = m["channel_id"].as_str()?.to_string();
                let channel = self.channels.get(&cid).map(|c| format!("#{}", c.name)).unwrap_or_default();
                let body: String = names::unmark(&message_text(m, false)).chars().take(160).collect();
                let time = m["timestamp"].as_str().and_then(parse_time).map(|t| stamp(&t)).unwrap_or_default();
                Some((cid, m["id"].as_str()?.to_string(), display_name(&m["author"]), format!("{channel} · {time}"), body))
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            let items: Vec<FoundItem> = hits
                .into_iter()
                .map(|(channel, message, author, detail, text)| FoundItem {
                    channel: channel.into(),
                    message: message.into(),
                    author: author.into(),
                    detail: detail.into(),
                    text: text.into(),
                })
                .collect();
            ui.set_found_total(total as i32);
            ui.set_found_title("".into());
            ui.set_found(ModelRc::new(VecModel::from(items)));
        });
        Ok(())
    }

    /// The pinned messages of the open channel, in the search results panel.
    /// Asked only when the inbox button is clicked.
    async fn show_mentions(&mut self) -> Result<(), String> {
        let answer = self.api.get("/users/@me/mentions?limit=25&roles=true&everyone=true").await?;
        let rows: Vec<[String; 5]> = answer
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let time = m["timestamp"].as_str().and_then(parse_time).map(|t| stamp(&t)).unwrap_or_default();
                let body: String = names::unmark(&message_text(m, false)).chars().take(160).collect();
                Some([m["channel_id"].as_str()?.to_string(), m["id"].as_str()?.to_string(), display_name(&m["author"]), time, body])
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            ui.set_found_total(rows.len() as i32);
            let items: Vec<FoundItem> = rows
                .into_iter()
                .map(|[channel, message, author, detail, text]| FoundItem { channel: channel.into(), message: message.into(), author: author.into(), detail: detail.into(), text: text.into() })
                .collect();
            ui.set_found(ModelRc::new(VecModel::from(items)));
            ui.set_found_title(i18n::t("Упоминания").into());
            ui.set_found_open(true);
        });
        Ok(())
    }

    async fn show_pins(&mut self) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let answer = self.api.get(&format!("/channels/{}/pins", snowflake(&channel)?)).await?;
        let rows: Vec<(String, String, String, String, String)> = answer
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let time = m["timestamp"].as_str().and_then(parse_time).map(|t| stamp(&t)).unwrap_or_default();
                let body: String = names::unmark(&message_text(m, false)).chars().take(160).collect();
                Some((channel.clone(), m["id"].as_str()?.to_string(), display_name(&m["author"]), format!("📌 {time}"), body))
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            ui.set_found_total(rows.len() as i32);
            let items: Vec<FoundItem> = rows
                .into_iter()
                .map(|(channel, message, author, detail, text)| FoundItem {
                    channel: channel.into(),
                    message: message.into(),
                    author: author.into(),
                    detail: detail.into(),
                    text: text.into(),
                })
                .collect();
            ui.set_found(ModelRc::new(VecModel::from(items)));
            ui.set_found_title(i18n::t("Закреплённые сообщения").into());
            ui.set_found_open(true);
        });
        Ok(())
    }

    /// The biggest file we may send here: from the account's Nitro and the server's boost level, as far as READY says.
    fn upload_limit(&self) -> u64 {
        let tier = self
            .guild
            .as_ref()
            .and_then(|g| self.ready["guilds"].as_array()?.iter().find(|x| x["id"].as_str() == Some(g.as_str())))
            .map(|g| g["premium_tier"].as_u64().or(g["properties"]["premium_tier"].as_u64()).unwrap_or(0))
            .unwrap_or(0);
        Api::upload_limit(self.premium_type, tier)
    }

    /// Ctrl+V with pictures or files on the clipboard: they wait above the message box until it is sent.
    async fn paste(&mut self, list: Vec<clipboard::Pasted>) -> Result<(), String> {
        if self.channel.is_none() || list.is_empty() {
            return Ok(());
        }
        let limit = self.upload_limit();
        let room = clipboard::MAX_FILES.saturating_sub(self.pending_files.len());
        if list.len() > room {
            flash(&self.ui, i18n::tf("Не больше {} файлов за раз", &[&clipboard::MAX_FILES]));
        }
        for pasted in list.into_iter().take(room) {
            match tokio::task::spawn_blocking(move || clipboard::prepare(pasted, limit)).await.map_err(|e| e.to_string())? {
                Ok(file) => self.pending_files.push(file),
                Err(e) => flash(&self.ui, e),
            }
        }
        self.show_pending();
        Ok(())
    }

    /// The cards of the files waiting above the message box.
    fn show_pending(&self) {
        let files: Vec<(String, String, Option<Pixels>)> = self
            .pending_files
            .iter()
            .map(|f| {
                let thumb = f.thumb.as_ref().map(|t| Pixels::clone_from_slice(t.as_raw(), t.width(), t.height()));
                (f.name.clone(), human_size(f.bytes.len() as u64), thumb)
            })
            .collect();
        on_ui(&self.ui, move |ui| ui.set_pending_files(ModelRc::new(VecModel::from(files.into_iter().map(|(n, s, t)| pending_item(n, s, t)).collect::<Vec<_>>()))));
    }

    /// Asks for a file and sends it to the open channel (the person picks it, so nothing else goes out).
    async fn attach_file(&mut self) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let picked = tokio::task::spawn_blocking(|| rfd::FileDialog::new().set_title(i18n::t("Выбери файл для отправки")).pick_file())
            .await
            .map_err(|e| e.to_string())?;
        let Some(path) = picked else { return Ok(()) };
        let limit = self.upload_limit();
        let name = path.file_name().and_then(|n| n.to_str()).map_or_else(|| i18n::t("файл"), str::to_string);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // A line in the status bar while it goes: "Отправка «a.zip»: 40%".
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ui, counted, finished, label) = (self.ui.clone(), sent.clone(), done.clone(), name.clone());
        tokio::spawn(async move {
            while !finished.load(std::sync::atomic::Ordering::Relaxed) {
                let percent = if size == 0 { 0 } else { counted.load(std::sync::atomic::Ordering::Relaxed) * 100 / size };
                let text = i18n::tf("Отправка «{}»: {}%", &[&label, &percent]);
                on_ui(&ui, move |ui| ui.set_status(text.into()));
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        });
        let result = self.api.upload(&channel, &path, "", sent, limit).await;
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        // The status line goes back to the connection state once it's over.
        let ok = result.is_ok();
        on_ui(&self.ui, move |ui| ui.set_status(if ok { i18n::t("В сети").into() } else { "".into() }));
        result.map(|_| ())
    }
    /// Opens a channel at a message found by search: the messages around it.
    async fn jump_to(&mut self, channel: String, message: String) -> Result<(), String> {
        if self.channel.as_deref() != Some(channel.as_str()) {
            on_ui(&self.ui, {
                let shown = channel.clone();
                move |ui| ui.set_selected_channel(shown.into())
            });
        }
        self.open_messages(channel, Some(message)).await
    }

    /// Right-click on someone: works out what their menu offers (our rights in
    /// the server they're in or we're looking at), then shows it.
    /// A person's card: what we know at once, then what their profile says (asked once in a while, only on a click
    /// or when their DM opens). A profile Discord refuses leaves the card with what we know.
    async fn user_card(&mut self, id: String, popup: bool) -> Result<(), String> {
        snowflake(&id)?;
        let look = userprofile::Look { name: &display_name, avatar: &avatar_url, banner: &banner_url, decoration: &decoration_url };
        let known = self.users.get(&id).cloned().unwrap_or_else(|| json!({ "id": id }));
        let status = if id == self.me_id { 1 } else { self.presences.get(&id).map_or(home::OFFLINE, |p| p.status) };
        let me = id == self.me_id;
        let since = clock::joined(&id).unwrap_or_default();
        let cached = self.user_profiles.get(&id).filter(|(at, _)| at.elapsed() < USER_PROFILE_FOR).map(|(_, p)| p.clone());
        let hit = cached.is_some();
        let first = userprofile::card(&id, cached.as_ref().unwrap_or(&Value::Null), &known, &look);
        push_user_card(&self.ui, &self.images, first, status, me, since.clone(), popup);
        if hit {
            return Ok(());
        }
        let guild = self.guild.clone().filter(|g| g != HOME).map_or(String::new(), |g| format!("&guild_id={g}"));
        let path = format!("/users/{id}/profile?with_mutual_guilds=true&with_mutual_friends_count=true{guild}");
        match self.api.get(&path).await {
            Ok(profile) => {
                self.user_profiles.insert(id.clone(), (Instant::now(), profile.clone()));
                let full = userprofile::card(&id, &profile, &known, &look);
                push_user_card(&self.ui, &self.images, full, status, me, since, popup);
            }
            Err(e) => log_line(&format!("profile of {id}: {e}")),
        }
        Ok(())
    }

    async fn user_menu(&mut self, user: String) -> Result<(), String> {
        self.show_user_menu(user, None).await
    }

    /// Right-click on a row of the DM list: the person's menu with the chat's own lines, or a group's lines alone.
    async fn dm_menu(&mut self, channel: String) -> Result<(), String> {
        let Some(dm) = self.ready["private_channels"].as_array().and_then(|l| l.iter().find(|c| c["id"].as_str() == Some(channel.as_str()))).cloned() else {
            return Ok(());
        };
        let other = (dm["type"].as_u64() == Some(1)).then(|| dm["recipients"][0]["id"].as_str().or(dm["recipient_ids"][0].as_str()).map(String::from)).flatten();
        let place = usermenu::DmPlace { unread: self.unread.is_unread(&channel), muted: self.unread.muted(&channel), pinned: usermenu::dm_pinned(&channel), favorite: usermenu::dm_favorite(&channel), channel };
        if let Some(id) = other {
            return self.show_user_menu(id, Some(place)).await;
        }
        self.menu_guild = None;
        let name = self.channels.get(&place.channel).map(|c| c.name.clone()).unwrap_or_default();
        on_ui(&self.ui, move |ui| {
            ui.set_user_menu(UserMenuInfo {
                name: name.into(),
                me: true,
                group: true,
                dm_channel: place.channel.into(),
                dm_unread: place.unread,
                dm_muted: place.muted,
                dm_pinned: place.pinned,
                dm_favorite: place.favorite,
                ..Default::default()
            });
            ui.invoke_show_user_menu();
        });
        Ok(())
    }

    async fn show_user_menu(&mut self, user: String, dm: Option<usermenu::DmPlace>) -> Result<(), String> {
        let me = user == self.me_id;
        let relation = self.ready["relationships"].as_array().and_then(|l| l.iter().find(|r| r["id"].as_str() == Some(user.as_str()))).cloned().unwrap_or(Value::Null);
        let friend = relation["type"].as_u64() == Some(1);
        let blocked = relation["type"].as_u64() == Some(2);
        let ignored = relation["user_ignored"].as_bool() == Some(true);
        let friend_nick = relation["nickname"].as_str().unwrap_or("").to_string();
        // Where they sit in Discord's voice, if anywhere: voice actions happen in that server.
        let voice = self.discord_voice.iter().find_map(|(g, states)| states.get(&user).map(|vs| (g.clone(), vs.clone())));
        let guild = voice.as_ref().map(|(g, _)| g.clone()).or_else(|| self.guild.clone().filter(|g| g != HOME));
        let mine = match &guild {
            Some(g) => self.guild_perms(g).await,
            None => None,
        };
        let rights = mine.as_ref().filter(|_| !me).map(|p| p.rights()).unwrap_or_default();
        let own_nick = me && mine.as_ref().is_some_and(|p| p.rights().own_nick);
        let in_voice = voice.is_some();
        let flag = |k: &str| voice.as_ref().is_some_and(|(_, vs)| vs[k].as_bool() == Some(true));
        let here = voice.as_ref().and_then(|(_, vs)| vs["channel_id"].as_str().map(String::from));
        let targets: Vec<(String, String)> = guild
            .as_ref()
            .and_then(|g| self.voice_rooms.get(g))
            .into_iter()
            .flatten()
            .filter(|(id, _)| Some(id) != here.as_ref())
            .cloned()
            .collect();
        let name = voice
            .as_ref()
            .and_then(|(_, vs)| vs["nick"].as_str().map(readable))
            .or_else(|| self.users.get(&user).map(display_name))
            .unwrap_or_else(|| user.clone());
        let volume = audio::user_volume(&user) as f32;
        let (server_muted, server_deafened) = (flag("mute"), flag("deaf"));
        let in_guild = guild.is_some();
        let mut servers: Vec<(String, String)> = self.guild_names.iter().filter(|(id, _)| Some(*id) != guild.as_ref()).map(|(id, name)| (id.clone(), name.clone())).collect();
        servers.sort_by_key(|(_, name)| name.to_lowercase());
        let (sound_muted, video_hidden) = (usermenu::sound_muted(&user), usermenu::video_hidden(&user));
        let can_moderate = !me && (rights.mute || rights.deafen || rights.move_members || rights.kick || rights.ban || rights.timeout);
        let call_here = self.voice_channel.is_some() && (here == self.voice_channel || dm.as_ref().map(|d| &d.channel) == self.voice_channel.as_ref());
        let spotify_track = self.spotify.get(&user).map_or(String::new(), |(track, _)| track.clone());
        let fill = guild.clone().zip(mine);
        self.menu_guild = guild;
        let note = self.notes.get(&user).cloned().unwrap_or_default();
        let about = user.clone();
        on_ui(&self.ui, move |ui| {
            let targets: Vec<MoveTarget> = targets.into_iter().map(|(id, name)| MoveTarget { id: id.into(), name: name.into() }).collect();
            let invite_guilds: Vec<MoveTarget> = servers.into_iter().map(|(id, name)| MoveTarget { id: id.into(), name: name.into() }).collect();
            let (dm_channel, dm_unread, dm_muted, dm_pinned, dm_favorite) = dm.map_or((String::new(), false, false, false, false), |d| (d.channel, d.unread, d.muted, d.pinned, d.favorite));
            ui.set_user_menu(UserMenuInfo {
                can_moderate,
                can_code: !me && call_here,
                spotify_track: spotify_track.into(),
                dm_favorite,
                in_guild,
                blocked,
                ignored,
                sound_muted,
                video_hidden,
                friend_nick: friend_nick.into(),
                can_nick: own_nick || rights.nick,
                invite_guilds: ModelRc::new(VecModel::from(invite_guilds)),
                dm_channel: dm_channel.into(),
                dm_unread,
                dm_muted,
                dm_pinned,
                note: note.into(),
                id: user.into(),
                name: name.into(),
                me,
                friend,
                in_voice,
                volume,
                can_mute: rights.mute && in_voice,
                can_deafen: rights.deafen && in_voice,
                can_move: rights.move_members && in_voice,
                can_kick: rights.kick,
                can_ban: rights.ban,
                server_muted,
                server_deafened,
                targets: ModelRc::new(VecModel::from(targets)),
                ..Default::default()
            });
            ui.invoke_show_user_menu();
        });
        // Roles decide the rest, and only Discord knows theirs; the menu is up already.
        if let Some((guild, mine)) = fill {
            self.fill_member_menu(guild, about.clone(), mine).await;
        }
        self.fill_apps_menu(about.clone()).await;
        // The note (ours alone) fills in once Discord has said it; the menu is up already.
        if !me {
            self.load_note(about).await;
        }
        Ok(())
    }

    /// The profile window: first what is known at once, then the note and the mutual friends as they come.
    async fn full_profile(&mut self, id: String) -> Result<(), String> {
        snowflake(&id)?;
        self.user_card(id.clone(), true).await?;
        let profile = self.user_profiles.get(&id).map_or(Value::Null, |(_, p)| p.clone());
        let relation = self.ready["relationships"].as_array().and_then(|l| l.iter().find(|r| r["id"].as_str() == Some(id.as_str()))).cloned().unwrap_or(Value::Null);
        let since = relation["since"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map_or(String::new(), |t| clock::date(t.with_timezone(&Local).date_naive()));
        let (friend, blocked) = (relation["type"].as_u64() == Some(1), relation["type"].as_u64() == Some(2));
        let activity = self.presences.get(&id).map_or(String::new(), |p| readable(&p.activity));
        let accounts = userprofile::accounts(&profile);
        let servers: Vec<(String, String)> = profile["mutual_guilds"].as_array().into_iter().flatten().filter_map(|g| {
            let guild = g["id"].as_str()?;
            Some((guild.to_string(), self.guild_names.get(guild)?.clone()))
        }).collect();
        let shown = id.clone();
        on_ui(&self.ui, move |ui| {
            if ui.get_user_card().id.as_str() != shown {
                return;
            }
            let accounts: Vec<ProfileAccount> = accounts.into_iter().map(|a| ProfileAccount { kind: a.kind.into(), name: a.name.into(), url: a.url.unwrap_or_default().into() }).collect();
            let servers: Vec<ProfileRow> = servers.into_iter().map(|(id, name)| ProfileRow { id: id.into(), name: name.into(), ..Default::default() }).collect();
            ui.set_profile_extra(ProfileExtra {
                activity: activity.into(),
                friends_since: since.into(),
                friend,
                blocked,
                accounts: ModelRc::new(VecModel::from(accounts)),
                servers: ModelRc::new(VecModel::from(servers)),
                ..Default::default()
            });
        });
        if id == self.me_id {
            return Ok(());
        }
        let note = match self.api.note(&id).await {
            Ok(note) => {
                self.notes.insert(id.clone(), note.clone());
                note
            }
            Err(_) => self.notes.get(&id).cloned().unwrap_or_default(),
        };
        let friends = match self.api.get(&format!("/users/{id}/relationships")).await {
            Ok(list) => list.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                log_line(&format!("mutual friends: {}", discord::friendly(&e)));
                Vec::new()
            }
        };
        let rows: Vec<(String, String, String)> = friends.iter().filter_map(|u| Some((u["id"].as_str()?.to_string(), display_name(u), avatar_url(u)))).collect();
        let pictures: Vec<(String, String)> = rows.iter().map(|(id, _, url)| (id.clone(), url.clone())).collect();
        let shown = id.clone();
        on_ui(&self.ui, move |ui| {
            if ui.get_user_card().id.as_str() != shown {
                return;
            }
            let mut extra = ui.get_profile_extra();
            if extra.note.is_empty() {
                extra.note = note.into();
            }
            let friends: Vec<ProfileRow> = rows.into_iter().map(|(id, name, _)| ProfileRow { id: id.into(), name: name.into(), ..Default::default() }).collect();
            extra.friends = ModelRc::new(VecModel::from(friends));
            ui.set_profile_extra(extra);
        });
        let (images, ui) = (self.images.clone(), self.ui.clone());
        tokio::spawn(async move {
            for (friend, url) in pictures {
                let Some(px) = images.get(url, MEMBER_AVATAR_PX, MEMBER_AVATAR_PX).await else { continue };
                let shown = id.clone();
                on_ui(&ui, move |ui| {
                    if ui.get_user_card().id.as_str() != shown {
                        return;
                    }
                    let friends = ui.get_profile_extra().friends;
                    if let Some(row) = friends.iter().position(|f| f.id.as_str() == friend) {
                        update_row(friends, row, |f| {
                            f.avatar = Image::from_rgba8(px);
                            f.has_avatar = true;
                        });
                    }
                });
            }
        });
        Ok(())
    }

    /// The open menu with what their roles allow, and the roles and nickname they have in this server.
    async fn fill_member_menu(&mut self, guild: String, user: String, mine: perms::Me) {
        let me = user == self.me_id;
        let owner = self.ready["guilds"].as_array().and_then(|l| l.iter().find(|g| g["id"].as_str() == Some(guild.as_str()))).and_then(|g| g["owner_id"].as_str().or(g["properties"]["owner_id"].as_str())).map(String::from);
        let member = match self.api.get(&format!("/guilds/{guild}/members/{user}")).await {
            Ok(member) => member,
            // Not in this server: nothing here can be done to them.
            Err(e) if e.contains(" 404 ") => {
                on_ui(&self.ui, move |ui| {
                    let mut info = ui.get_user_menu();
                    if info.id.as_str() == user {
                        (info.can_kick, info.can_ban, info.can_nick, info.can_roles) = (false, false, false, false);
                        ui.set_user_menu(info);
                    }
                });
                return;
            }
            Err(e) => return log_line(&format!("member for the menu: {}", discord::friendly(&e))),
        };
        let theirs: Vec<String> = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
        let over = mine.rights_over(owner.as_deref() == Some(user.as_str()), &theirs);
        let own_nick = mine.rights().own_nick;
        let rows = usermenu::role_rows(&mine, &theirs);
        let nick = member["nick"].as_str().unwrap_or("").to_string();
        on_ui(&self.ui, move |ui| {
            let mut info = ui.get_user_menu();
            if info.id.as_str() != user {
                return;
            }
            info.can_kick = !me && over.kick;
            info.can_ban = !me && over.ban;
            info.can_nick = if me { own_nick } else { over.nick };
            info.can_roles = over.manage_roles;
            info.nick = nick.into();
            let roles: Vec<RoleItem> = rows.into_iter().map(|r| RoleItem { id: r.id.into(), name: r.name.into(), has: r.has, editable: r.editable }).collect();
            info.roles = ModelRc::new(VecModel::from(roles));
            ui.set_user_menu(info);
        });
    }

    fn note_spotify(&mut self, list: Vec<(String, Option<(String, i64)>)>) {
        for (user, track) in list {
            match track {
                Some(track) => {
                    self.spotify.insert(user, track);
                }
                None => {
                    self.spotify.remove(&user);
                }
            }
        }
    }

    /// The menu's "Приложения": what the apps of the open chat can do to a person.
    async fn fill_apps_menu(&mut self, user: String) {
        let Some(index) = self.slash_index().await else { return };
        let apps: Vec<MoveTarget> = index.on_user.iter().filter_map(|(app, c)| Some(MoveTarget { id: c["id"].as_str()?.into(), name: format!("{app}: {}", c["name"].as_str()?).into() })).collect();
        if apps.is_empty() {
            return;
        }
        on_ui(&self.ui, move |ui| {
            let mut info = ui.get_user_menu();
            if info.id.as_str() == user {
                info.apps = ModelRc::new(VecModel::from(apps));
                ui.set_user_menu(info);
            }
        });
    }

    /// A line of "Приложения": once, on the click. The app's answer comes as an ordinary message.
    async fn run_app_command(&mut self, command: &str, user: &str) -> Result<(), String> {
        let (guild, channel, _) = self.slash_key().ok_or_else(|| i18n::t("не на сервере"))?;
        let index = self.slash_index().await.ok_or_else(|| i18n::t("не на сервере"))?;
        let Some((_, command)) = index.on_user.iter().find(|(_, c)| c["id"].as_str() == Some(command)) else { return Ok(()) };
        let session = self.ready["session_id"].as_str().unwrap_or("").to_string();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let body = slash::user_interaction(command, &snowflake(user)?.to_string(), guild.as_deref(), &channel, &session, &slash::nonce(now));
        self.api.post("/interactions", body).await.map(|_| ()).map_err(|e| i18n::tf("Команда не сработала: {}", &[&discord::refusal(&e)]))
    }

    /// "Открыть с доступом модератора": what the server knows about them, and the time-outs if we may.
    async fn mod_view(&mut self, user: String) -> Result<(), String> {
        let guild = self.menu_guild.clone().ok_or_else(|| i18n::t("не на сервере"))?;
        let mine = self.guild_perms(&guild).await.ok_or_else(|| i18n::t("не на сервере"))?;
        let member = self.api.get(&format!("/guilds/{}/members/{}", snowflake(&guild)?, snowflake(&user)?)).await?;
        let theirs: Vec<String> = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
        let owner = self.ready["guilds"].as_array().and_then(|l| l.iter().find(|g| g["id"].as_str() == Some(guild.as_str()))).and_then(|g| g["owner_id"].as_str().or(g["properties"]["owner_id"].as_str()));
        let timeouts = user != self.me_id && mine.rights_over(owner == Some(user.as_str()), &theirs).timeout;
        let text = usermenu::mod_lines(&member, &mine.all_roles, clock::joined(&user), chrono::Utc::now()).join("\n");
        on_ui(&self.ui, move |ui| {
            ui.set_info_title(i18n::t("Доступ модератора").into());
            ui.set_info_text(text.into());
            ui.set_info_timeouts(timeouts);
            ui.set_info_open(true);
        });
        Ok(())
    }

    /// DMs live on the DM side: go there first, as the search box does.
    async fn open_dm(&mut self, user: String) -> Result<(), String> {
        if self.guild.as_deref() != Some(HOME) {
            on_ui(&self.ui, |ui| {
                ui.set_selected_guild(HOME.into());
                ui.set_show_call(false);
                ui.set_forum_open(false);
            });
            self.select_guild(HOME.into()).await?;
        }
        self.people_page = 1;
        self.person_open(user).await
    }

    /// A new invite to a server's first channel we can see: its link and the channel's name.
    async fn guild_invite(&mut self, guild: &str) -> Result<(String, String), String> {
        let mine = self.guild_perms(guild).await.ok_or_else(|| i18n::t("не на сервере"))?;
        let kept = self.ready["guilds"].as_array().and_then(|l| l.iter().find(|g| g["id"].as_str() == Some(guild))).and_then(|g| g["channels"].as_array()).cloned();
        let channels = match kept {
            Some(list) if !list.is_empty() => list,
            _ => self.api.get(&format!("/guilds/{}/channels", snowflake(guild)?)).await?.as_array().cloned().unwrap_or_default(),
        };
        let channel = usermenu::invite_channel(&channels, &mine).ok_or_else(|| i18n::t("На этом сервере нет канала для приглашения"))?;
        let name = channels.iter().find(|c| c["id"].as_str() == Some(channel.as_str())).and_then(|c| c["name"].as_str()).map(readable).unwrap_or_default();
        let made = self.api.create_invite(&channel).await?;
        let link = made["code"].as_str().filter(|c| invite::valid_code(c)).map(invite::link).ok_or_else(|| "Discord did not give a code".to_string())?;
        Ok((link, name))
    }

    /// The link in a DM with them.
    async fn send_link(&mut self, user: &str, link: &str) -> Result<(), String> {
        let dm = self.api.post("/users/@me/channels", json!({ "recipient_id": snowflake(user)?.to_string() })).await?;
        let to = dm["id"].as_str().ok_or_else(|| "Discord did not give a DM".to_string())?;
        self.api.post(&format!("/channels/{}/messages", snowflake(to)?), json!({ "content": link })).await?;
        Ok(())
    }

    /// "Пригласить на сервер" on a person: a new invite, sent to them in the DM.
    async fn invite_to(&mut self, guild: &str, user: &str) -> Result<(), String> {
        let (link, _) = self.guild_invite(guild).await?;
        self.send_link(user, &link).await?;
        notice(&self.ui, i18n::t("Приглашение отправлено"));
        Ok(())
    }

    /// The server menu's "Пригласить на сервер": one request, then the window with the link and all
    /// the friends (or Discord's refusal). Nobody gets anything until they are clicked.
    async fn invite_window(&mut self, guild: &str) {
        let (link, channel) = match self.guild_invite(guild).await {
            Ok(made) => made,
            Err(e) => {
                on_ui(&self.ui, move |ui| {
                    ui.set_made_invite_error(discord::refusal(&e).into());
                    ui.set_made_invite_state(3);
                });
                return;
            }
        };
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        let relationships = self.ready["relationships"].as_array().cloned().unwrap_or_default();
        let rows = home::friend_rows(&relationships, &self.users, &self.presences, 1, &look);
        let username = |id: &str| {
            let told = relationships.iter().find(|r| r["id"].as_str() == Some(id)).and_then(|r| r["user"]["username"].as_str());
            told.or(self.users.get(id).and_then(|u| u["username"].as_str())).unwrap_or_default().to_string()
        };
        let names: Vec<String> = rows.iter().map(|p| username(&p.id)).collect();
        let jobs: Vec<ImageJob> = rows
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.avatar_url.is_empty())
            .map(|(row, p)| ImageJob { row, url: p.avatar_url.clone(), w: DM_AVATAR_PX, h: DM_AVATAR_PX, ..Default::default() })
            .collect();
        let ids: Vec<String> = rows.iter().map(|p| p.id.clone()).collect();
        on_ui(&self.ui, move |ui| {
            let items = rows.iter().zip(names).map(|(p, username)| PersonItem {
                id: p.id.clone().into(),
                name: readable(&p.name).into(),
                initial: p.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                subtitle: username.into(),
                ..Default::default()
            });
            INVITE_PEOPLE.set(items.collect());
            show_invite_people(&ui);
            ui.set_made_invite_link(link.into());
            ui.set_made_invite_channel(channel.into());
            ui.set_made_invite_state(2);
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let Some(id) = ids.get(job.row).cloned() else { return };
            on_ui(&ui, move |ui| {
                change_invite_person(&ui, &id, |p| {
                    p.avatar = Image::from_rgba8(pixels);
                    p.has_avatar = true;
                });
            });
        });
    }

    /// "Покинуть сервер", after its question: it leaves the rail once Discord has said yes.
    async fn leave_server(&mut self, guild: String) -> Result<(), String> {
        self.api.leave_guild(&snowflake(&guild)?.to_string()).await?;
        self.guild_gone(guild).await
    }

    /// A server we joined somewhere else (the official client, when Discord wanted its check there)
    /// goes on the rail at once; one joined here is already there, and gets its details.
    fn guild_appeared(&mut self, data: Value) {
        let Some((id, name, icon)) = new_guild(&data) else { return };
        names::note_roles(data["roles"].as_array().map(Vec::as_slice).unwrap_or_default());
        names::note_channels(data["channels"].as_array().map(Vec::as_slice).unwrap_or_default());
        if let Some(list) = self.ready["guilds"].as_array_mut() {
            if !list.iter().any(|g| g["id"].as_str() == Some(id.as_str())) {
                list.push(data);
            }
        }
        if self.guild_names.contains_key(&id) {
            return;
        }
        self.guild_names.insert(id.clone(), name.clone());
        let item = Guild { id: id.clone(), name, ..Default::default() };
        on_ui(&self.ui, move |ui| {
            let model = ui.get_guilds();
            if let Some(list) = model.as_any().downcast_ref::<VecModel<GuildItem>>() {
                list.push(item.item());
            }
            sync_rail(&ui);
        });
        let Some(hash) = icon else { return };
        let job = ImageJob { url: format!("{CDN}/icons/{id}/{hash}.png?size=128"), w: GUILD_ICON_PX, h: GUILD_ICON_PX, ..Default::default() };
        let ui = self.ui.clone();
        load_images(self.images.clone(), vec![job], move |_, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let id = id.clone();
            on_ui(&ui, move |ui| {
                let model = ui.get_guilds();
                if let Some(row) = model.iter().position(|g| g.id == id) {
                    update_row(model, row, |g| {
                        g.icon = Image::from_rgba8(pixels);
                        g.has_icon = true;
                    });
                }
                sync_rail(&ui);
            });
        });
    }

    /// A server we are out of, by our own click or from elsewhere, leaves the rail.
    async fn guild_gone(&mut self, guild: String) -> Result<(), String> {
        if let Some(list) = self.ready["guilds"].as_array_mut() {
            list.retain(|g| g["id"].as_str() != Some(guild.as_str()));
        }
        self.guild_names.remove(&guild);
        let open = self.guild.as_deref() == Some(guild.as_str());
        on_ui(&self.ui, move |ui| {
            let model = ui.get_guilds();
            if let Some(list) = model.as_any().downcast_ref::<VecModel<GuildItem>>() {
                if let Some(row) = list.iter().position(|g| g.id == guild) {
                    list.remove(row);
                }
            }
            folders::forget(&guild);
            if open {
                ui.set_selected_guild(HOME.into());
                ui.set_show_call(false);
                ui.set_forum_open(false);
            }
            sync_rail(&ui);
        });
        if open {
            self.select_guild(HOME.into()).await?;
        }
        Ok(())
    }

    /// The menu's choices that carry something after their name, and the newer ones.
    async fn user_action_more(&mut self, action: &str, user: String) -> Result<(), String> {
        let guild = self.menu_guild.clone();
        let in_guild = || guild.clone().ok_or_else(|| i18n::t("не на сервере"));
        if let Some(channel) = action.strip_prefix("move:") {
            return self.api.edit_member(&in_guild()?, &user, json!({ "channel_id": channel })).await.map(|_| ());
        }
        let role = action.strip_prefix("role-add:").map(|r| (r, true)).or_else(|| action.strip_prefix("role-remove:").map(|r| (r, false)));
        if let Some((role, add)) = role {
            let path = format!("/guilds/{}/members/{}/roles/{}", snowflake(&in_guild()?)?, snowflake(&user)?, snowflake(role)?);
            if add {
                self.api.put(&path).await?;
            } else {
                self.api.delete(&path).await?;
            }
            let role = role.to_string();
            on_ui(&self.ui, move |ui| {
                let roles = ui.get_user_menu().roles;
                if let Some(row) = roles.iter().position(|r| r.id.as_str() == role) {
                    update_row(roles, row, |r| r.has = add);
                }
            });
            return Ok(());
        }
        if let Some(text) = action.strip_prefix("nick:") {
            let text = usermenu::nick(text);
            let server = in_guild()?;
            if user == self.me_id {
                self.api.patch(&format!("/guilds/{}/members/@me", snowflake(&server)?), json!({ "nick": text })).await?;
            } else {
                self.api.edit_member(&server, &user, json!({ "nick": text })).await?;
            }
            return Ok(());
        }
        if let Some(text) = action.strip_prefix("friend-nick:") {
            return self.api.friend_nick(&user, &usermenu::nick(text)).await.map(|_| ());
        }
        if let Some(server) = action.strip_prefix("invite:") {
            return self.invite_to(server, &user).await;
        }
        if let Some(command) = action.strip_prefix("app:") {
            return self.run_app_command(command, &user).await;
        }
        if let Some(seconds) = action.strip_prefix("timeout:") {
            let until = usermenu::timeout_until(seconds.parse().unwrap_or(0), chrono::Utc::now());
            return self.api.edit_member(&in_guild()?, &user, json!({ "communication_disabled_until": until })).await.map(|_| ());
        }
        if let Some(channel) = action.strip_prefix("dm-fav:") {
            usermenu::toggle_favorite(channel);
            self.refresh_home();
            return Ok(());
        }
        if let Some(channel) = action.strip_prefix("dm-read:") {
            let channel = channel.to_string();
            self.read_channel(&channel, 0).await;
            return Ok(());
        }
        if let Some(channel) = action.strip_prefix("dm-pin:") {
            usermenu::toggle_pinned(channel);
            self.refresh_home();
            return Ok(());
        }
        if let Some(rest) = action.strip_prefix("dm-mute:") {
            // The channel, and for how many minutes when not until turned off.
            let (channel, what) = rest.split_once(':').map_or((rest.to_string(), "mute".to_string()), |(c, minutes)| (c.to_string(), format!("mute:{minutes}")));
            return self.notify_change(HOME.into(), channel, &what).await;
        }
        if let Some(channel) = action.strip_prefix("dm-unmute:") {
            return self.notify_change(HOME.into(), channel.to_string(), "unmute").await;
        }
        if let Some(channel) = action.strip_prefix("dm-close:") {
            self.api.delete(&format!("/channels/{}", snowflake(channel)?)).await?;
            // The chat that was just closed may be the one on screen.
            if self.channel.as_deref() == Some(channel) {
                on_ui(&self.ui, |ui| {
                    ui.set_people_page(1);
                    ui.set_selected_channel("friends".into());
                    ui.invoke_channel_clicked("friends".into());
                });
            }
            return Ok(());
        }
        match action {
            "call" => {
                self.open_dm(user).await?;
                self.join_dm_call(true)
            }
            "mention" => {
                let id = snowflake(&user)?;
                let name = self.users.get(&user).map(display_name).unwrap_or_else(|| user.clone());
                let insert = format!("@{name}");
                let text = format!("{}{insert} ", self.input_text);
                self.picked_mentions.push((insert, format!("<@{id}>")));
                self.input_text = text.clone();
                on_ui(&self.ui, move |ui| ui.invoke_set_input(text.into()));
                Ok(())
            }
            "mod-view" => self.mod_view(user).await,
            "code" => self.voice.send(voice::VoiceCmd::VerifyCode(user)).map_err(|e| e.to_string()),
            "listen" => {
                let Some((track, started)) = self.spotify.get(&user).cloned() else { return Ok(()) };
                self.api.spotify_play(&track, chrono::Utc::now().timestamp_millis() - started).await
            }
            "block" => self.api.block(&user).await.map(|_| ()),
            "unblock" => self.api.remove_friend(&snowflake(&user)?.to_string()).await.map(|_| ()),
            "ignore" | "unignore" => self.api.ignore(&user, action == "ignore").await.map(|_| ()),
            "sound-mute" => {
                usermenu::toggle_sound(&user);
                Ok(())
            }
            "video-hide" => {
                if usermenu::toggle_video(&user) {
                    set_camera_frame(&self.ui, user, None);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn load_note(&mut self, user: String) {
        let note = match self.api.note(&user).await {
            Ok(note) => note,
            Err(e) => {
                log_line(&format!("note: not loaded ({})", discord::friendly(&e)));
                return;
            }
        };
        self.notes.insert(user.clone(), note.clone());
        on_ui(&self.ui, move |ui| {
            let mut info = ui.get_user_menu();
            // Only into the same person's menu, and not over what is being typed there.
            if info.id.as_str() == user && info.note.is_empty() {
                info.note = note.into();
                ui.set_user_menu(info);
            }
        });
    }

    /// The note field was left (Enter, or it lost focus): saved once, only when it says something new.
    async fn save_note(&mut self, user: String, typed: String) -> Result<(), String> {
        let saved = self.notes.get(&user).cloned().unwrap_or_default();
        let Some(text) = notes::to_save(&saved, &typed) else { return Ok(()) };
        self.api.set_note(&user, &text).await?;
        self.notes.insert(user, text);
        Ok(())
    }

    /// A choice from someone's menu. Moderator actions go to the server the menu was for.
    async fn user_action(&mut self, action: &str, user: String) -> Result<(), String> {
        let guild = || self.menu_guild.clone().ok_or_else(|| i18n::t("не на сервере"));
        match action {
            "dm" => return self.open_dm(user).await,
            // `user` is the server's id here.
            "leave-server" => return self.leave_server(user).await,
            "friend-add" => self.api.request_friend(&user).await,
            "friend-remove" => self.api.remove_friend(&snowflake(&user)?.to_string()).await,
            "mute" | "unmute" => self.api.edit_member(&guild()?, &user, json!({ "mute": action == "mute" })).await,
            "deafen" | "undeafen" => self.api.edit_member(&guild()?, &user, json!({ "deaf": action == "deafen" })).await,
            "disconnect" => self.api.edit_member(&guild()?, &user, json!({ "channel_id": null })).await,
            // `user` is the message's id here; only ours can be deleted, Discord checks it too.
            "delete-message" => match self.channel.clone() {
                Some(channel) => self.api.delete_message(&channel, &user).await,
                None => return Ok(()),
            },
            "pin" | "unpin" => match self.channel.clone() {
                Some(channel) => {
                    let done = self.api.pin(&channel, &user, action == "pin").await;
                    if done.is_ok() {
                        let pinned = action == "pin";
                        let id = user.clone();
                        on_ui(&self.ui, move |ui| {
                            if let Some(row) = message_row(&ui, &id) {
                                update_row(ui.get_messages(), row, |m| m.pinned = pinned);
                            }
                        });
                    }
                    done
                }
                None => return Ok(()),
            },
            "kick" => self.api.kick(&guild()?, &user).await,
            "voice-server" => return self.answer_server(user, true),
            "voice-refuse" => return self.answer_server(user, false),
            "ban" => self.api.ban(&guild()?, &user).await,
            other => return self.user_action_more(other, user).await,
        }
        .map(|_| ())
    }

    /// A forum channel: its posts instead of a chat.
    async fn open_forum(&mut self, id: String) -> Result<(), String> {
        let channel = snowflake(&id)?;
        let mut results = Vec::new();
        let mut failure = None;
        for path in forum::search_paths(channel) {
            match self.api.get(&path).await {
                Ok(found) => results.push(found),
                Err(e) => failure = Some(e),
            }
        }
        if self.channel.as_deref() != Some(id.as_str()) {
            return Ok(());
        }
        let posts = forum::posts(&results, Local::now().timestamp_millis());
        let notice = match (&failure, results.is_empty(), posts.is_empty()) {
            (Some(e), true, _) if e.contains(" 403 ") => i18n::t("🔒 Нет доступа к этому каналу"),
            (Some(e), true, _) => i18n::tf("Не удалось загрузить публикации: {}", &[&e]),
            (_, _, true) => i18n::t("Здесь пока нет публикаций"),
            _ => String::new(),
        };
        // Posts open like channels; their names go where channel names are looked up.
        for p in &posts {
            self.channels.insert(p.id.clone(), ChannelInfo { name: p.title.clone(), topic: String::new(), dm: false, forum: false });
            self.thread_ids.insert(p.id.clone());
        }
        self.forum_posts = posts.iter().map(|p| p.id.clone()).collect();
        self.history = History::default();
        let name = self.channels.get(&id).map(|c| c.name.clone()).unwrap_or_default();
        let jobs: Vec<ImageJob> = posts
            .iter()
            .enumerate()
            .filter_map(|(row, p)| Some(ImageJob { row, url: p.image.clone()?, w: FORUM_THUMB_PX, h: FORUM_THUMB_PX, ..Default::default() }))
            .collect();
        let shown = id.clone();
        on_ui(&self.ui, move |ui| {
            let items: Vec<ForumPostItem> = posts
                .iter()
                .map(|p| ForumPostItem {
                    id: p.id.clone().into(),
                    title: p.title.clone().into(),
                    author: p.author.clone().into(),
                    preview: p.preview.clone().into(),
                    reaction: p.reaction.clone().into(),
                    reactions: p.reactions as i32,
                    replies: p.replies as i32,
                    when: p.when.clone().into(),
                    image: Image::default(),
                    has_image: false,
                    pinned: p.pinned,
                })
                .collect();
            ANIMATIONS.with(|a| a.borrow_mut().clear());
            ui.set_selected_channel(id.into());
            ui.set_channel_title(name.clone().into());
            ui.set_channel_topic("".into());
            ui.set_window_title(format!("#{name} — Nimbus").into());
            ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
            ui.set_forum_posts(ModelRc::new(VecModel::from(items)));
            ui.set_forum_open(true);
            ui.set_channel_is_thread(false);
            ui.set_notice(notice.into());
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let (row, shown) = (job.row, shown.clone());
            on_ui(&ui, move |ui| {
                if ui.get_selected_channel().as_str() == shown {
                    update_row(ui.get_forum_posts(), row, |p| {
                        p.image = Image::from_rgba8(pixels);
                        p.has_image = true;
                    });
                }
            });
        });
        Ok(())
    }

    fn load_message_images(&self, channel: String, jobs: Vec<ImageJob>) {
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let (message, is_picture, deco, sticker, channel) = (job.message.clone(), job.is_picture, job.deco, job.sticker, channel.clone());
            on_ui(&ui, move |ui| {
                if ui.get_selected_channel().as_str() != channel {
                    return;
                }
                let Some(row) = message_row(&ui, &message) else { return };
                let Some(loaded) = loaded else {
                    // Nothing worked: drop the empty frame instead of leaving a grey box; a sticker shows its name.
                    if is_picture {
                        update_row(ui.get_messages(), row, |m| m.has_picture = false);
                    } else if sticker {
                        update_row(ui.get_messages(), row, |m| m.sticker_kind = 2);
                    }
                    return;
                };
                let first = Image::from_rgba8(loaded.first());
                if sticker {
                    update_row(ui.get_messages(), row, |m| m.sticker = first);
                    if let Loaded::Frames(frames) = loaded {
                        let frames: Vec<(Image, u32)> = frames.into_iter().map(|(px, delay)| (Image::from_rgba8(px), delay)).collect();
                        let next = Instant::now() + Duration::from_millis(frames[0].1 as u64);
                        ANIMATIONS.with(|a| a.borrow_mut().push(Animation { channel, message, sticker: true, frames, current: 0, next }));
                    }
                    return;
                }
                if deco {
                    update_row(ui.get_messages(), row, |m| {
                        m.decoration = first;
                        m.has_decoration = true;
                    });
                    return;
                }
                update_row(ui.get_messages(), row, |m| {
                    if is_picture {
                        m.picture = first;
                    } else {
                        m.avatar = first;
                    }
                });
                if let Loaded::Frames(frames) = loaded {
                    let frames: Vec<(Image, u32)> = frames.into_iter().map(|(px, delay)| (Image::from_rgba8(px), delay)).collect();
                    let next = Instant::now() + Duration::from_millis(frames[0].1 as u64);
                    ANIMATIONS.with(|a| a.borrow_mut().push(Animation { channel, message, sticker: false, frames, current: 0, next }));
                }
            });
        });
    }

    /// Takes in what the list's trimming on the UI side left behind (see `TRIM`).
    fn apply_trim(&mut self) {
        let mut trim = TRIM.lock().unwrap();
        if let Some(id) = trim.0.take() {
            self.history.newest = Some(id);
            self.history.detached = true;
        }
        if let Some(id) = trim.1.take() {
            self.history.oldest = Some(id);
            self.history.done = false;
        }
    }

    fn on_message(&mut self, m: Value) {
        self.apply_trim();
        // Unread pills and mentions: our own messages are read already, and
        // the open channel is being read as it comes in.
        let cid = m["channel_id"].as_str().unwrap_or("").to_string();
        let id: u64 = m["id"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mine = m["author"]["id"].as_str() == Some(self.me_id.as_str());
        let dm = m["guild_id"].is_null();
        let guild = m["guild_id"].as_str();
        // @everyone and @here don't count where the server has them silenced for us.
        let everyone = m["mention_everyone"].as_bool() == Some(true) && !self.unread.settings.everyone_silenced(guild);
        let named = m["mentions"].as_array().is_some_and(|list| list.iter().any(|u| u["id"].as_str() == Some(self.me_id.as_str())));
        let mentions_me = !mine && (dm || everyone || named);
        self.unread.on_message(&cid, guild, id, mentions_me);
        let arrival = notify::Arrival {
            mine,
            dm,
            mentions_me: named,
            mentions_everyone: everyone,
            muted: self.unread.muted(&cid),
            level: self.unread.settings.level(&cid, guild),
            focused: false, // the window's own state is looked at on the UI thread
        };
        let prefs = notify::current();
        if notify::should_notify(&prefs, &arrival) {
            let place = if dm {
                i18n::t("Личное сообщение")
            } else {
                let server = guild.and_then(|g| self.guild_names.get(g)).cloned().unwrap_or_default();
                let channel = self.channels.get(&cid).map(|c| format!("#{}", c.name)).unwrap_or_default();
                [server, channel].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" · ")
            };
            let shown = forward::snapshot(&m).unwrap_or(&m);
            let text = notify::toast_text(&prefs, &display_name(&m["author"]), &place, &names::unmark(&message_text(shown, false)));
            let author = m["author"]["id"].as_str().unwrap_or("").to_string();
            let who = ToastSource { author: author.clone(), guild: guild.unwrap_or("").to_string(), channel: cid.clone() };
            // The picture comes on its own; hidden text means nobody's face either.
            if !text.anonymous {
                let (images, ui, url) = (self.images.clone(), self.ui.clone(), avatar_url(&m["author"]));
                tokio::spawn(async move {
                    if let Some(px) = images.get(url, 80, 80).await {
                        on_ui(&ui, move |_| set_toast_avatar(&author, px));
                    }
                });
            }
            on_ui(&self.ui, move |ui| show_toast(&ui, text, who, prefs.sound));
        }
        self.typing.stop(&cid, m["author"]["id"].as_str().unwrap_or(""));
        self.show_typing();
        if mine || self.channel.as_deref() == Some(cid.as_str()) {
            self.unread.mark_read(&cid, id);
        }
        self.refresh_badges();
        // A DM that just got a message moves to the top of the list.
        if m["guild_id"].is_null() {
            let cid = m["channel_id"].as_str().unwrap_or("");
            let mut moved = false;
            if let Some(private) = self.ready["private_channels"].as_array_mut() {
                if let Some(c) = private.iter_mut().find(|c| c["id"].as_str() == Some(cid)) {
                    c["last_message_id"] = m["id"].clone();
                    moved = true;
                }
            }
            if moved {
                self.refresh_home();
            }
        }
        let Some(channel) = self.channel.clone() else { return };
        if m["channel_id"].as_str() != Some(channel.as_str()) || self.history.detached {
            return;
        }
        self.note_raw(&m);
        let mut jobs = Vec::new();
        self.learn_roles(&m);
        let mut msg = to_msg(&m, &mut self.last, &mut jobs);
        msg.color = self.color_of(&msg.author_id);
        self.message_rows += 1;
        if !msg.reactions.is_empty() {
            self.reactions.insert(msg.id.clone(), msg.reactions.clone());
        }
        self.load_emojis(emoji_urls([&msg]));
        self.note_extras(std::slice::from_ref(&msg));
        let mine = msg.author_id == self.me_id;
        on_ui(&self.ui, move |ui| {
            // Someone reading far up keeps their place; their own message brings them down.
            let follow = mine || ui.invoke_near_bottom();
            push_fresh(&ui, msg.item());
            if follow {
                ui.invoke_follow_bottom();
            }
        });
        self.load_message_images(channel, jobs);
    }

    /// Colours the names of the authors of messages about to be shown.
    fn paint(&self, msgs: &mut [Msg]) {
        for m in msgs {
            m.color = self.color_of(&m.author_id);
        }
    }

    /// The name colour of someone in the open server, if we know their roles.
    fn color_of(&self, user: &str) -> Option<u32> {
        let guild = self.members_guild.as_ref()?;
        let roles = self.role_of.get(user)?;
        members::name_color(roles, self.role_tables.get(guild)?)
    }

    /// A message from a server carries its author's roles; keep them.
    fn learn_roles(&mut self, m: &Value) {
        let (Some(user), Some(roles)) = (m["author"]["id"].as_str(), m["member"]["roles"].as_array()) else { return };
        let roles = roles.iter().filter_map(|r| r.as_str().map(String::from)).collect();
        self.role_of.insert(user.to_string(), roles);
    }

    /// The emoji of the open server: (id, name, animated); asked for once per server.
    async fn server_emojis(&mut self) -> Vec<(String, String, bool)> {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return Vec::new() };
        if let Some(known) = self.guild_emojis.get(&guild) {
            return known.clone();
        }
        let from_ready = self.ready["guilds"].as_array().and_then(|g| g.iter().find(|g| g["id"].as_str() == Some(guild.as_str()))).and_then(|g| g["emojis"].as_array().cloned());
        let list = match from_ready {
            Some(list) if !list.is_empty() => list,
            _ => match snowflake(&guild) {
                Ok(id) => self.api.get(&format!("/guilds/{id}/emojis")).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default(),
                Err(_) => Vec::new(),
            },
        };
        let emojis: Vec<(String, String, bool)> = list
            .iter()
            .filter(|e| e["available"].as_bool() != Some(false))
            .filter_map(|e| Some((e["id"].as_str()?.to_string(), e["name"].as_str()?.to_string(), e["animated"].as_bool().unwrap_or(false))))
            .collect();
        self.guild_emojis.insert(guild, emojis.clone());
        emojis
    }

    /// The emoji panel for what's typed in its search box: ours first, then the server's.
    async fn emoji_query(&mut self, query: String) -> Result<(), String> {
        let server = self.server_emojis().await;
        // (glyph, what goes into the box, reaction name, picture)
        let mut rows: Vec<(String, String, String, Option<String>)> =
            emoji::search(&query).into_iter().map(|e| (e.to_string(), e.to_string(), e.to_string(), None)).collect();
        for (id, name, animated) in emoji::server_matches(&server, &query, 24) {
            rows.push((String::new(), emoji::markup(id, name, *animated), format!("{name}:{id}"), Some(format!("{CDN}/emojis/{id}.png?size=48"))));
        }
        let jobs: Vec<ImageJob> = rows
            .iter()
            .filter_map(|(_, _, key, url)| Some(ImageJob { message: key.clone(), url: url.clone()?, w: EMOJI_PX, h: EMOJI_PX, ..Default::default() }))
            .collect();
        on_ui(&self.ui, move |ui| {
            let items: Vec<EmojiItem> = rows
                .into_iter()
                .map(|(glyph, insert, key, _)| EmojiItem { glyph: glyph.into(), insert: insert.into(), key: key.into(), image: Image::default(), has_image: false })
                .collect();
            ui.set_emoji_items(ModelRc::new(VecModel::from(items)));
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let key = job.message.clone();
            on_ui(&ui, move |ui| {
                let model = ui.get_emoji_items();
                for row in 0..model.row_count() {
                    if model.row_data(row).is_some_and(|e| e.key.as_str() == key) {
                        update_row(model.clone(), row, |e| {
                            e.image = Image::from_rgba8(pixels.clone());
                            e.has_image = true;
                        });
                    }
                }
            });
        });
        Ok(())
    }

    /// What @ or # at the end of the box could mean: people of the server, or its channels.
    fn suggest(&mut self, text: String) {
        self.input_text = text.clone();
        let found = mentions::token(&text).map(|(start, sigil, query)| {
            let query_lower = query.to_lowercase();
            let mut list: Vec<mentions::Suggestion> = Vec::new();
            if sigil == '@' {
                for item in self.members.items() {
                    if let members::Item::Member { id, user, nick, .. } = item {
                        let name = nick.map(|n| readable(&n)).unwrap_or_else(|| display_name(&user));
                        let handle = user["username"].as_str().unwrap_or("").to_string();
                        if mentions::matches(&query_lower, &[&name, &handle]) {
                            list.push(mentions::Suggestion { label: name.clone(), detail: handle, insert: format!("@{name}"), token: format!("<@{id}>") });
                        }
                    }
                }
            } else if let Some(guild) = self.guild.as_deref().filter(|g| *g != HOME) {
                let mut names: Vec<(&String, &ChannelInfo)> = self.channels.iter().filter(|(_, c)| !c.dm && !c.forum).collect();
                names.sort_by(|a, b| a.1.name.cmp(&b.1.name));
                let _ = guild;
                for (id, c) in names {
                    if mentions::matches(&query_lower, &[&c.name]) {
                        list.push(mentions::Suggestion { label: format!("#{}", c.name), detail: String::new(), insert: format!("#{}", c.name), token: format!("<#{id}>") });
                    }
                }
            }
            list.truncate(7);
            (start, list)
        });
        let (start, list) = found.unwrap_or((0, Vec::new()));
        self.suggest_at = start;
        self.suggestions = list.clone();
        on_ui(&self.ui, move |ui| {
            let items: Vec<SuggestItem> = list.iter().map(|s| SuggestItem { label: s.label.clone().into(), detail: s.detail.clone().into() }).collect();
            ui.set_suggest_open(!items.is_empty());
            ui.set_suggest_index(0);
            ui.set_suggest(ModelRc::new(VecModel::from(items)));
        });
    }

    /// Puts the picked name into the box; on sending it becomes the real mention.
    fn pick_suggestion(&mut self, index: usize) {
        let Some(s) = self.suggestions.get(index).cloned() else { return };
        let head: String = self.input_text.chars().take(self.suggest_at).collect();
        let text = format!("{head}{} ", s.insert);
        self.picked_mentions.push((s.insert.clone(), s.token.clone()));
        self.input_text = text.clone();
        self.suggestions.clear();
        on_ui(&self.ui, move |ui| {
            ui.set_suggest_open(false);
            ui.invoke_set_input(text.into());
        });
    }

    /// Which cached command list the open chat uses: its server's, or the DM's own.
    fn slash_key(&self) -> Option<(Option<String>, String, String)> {
        let channel = self.channel.clone()?;
        let guild = self.guild.clone().filter(|g| g != HOME);
        let key = guild.clone().unwrap_or_else(|| channel.clone());
        Some((guild, channel, key))
    }

    /// The open chat's slash commands: as last asked if that was lately, else asked again. A failed
    /// request is remembered for a minute, so typing doesn't ask on every letter.
    async fn slash_index(&mut self) -> Option<Arc<slash::Index>> {
        let (guild, channel, key) = self.slash_key()?;
        if let Some((at, index)) = self.slash.get(&key) {
            if at.elapsed() < GUILD_CACHE_FOR {
                return Some(index.clone());
            }
        }
        let path = slash::index_path(guild.as_deref(), &channel).ok()?;
        let (at, index) = match self.api.get(&path).await {
            Ok(answer) => (Instant::now(), Arc::new(slash::parse_index(&answer))),
            Err(e) => {
                log_line(&format!("slash commands: {e}"));
                let retry = Instant::now().checked_sub(GUILD_CACHE_FOR - Duration::from_secs(60)).unwrap_or_else(Instant::now);
                (retry, Arc::new(slash::Index::default()))
            }
        };
        self.slash.insert(key, (at, index.clone()));
        Some(index)
    }

    /// The message box changed: the command panel while `/name` is typed, the picked command's options
    /// once a space follows it, nothing otherwise.
    async fn slash_input(&mut self, text: &str) {
        if !text.starts_with('/') {
            self.slash_hide();
            return;
        }
        let Some(index) = self.slash_index().await else { return };
        self.slash_shown = true;
        if let Some(query) = slash::query(text) {
            let rows = slash::rows(&index, &query, &self.slash_recent);
            let (lines, strip, height) = slash_panel(&index, &rows);
            self.slash_rows = rows;
            on_ui(&self.ui, move |ui| {
                ui.set_slash_hint_name("".into());
                show_slash(&ui, lines, strip, height);
            });
            self.load_slash_icons(&index);
        } else {
            let hint = slash::find(&index, text).map(|(entry, rest)| (format!("/{}", entry.name), slash::hint(entry, rest)));
            on_ui(&self.ui, move |ui| {
                ui.set_slash_open(false);
                let (name, (pills, active)) = hint.unwrap_or_default();
                ui.set_slash_hint_name(name.into());
                ui.set_slash_hint(ModelRc::new(VecModel::from(pills.into_iter().map(slint::SharedString::from).collect::<Vec<_>>())));
                ui.set_slash_hint_active(active);
            });
        }
    }

    /// Closes the command panel and the hint, if they are on screen.
    fn slash_hide(&mut self) {
        if std::mem::take(&mut self.slash_shown) {
            self.slash_rows.clear();
            on_ui(&self.ui, |ui| {
                ui.set_slash_open(false);
                ui.set_slash_hint_name("".into());
            });
        }
    }

    /// The bots' icons in the panel, each asked for once.
    fn load_slash_icons(&mut self, index: &slash::Index) {
        let jobs: Vec<ImageJob> = index
            .apps
            .iter()
            .filter(|a| self.slash_icons_asked.insert(a.id.clone()))
            .filter_map(|a| Some(ImageJob { message: a.id.clone(), url: a.icon.clone()?, w: SLASH_ICON_PX, h: SLASH_ICON_PX, ..Default::default() }))
            .collect();
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(pixels) = loaded.map(|l| l.first()) else { return };
            let app = job.message.clone();
            on_ui(&ui, move |ui| {
                let image = Image::from_rgba8(pixels);
                SLASH_ICONS.with(|icons| icons.borrow_mut().insert(app.clone(), image.clone()));
                let rows = ui.get_slash_rows();
                for i in (0..rows.row_count()).filter(|&i| rows.row_data(i).is_some_and(|r| r.app.as_str() == app)) {
                    update_row(rows.clone(), i, |r| {
                        r.icon = image.clone();
                        r.has_icon = true;
                    });
                }
                let strip = ui.get_slash_apps();
                for i in (0..strip.row_count()).filter(|&i| strip.row_data(i).is_some_and(|a| a.id.as_str() == app)) {
                    update_row(strip.clone(), i, |a| {
                        a.icon = image.clone();
                        a.has_icon = true;
                    });
                }
            });
        });
    }

    /// A command picked in the panel: its name goes into the box, its options show over it.
    fn slash_pick(&mut self, row: usize) {
        let Some(slash::Row::Command(i)) = self.slash_rows.get(row).cloned() else { return };
        let Some((_, _, key)) = self.slash_key() else { return };
        let Some(entry) = self.slash.get(&key).and_then(|(_, index)| index.entries.get(i).cloned()) else { return };
        let text = format!("/{} ", entry.name);
        let (pills, active) = slash::hint(&entry, "");
        self.input_text = text.clone();
        self.slash_rows.clear();
        on_ui(&self.ui, move |ui| {
            ui.set_slash_open(false);
            ui.set_slash_hint_name(format!("/{}", entry.name).into());
            ui.set_slash_hint(ModelRc::new(VecModel::from(pills.into_iter().map(slint::SharedString::from).collect::<Vec<_>>())));
            ui.set_slash_hint_active(active);
            ui.invoke_set_input(text.into());
        });
    }

    /// Runs `/name arguments` if it names one of the chat's commands (None: it doesn't, send it as text).
    /// When the arguments don't fit, the text goes back into the box and the status says why.
    async fn run_slash(&mut self, text: &str, typed: String, picked: Vec<(String, String)>) -> Option<Result<(), String>> {
        let index = self.slash_index().await?;
        let (entry, rest) = slash::find(&index, text)?;
        let (guild, channel, _) = self.slash_key()?;
        let options = match slash::arguments(entry, rest) {
            Ok(options) => slash::wrap(entry, options),
            Err(e) => {
                self.picked_mentions = picked;
                self.input_text = typed.clone();
                on_ui(&self.ui, move |ui| ui.invoke_set_input(typed.into()));
                return Some(Err(slash::error_text(&e)));
            }
        };
        let session = self.ready["session_id"].as_str().unwrap_or("").to_string();
        if session.is_empty() {
            return Some(Err(i18n::t("Команда не сработала: нет связи с Discord")));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let body = slash::interaction(entry, options, guild.as_deref(), &channel, &session, &slash::nonce(now));
        slash::remember(&mut self.slash_recent, entry.key());
        prefs::write("slash_recent", json!(self.slash_recent));
        // The bot's answer comes as an ordinary message through the gateway.
        Some(self.api.post("/interactions", body).await.map(|_| ()).map_err(|e| {
            log_line(&format!("slash command: {e}"));
            i18n::tf("Команда не сработала: {}", &[&discord::refusal(&e)])
        }))
    }

    /// The server of the voice channel we're in (None in a DM call).
    fn voice_guild(&self) -> Option<String> {
        let channel = self.voice_channel.as_deref()?;
        self.voice_rooms.iter().find(|(_, rooms)| rooms.iter().any(|(id, _)| id == channel)).map(|(guild, _)| guild.clone())
    }

    /// The soundboard's lists: Discord's own once, the call's server's when not asked lately.
    async fn load_sounds(&mut self) {
        let muted = self.sounds_muted;
        on_ui(&self.ui, move |ui| {
            ui.set_sounds_muted(muted);
            ui.set_sounds_busy(true);
        });
        if self.sound_defaults.is_none() {
            match self.api.get(soundboard::DEFAULTS_PATH).await {
                Ok(list) => self.sound_defaults = Some(soundboard::parse(&list, None)),
                Err(e) => log_line(&format!("soundboard: {e}")),
            }
        }
        if let Some(guild) = self.voice_guild() {
            if self.sound_servers.get(&guild).is_none_or(|(at, _)| at.elapsed() >= GUILD_CACHE_FOR) {
                let Ok(path) = soundboard::guild_path(&guild) else { return };
                match self.api.get(&path).await {
                    Ok(list) => {
                        self.sound_servers.insert(guild.clone(), (Instant::now(), soundboard::parse(&list, Some(&guild))));
                    }
                    Err(e) => log_line(&format!("soundboard of a server: {e}")),
                }
            }
        }
    }

    /// The soundboard panel for what's typed in its search.
    fn show_sounds(&mut self) {
        let guild = self.voice_guild();
        let server = guild.as_ref().and_then(|g| self.sound_servers.get(g)).map(|(_, list)| list.clone()).unwrap_or_default();
        let defaults = self.sound_defaults.clone().unwrap_or_default();
        let sections = soundboard::sections(&server, &defaults, &self.sound_recent, &self.sound_query);
        let server_name = guild.as_ref().and_then(|g| self.guild_names.get(g)).cloned().unwrap_or_default();
        let (tiles, strip, height) = sound_panel(&sections, &server_name);
        let emojis: Vec<String> = sections.iter().flat_map(|s| &s.sounds).filter_map(|s| s.emoji_id.as_deref().map(|id| chat::emoji_picture(id, false))).collect();
        self.load_emojis(emojis);
        on_ui(&self.ui, move |ui| {
            ui.set_sounds_busy(false);
            show_sound_panel(&ui, tiles, strip, height);
        });
    }

    /// A tile clicked: the sound plays for everyone in our voice channel (it comes back to us, like to
    /// everyone, as VOICE_CHANNEL_EFFECT_SEND). Sent once.
    async fn play_sound(&mut self, id: String, guild: String) -> Result<(), String> {
        let Some(channel) = self.voice_channel.clone() else { return Ok(()) };
        let sound = soundboard::Sound { id: id.clone(), name: String::new(), volume: 1.0, emoji: String::new(), emoji_id: None, guild: Some(guild).filter(|g| !g.is_empty()) };
        let (path, body) = soundboard::play_request(&channel, &sound)?;
        self.api.post(&path, body).await.map_err(|e| i18n::tf("Звук не проиграть: {}", &[&discord::refusal(&e)]))?;
        soundboard::remember(&mut self.sound_recent, &id);
        prefs::write("sound_recent", json!(self.sound_recent));
        Ok(())
    }

    /// Someone (we too) played a sound in our voice channel: it is fetched once, decoded, and played
    /// with the call's sound, as loud as they set it.
    fn on_sound_effect(&self, data: &Value) {
        if self.sounds_muted {
            return;
        }
        if data["user_id"].as_str().is_some_and(usermenu::sound_muted) {
            return;
        }
        let Some(channel) = self.voice_channel.as_deref() else { return };
        let Some((id, volume)) = soundboard::effect(data, channel) else { return };
        let Some(url) = soundboard::file_url(&id).filter(|u| is_trusted_media_host(u)) else { return };
        let (images, kept, voice) = (self.images.clone(), self.sounds_kept.clone(), self.voice.clone());
        tokio::spawn(async move {
            let known = kept.lock().unwrap().get(&id);
            let sound = match known {
                Some(sound) => sound,
                None => {
                    let Some(bytes) = images.download(&url, soundboard::MAX_BYTES).await else {
                        log_line(&format!("soundboard: could not download sound {id}"));
                        return;
                    };
                    match tokio::task::spawn_blocking(move || soundboard::decode(&bytes)).await {
                        Ok(Ok(samples)) => {
                            let sound = Arc::new(samples);
                            kept.lock().unwrap().put(&id, sound.clone());
                            sound
                        }
                        Ok(Err(e)) => return log_line(&format!("soundboard: sound {id}: {e}")),
                        Err(e) => return log_line(&format!("soundboard: {e}")),
                    }
                }
            };
            let _ = voice.send(voice::VoiceCmd::PlaySound(Arc::new(soundboard::scaled(&sound, volume))));
        });
    }

    /// The text to send: picked names become mentions (<@id>, <#id>).
    fn with_mentions(&mut self, text: String) -> String {
        let out = mentions::resolve(&text, &self.picked_mentions);
        self.picked_mentions.clear();
        self.suggestions.clear();
        self.input_text.clear();
        on_ui(&self.ui, |ui| ui.set_suggest_open(false));
        out
    }

    /// Remembers a message's text as Discord has it.
    fn note_raw(&mut self, m: &Value) {
        if let (Some(id), Some(text)) = (m["id"].as_str(), m["content"].as_str()) {
            self.raw.insert(id.to_string(), text.to_string());
        }
    }

    /// Someone joined (`channel` is the DM) or left (None) a call in a DM or group; `state` is
    /// their voice state (for the mute and deafen marks).
    fn note_dm_call_state(&mut self, user: &str, channel: Option<&str>, state: &Value) {
        match channel {
            Some(c) => {
                let flag = |k: &str| state[k].as_bool().unwrap_or(false);
                let entry = (c.to_string(), flag("mute") || flag("self_mute"), flag("deaf") || flag("self_deaf"));
                self.dm_call_of.insert(user.to_string(), entry);
            }
            None => {
                self.dm_call_of.remove(user);
            }
        }
        self.show_dm_call();
    }

    /// A voice state with no guild: someone's place in a DM or group call. Ours carries the
    /// session id our own call needs, and is the moment a pressed call button rings the others.
    async fn on_dm_voice_state(&mut self, uid: String, data: &Value) {
        let channel = data["channel_id"].as_str();
        if uid == self.me_id {
            let _ = self.voice.send(voice::VoiceCmd::DiscordState(data.clone()));
            if let Some(dm) = self.ring_pending.take_if(|dm| Some(dm.as_str()) == channel) {
                self.ring(dm).await;
            }
        } else if let Some(ours) = self.voice_channel.as_deref() {
            let before = self.dm_call_of.get(&uid).is_some_and(|(c, _, _)| c == ours);
            let now = channel == Some(ours);
            if before != now {
                let _ = self.voice.send(voice::VoiceCmd::DiscordPeer { user_id: uid.clone(), joined: now });
            }
        }
        self.note_dm_call_state(&uid, channel, data);
    }

    /// Rings the others of a DM for our call there, once; a failure is only shown, not retried.
    async fn ring(&self, channel: String) {
        match self.api.ring(&channel).await {
            Ok(_) => log_line("dm call: rang the others"),
            Err(e) => {
                log_line(&format!("dm call: ring failed: {e}"));
                let text = format!("{}{}", i18n::t("Не удалось позвонить: "), discord::friendly(&e));
                on_ui(&self.ui, move |ui| ui.set_notice(text.into()));
            }
        }
    }

    /// Joins the call of the open DM or group in Discord's own voice (no Nimbus room there).
    /// `ring`: the call button was pressed, so the others are rung once we're in.
    fn join_dm_call(&mut self, ring: bool) -> Result<(), String> {
        let channel = self.channel.clone().filter(|c| self.channels.get(c).is_some_and(|i| i.dm)).ok_or_else(|| i18n::t("канал не выбран"))?;
        snowflake(&channel)?;
        self.voice_channel = Some(channel.clone());
        self.ring_pending = ring.then(|| channel.clone());
        self.dm_call_declined.remove(&channel);
        let name = self.channels.get(&channel).map(|c| c.name.clone()).unwrap_or_default();
        self.send_join(LastJoin { guild_id: String::new(), dm: true, channel_id: channel, channel_name: name, guild_name: String::new(), server: String::new() })
    }

    /// Asks the voice side to join a channel, and remembers it.
    fn send_join(&mut self, place: LastJoin) -> Result<(), String> {
        self.last_join = Some(place.clone());
        let dm = place.dm;
        let cmd = voice::VoiceCmd::Join {
            guild_id: place.guild_id,
            dm,
            channel_id: place.channel_id,
            channel_name: place.channel_name,
            guild_name: place.guild_name,
            server: place.server,
            identity: self.me_id.clone(),
            name: self.me_name.clone(),
        };
        on_ui(&self.ui, move |ui| {
            ui.set_voice_dm(dm);
            ui.set_voice_taken(false);
        });
        self.voice.send(cmd).map_err(|e| e.to_string())?;
        // A DM call we were in is left: its people go from the call screen.
        self.show_dm_call();
        Ok(())
    }

    /// The banner's button: back into the channel, wherever the client is looking now.
    fn rejoin_voice(&mut self) -> Result<(), String> {
        let Some(place) = self.last_join.clone() else { return Ok(()) };
        self.voice_channel = Some(place.channel_id.clone());
        self.ring_pending = None;
        self.send_join(place)
    }

    /// The line above a DM that has a call going on: who is in it, and whether we are; nothing
    /// for anything else or once declined. Also the Discord people of the DM call we are in.
    fn show_dm_call(&self) {
        let here = self.channel.clone().filter(|c| self.channels.get(c).is_some_and(|i| i.dm) && !self.dm_call_declined.contains(c));
        let joined = here.is_some() && here == self.voice_channel;
        let text = here
            .map(|channel| {
                let names: Vec<String> = self
                    .dm_call_of
                    .iter()
                    .filter(|(_, (c, _, _))| *c == channel)
                    .map(|(u, _)| if *u == self.me_id { i18n::t("ты") } else { self.users.get(u).map(display_name).unwrap_or_else(|| i18n::t("кто-то")) })
                    .collect();
                dm_call_line(&names)
            })
            .unwrap_or_default();
        let people = self.dm_call_people();
        on_ui(&self.ui, move |ui| {
            ui.set_dm_call(text.into());
            ui.set_dm_call_joined(joined);
            ui.set_call_people(ModelRc::new(VecModel::from(people.into_iter().map(DmPerson::member).collect::<Vec<_>>())));
            hide_doubles(&ui);
        });
        self.push_call_people();
    }

    /// The people of the server voice channel we are in, for when another server or the DMs
    /// are open: its row, where they are otherwise read from, isn't in the list then.
    fn push_call_people(&self) {
        let Some(place) = self.last_join.as_ref().filter(|p| !p.dm && self.voice_channel.as_deref() == Some(p.channel_id.as_str())) else { return };
        let people: Vec<(DmPerson, bool)> = self
            .discord_voice
            .get(&place.guild_id)
            .into_iter()
            .flat_map(|states| states.values())
            .filter(|vs| vs["channel_id"].as_str() == Some(place.channel_id.as_str()))
            .filter_map(|vs| {
                let uid = vs["user_id"].as_str()?;
                let name = match vs["nick"].as_str() {
                    Some(nick) => readable(nick),
                    None => self.users.get(uid).map(display_name).unwrap_or_else(|| uid.to_string()),
                };
                let flag = |k: &str| vs[k].as_bool().unwrap_or(false);
                let person = DmPerson { id: uid.to_string(), name, muted: flag("mute") || flag("self_mute"), deafened: flag("deaf") || flag("self_deaf"), avatar: voice_avatar(uid), frame: voice_frame(uid) };
                Some((person, flag("self_stream")))
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            let members: Vec<VoiceMember> = people.into_iter().map(|(person, streaming)| VoiceMember { streaming, ..person.member() }).collect();
            ui.set_call_people(ModelRc::new(VecModel::from(members)));
            hide_doubles(&ui);
        });
    }

    /// Everyone but us in the DM call we are in, as plain data for the UI thread.
    fn dm_call_people(&self) -> Vec<DmPerson> {
        let Some(ours) = self.voice_channel.as_deref() else { return Vec::new() };
        let mut people: Vec<DmPerson> = self
            .dm_call_of
            .iter()
            .filter(|(u, (c, _, _))| c == ours && **u != self.me_id)
            .map(|(u, (_, muted, deafened))| DmPerson {
                id: u.clone(),
                name: self.users.get(u).map(display_name).unwrap_or_else(|| u.clone()),
                muted: *muted,
                deafened: *deafened,
                avatar: voice_avatar(u),
                frame: voice_frame(u),
            })
            .collect();
        people.sort_by(|a, b| a.name.cmp(&b.name));
        people
    }

    /// Typing, edits, deletions and reactions in chats.
    fn on_chat_event(&mut self, kind: &str, data: Value) {
        let channel = data["channel_id"].as_str().unwrap_or("").to_string();
        let open = self.channel.as_deref() == Some(channel.as_str());
        match kind {
            "VOICE_CHANNEL_EFFECT_SEND" => self.on_sound_effect(&data),
            "GUILD_MEMBER_LIST_UPDATE" => {
                if data["guild_id"].as_str() == self.members_guild.as_deref() && self.members.apply(&data) {
                    self.show_members();
                }
            }
            "TYPING_START" => {
                let user = data["user_id"].as_str().unwrap_or("");
                if user == self.me_id || user.is_empty() {
                    return;
                }
                if data["member"]["user"].is_object() {
                    self.users.insert(user.to_string(), data["member"]["user"].clone());
                }
                let name = chat::typing_name(&data, self.users.get(user)).map(|n| readable(&n)).unwrap_or_else(|| i18n::t("Кто-то"));
                self.typing.start(&channel, user, &name, Instant::now());
                self.show_typing();
            }
            "MESSAGE_UPDATE" if open => {
                let id = data["id"].as_str().unwrap_or("").to_string();
                // A poll's final count arrives as an edit of its message.
                if let Some(p) = poll::parse(&data) {
                    self.polls.insert(id.clone(), p);
                    self.show_poll(&id);
                }
                // A bot answering a click often edits its message: new buttons and menus (or none).
                if data["components"].is_array() {
                    let rows = components::parse(&data).rows;
                    let mut target = components::target_of(&data);
                    if let Some((old, _)) = self.components.get(&id) {
                        target.guild = target.guild.or(old.guild.clone());
                        if target.application.is_empty() {
                            target.application = old.application.clone();
                        }
                    }
                    self.select_chosen.remove(&id);
                    self.components.insert(id.clone(), (target, rows.clone()));
                    self.load_emojis(rows.iter().flatten().flat_map(|c| c.emoji_image.iter().cloned().chain(c.options.iter().filter_map(|o| o.emoji_image.clone()))).collect());
                    self.show_components(&id);
                }
                // Only edits of the text; reaction-free embed refreshes come here too.
                if !data["content"].is_string() {
                    return;
                }
                self.note_raw(&data);
                let msg = to_msg(&data, &mut None, &mut Vec::new());
                let edited = data["edited_timestamp"].is_string();
                let pinned = data["pinned"].as_bool();
                let blocks = msg.blocks;
                on_ui(&self.ui, move |ui| {
                    let blocks: Vec<Block> = blocks.iter().map(block_item).collect();
                    let Some(row) = message_row(&ui, &id) else { return };
                    update_row(ui.get_messages(), row, |m| {
                        m.blocks = ModelRc::new(VecModel::from(blocks));
                        m.edited |= edited;
                        if let Some(p) = pinned {
                            m.pinned = p;
                        }
                    });
                });
            }
            "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" if open => {
                let ids: Vec<String> = match data["ids"].as_array() {
                    Some(ids) => ids.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                    None => data["id"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default(),
                };
                on_ui(&self.ui, move |ui| {
                    let model = ui.get_messages();
                    for id in ids {
                        let Some(row) = message_row(&ui, &id) else { continue };
                        let Some(gone) = model.row_data(row) else { continue };
                        update_row(model.clone(), row, |m| m.deleted = true);
                        // The next message of the same group now starts it: give it a header.
                        if !gone.grouped {
                            if let Some(next) = (row + 1..model.row_count()).find(|&i| model.row_data(i).is_some_and(|m| !m.deleted)) {
                                update_row(model.clone(), next, |m| {
                                    if m.grouped {
                                        m.grouped = false;
                                        m.avatar = gone.avatar.clone();
                                        m.decoration = gone.decoration.clone();
                                        m.has_decoration = gone.has_decoration;
                                    }
                                });
                            }
                        }
                    }
                });
            }
            k if k.starts_with("MESSAGE_REACTION") && open => {
                let id = data["message_id"].as_str().unwrap_or("").to_string();
                let list = self.reactions.entry(id.clone()).or_default();
                chat::apply_reaction(list, kind, &data, &self.me_id);
                let list = list.clone();
                self.load_emojis(list.iter().filter_map(|r| r.image.clone()).collect());
                self.show_reactions(id, list);
            }
            "INTERACTION_SUCCESS" | "INTERACTION_FAILURE" => self.on_interaction_result(kind, &data),
            k if k.starts_with("GUILD_SCHEDULED_EVENT_") => self.on_scheduled_event(k, &data),
            "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" if open => {
                let id = data["message_id"].as_str().unwrap_or("").to_string();
                if self.polls.get_mut(&id).is_some_and(|p| p.apply_vote(kind, &data, &self.me_id)) {
                    self.show_poll(&id);
                }
            }
            _ => {}
        }
    }

    fn show_reactions(&self, message: String, list: Vec<chat::Reaction>) {
        on_ui(&self.ui, move |ui| {
            let Some(row) = message_row(&ui, &message) else { return };
            update_row(ui.get_messages(), row, |m| m.reactions = reaction_items(&list));
        });
    }

    /// Starts loading custom emoji pictures not asked for yet (reactions, big emoji, poll answers); rows
    /// showing them are redrawn when they arrive, and animated ones keep moving (see `tick_emojis`).
    fn load_emojis(&mut self, urls: Vec<String>) {
        let jobs: Vec<ImageJob> = urls
            .into_iter()
            .filter(|url| self.emoji_asked.insert(url.clone()))
            .filter_map(|url| {
                let id = chat::emoji_id_in_url(&url)?;
                let animated = url.contains("animated=true");
                // An animated one that can't be played is drawn still.
                let fallbacks = if animated { vec![chat::emoji_picture(&id, false)] } else { Vec::new() };
                Some(ImageJob { message: id, url, fallbacks, w: EMOJI_PX, h: EMOJI_PX, animated, ..Default::default() })
            })
            .collect();
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(loaded) = loaded else { return };
            let id = job.message.clone();
            on_ui(&ui, move |ui| {
                let first = Image::from_rgba8(loaded.first());
                EMOJIS.with(|e| e.borrow_mut().insert(id.clone(), first.clone()));
                if let Loaded::Frames(frames) = loaded {
                    let frames: Vec<(Image, u32)> = frames.into_iter().map(|(px, delay)| (Image::from_rgba8(px), delay)).collect();
                    let next = Instant::now() + Duration::from_millis(frames[0].1 as u64);
                    EMOJI_ANIMATIONS.with(|a| a.borrow_mut().insert(id.clone(), EmojiAnimation { frames, current: 0, next }));
                }
                show_emoji_images(&ui, &HashMap::from([(id, first)]));
            });
        });
    }

    /// Remembers the polls and voice messages of messages about to be shown, for clicks on them.
    fn note_extras(&mut self, msgs: &[Msg]) {
        for m in msgs {
            if let Some(p) = &m.poll {
                self.polls.insert(m.id.clone(), p.clone());
            }
            if let Some(c) = &m.voice {
                self.clips.insert(m.id.clone(), c.clone());
            }
            if !m.components.is_empty() {
                self.components.insert(m.id.clone(), (m.target.clone(), m.components.clone()));
            }
        }
        self.load_invites(msgs);
    }

    /// A poll on screen, as it stands now.
    fn show_poll(&self, message: &str) {
        let Some(poll) = self.polls.get(message).cloned() else { return };
        let message = message.to_string();
        on_ui(&self.ui, move |ui| {
            let Some(row) = message_row(&ui, &message) else { return };
            update_row(ui.get_messages(), row, |m| {
                m.poll = poll_item(&poll);
                m.has_poll = true;
            });
        });
    }

    /// Our click on a poll's answer: the new choice is sent once (never repeated), then shown.
    async fn poll_vote(&mut self, message: String, answer: u64) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let Some(poll) = self.polls.get(&message) else { return Ok(()) };
        if poll.ended(Local::now().timestamp_millis()) || !poll.answers.iter().any(|a| a.id == answer) {
            return Ok(());
        }
        let choice = poll.choice(answer);
        self.api.poll_vote(&channel, &message, &choice).await?;
        if let Some(poll) = self.polls.get_mut(&message) {
            poll.set_mine(&choice);
        }
        self.show_poll(&message);
        Ok(())
    }

    /// Sends a bot the interaction of a click on its button or a choice in its menu (once, never repeated).
    /// The button waits (dots) until the bot answers or three seconds pass; a refusal says so under the box.
    async fn send_interaction(&mut self, message: &str, custom: &str, values: Option<Vec<String>>) -> Result<(), String> {
        let Some((target, rows)) = self.components.get(message) else { return Ok(()) };
        let Some(component) = components::find(rows, custom) else { return Ok(()) };
        if component.disabled {
            return Ok(());
        }
        let session = self.ready["session_id"].as_str().unwrap_or("").to_string();
        if session.is_empty() {
            flash(&self.ui, i18n::t("Взаимодействие не удалось"));
            return Ok(());
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let nonce = slash::nonce(now);
        let body = components::interaction(target, component, values.as_deref(), &session, &nonce)?;
        self.interactions.insert(nonce.clone(), (message.to_string(), custom.to_string()));
        let (shown_message, shown_custom) = (message.to_string(), custom.to_string());
        on_ui(&self.ui, move |ui| set_component_pending(&ui, &shown_message, &shown_custom, true));
        // The bot answers through the gateway; the wait ends by itself after a while.
        let (ui, waited_message, waited_custom) = (self.ui.clone(), message.to_string(), custom.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            on_ui(&ui, move |ui| set_component_pending(&ui, &waited_message, &waited_custom, false));
        });
        if let Err(e) = self.api.post("/interactions", body).await {
            log_line(&format!("component interaction: {e}"));
            self.interactions.remove(&nonce);
            let (m, c) = (message.to_string(), custom.to_string());
            on_ui(&self.ui, move |ui| set_component_pending(&ui, &m, &c, false));
            flash(&self.ui, i18n::t("Взаимодействие не удалось"));
        }
        Ok(())
    }

    async fn component_click(&mut self, message: String, custom: String) -> Result<(), String> {
        let is_button = self.components.get(&message).and_then(|(_, rows)| components::find(rows, &custom)).is_some_and(|c| c.kind == components::BUTTON && c.style != components::LINK);
        if !is_button {
            return Ok(());
        }
        self.send_interaction(&message, &custom, None).await
    }

    /// A select menu opened: its options, with what is chosen now.
    fn select_open(&self, message: &str, custom: &str) {
        let Some(c) = self.components.get(message).and_then(|(_, rows)| components::find(rows, custom)).cloned() else { return };
        let chosen = self.select_chosen.get(message).and_then(|m| m.get(custom)).cloned().unwrap_or_else(|| c.options.iter().filter(|o| o.default).map(|o| o.value.clone()).collect());
        let tall = c.options.iter().any(|o| !o.description.is_empty());
        let (multi, max) = (c.max > 1, c.max.min(25) as i32);
        let (message, custom) = (message.to_string(), custom.to_string());
        on_ui(&self.ui, move |ui| {
            if ui.get_select_message().as_str() != message || ui.get_select_custom().as_str() != custom {
                return;
            }
            ui.set_select_multi(multi);
            ui.set_select_max(max);
            ui.set_select_tall(tall);
            ui.set_select_options(ModelRc::new(VecModel::from(select_option_items(&c, &chosen))));
        });
    }

    /// The menu closed with these values picked: sent when they make a new, valid choice.
    async fn component_select(&mut self, message: String, custom: String, picked: Vec<String>) -> Result<(), String> {
        let Some(c) = self.components.get(&message).and_then(|(_, rows)| components::find(rows, &custom)).cloned() else { return Ok(()) };
        let before = self.select_chosen.get(&message).and_then(|m| m.get(&custom)).cloned().unwrap_or_else(|| c.options.iter().filter(|o| o.default).map(|o| o.value.clone()).collect());
        let Some(values) = c.choice(&picked) else { return Ok(()) };
        if values == before {
            return Ok(());
        }
        self.select_chosen.entry(message.clone()).or_default().insert(custom.clone(), values.clone());
        self.show_components(&message);
        self.send_interaction(&message, &custom, Some(values)).await
    }

    /// A message's buttons and menus on screen as they stand (after an edit, or a choice in a menu).
    fn show_components(&self, message: &str) {
        let rows = self.components.get(message).map(|(_, rows)| rows.clone()).unwrap_or_default();
        let chosen = self.select_chosen.get(message).cloned().unwrap_or_default();
        let message = message.to_string();
        on_ui(&self.ui, move |ui| {
            let Some(row) = message_row(&ui, &message) else { return };
            update_row(ui.get_messages(), row, |m| m.components = component_rows(&rows, &chosen));
        });
    }

    /// INTERACTION_SUCCESS / INTERACTION_FAILURE: the bot took the click, or Discord says it failed.
    fn on_interaction_result(&mut self, kind: &str, data: &Value) {
        let Some((message, custom)) = data["nonce"].as_str().and_then(|n| self.interactions.remove(n)) else { return };
        on_ui(&self.ui, move |ui| set_component_pending(&ui, &message, &custom, false));
        if kind == "INTERACTION_FAILURE" {
            flash(&self.ui, i18n::t("Взаимодействие не удалось"));
        }
    }

    /// A server's scheduled events: kept for five minutes (the gateway's changes are applied to the kept list),
    /// then asked again. Shown when it is the open server.
    async fn refresh_events(&mut self, guild: &str, fresh: bool) {
        let kept = self.events.get(guild).is_some_and(|(at, _)| at.elapsed() < EVENTS_CACHE_FOR);
        if fresh || !kept {
            match events::list_path(guild) {
                Ok(path) => match self.api.get(&path).await {
                    Ok(v) => {
                        self.events.insert(guild.to_string(), (Instant::now(), events::parse_list(&v)));
                    }
                    Err(e) => log_line(&format!("scheduled events of {guild}: {e}")),
                },
                Err(e) => log_line(&format!("scheduled events: {e}")),
            }
        }
        self.show_events();
    }

    /// The events row and window of the open server, as kept now.
    fn show_events(&self) {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return };
        let list = self.events.get(&guild).map(|(_, l)| l.clone()).unwrap_or_default();
        let today = Local::now().date_naive();
        let rows: Vec<(events::ScheduledEvent, String, String, bool, Option<Pixels>)> = list
            .into_iter()
            .map(|e| {
                let when = DateTime::from_timestamp_millis(e.start_ms).map(|t| events::when(t.with_timezone(&Local).naive_local(), today)).unwrap_or_default();
                let place = match (e.entity, &e.channel) {
                    (events::STAGE | events::VOICE, Some(c)) => self.channels.get(c).map(|c| c.name.clone()).unwrap_or_default(),
                    _ => readable(&e.location),
                };
                let interested = self.events_interested.contains(&e.id);
                let avatar = self.event_avatars.get(&e.creator_id).cloned().flatten();
                (e, when, place, interested, avatar)
            })
            .collect();
        let me = self.me_id.clone();
        on_ui(&self.ui, move |ui| {
            if ui.get_selected_guild().as_str() != guild {
                return;
            }
            let items: Vec<EventItem> = rows.into_iter().map(|(e, when, place, interested, avatar)| event_item(&e, when, place, interested, avatar, &me)).collect();
            ui.set_events_title(events::count_title(items.len()).into());
            ui.set_events(ModelRc::new(VecModel::from(items)));
            ui.set_events_loading(false);
        });
    }

    /// The events window opened: the list asked again if it's old, our right to create events, the creators' avatars.
    async fn events_open(&mut self) -> Result<(), String> {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return Ok(()) };
        if !self.events.contains_key(&guild) {
            on_ui(&self.ui, |ui| ui.set_events_loading(true));
        }
        self.refresh_events(&guild, false).await;
        let can = self.guild_perms(&guild).await.is_some_and(|me| me.can_create_events());
        on_ui(&self.ui, move |ui| ui.set_events_can_create(can));
        let wanted: Vec<(String, String)> = self
            .events
            .get(&guild)
            .map(|(_, l)| l.iter().filter_map(|e| Some((e.creator_id.clone(), e.creator_avatar.clone()?))).collect())
            .unwrap_or_default();
        let mut loaded = false;
        for (user, url) in wanted {
            if self.event_avatars.contains_key(&user) {
                continue;
            }
            let picture = self.images.get(url, AVATAR_PX, AVATAR_PX).await;
            self.event_avatars.insert(user, picture);
            loaded = true;
        }
        if loaded {
            self.show_events();
        }
        on_ui(&self.ui, |ui| ui.set_events_loading(false));
        Ok(())
    }

    /// "Интересно" clicked: sent once; shown at once when Discord took it.
    async fn event_interest(&mut self, event: String, on: bool) -> Result<(), String> {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return Ok(()) };
        let path = events::interest_path(&guild, &event)?;
        let sent = if on { self.api.put(&path).await } else { self.api.delete(&path).await };
        if let Err(e) = sent {
            flash(&self.ui, discord::refusal(&e));
            return Ok(());
        }
        if let Some((_, list)) = self.events.get_mut(&guild) {
            events::set_interested(list, &mut self.events_interested, &event, on);
        }
        self.show_events();
        Ok(())
    }

    /// "Отменить мероприятие" on one of ours: deleted once, gone from the list when Discord took it.
    async fn event_delete(&mut self, event: String) -> Result<(), String> {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return Ok(()) };
        if let Err(e) = self.api.delete(&events::event_path(&guild, &event)?).await {
            flash(&self.ui, discord::refusal(&e));
            return Ok(());
        }
        if let Some((_, list)) = self.events.get_mut(&guild) {
            list.retain(|e| e.id != event);
        }
        self.show_events();
        Ok(())
    }

    /// "Создать событие": the server's stage and voice channels, and the start an hour from now.
    fn event_create_open(&self) {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return };
        let list = self.guild_cache.get(&guild).map(|(_, l, _)| l.as_array().cloned().unwrap_or_default()).unwrap_or_default();
        let mut rooms: Vec<&Value> = list.iter().filter(|c| matches!(c["type"].as_u64(), Some(2 | 13))).collect();
        rooms.sort_by_key(|c| c["position"].as_i64().unwrap_or(0));
        let channels: Vec<(String, String, bool)> = rooms
            .iter()
            .filter_map(|c| Some((c["id"].as_str()?.to_string(), tidy_name(&readable(c["name"].as_str().unwrap_or(""))), c["type"].as_u64() == Some(13))))
            .collect();
        let start = events::default_start(Local::now().naive_local());
        let (date, time) = events::date_field(start);
        let (end_date, end_time) = events::date_field(start + chrono::Duration::hours(1));
        on_ui(&self.ui, move |ui| {
            let items: Vec<EventChannelItem> = channels.into_iter().map(|(id, name, stage)| EventChannelItem { id: id.into(), name: name.into(), stage }).collect();
            ui.set_event_channels(ModelRc::new(VecModel::from(items)));
            ui.set_event_kind(2);
            ui.set_event_channel("".into());
            ui.set_event_location("".into());
            ui.set_event_name("".into());
            ui.set_event_desc("".into());
            ui.set_event_date(date.into());
            ui.set_event_time(time.into());
            ui.set_event_end_date(end_date.into());
            ui.set_event_end_time(end_time.into());
            ui.set_event_busy(false);
        });
    }

    /// The last step's "Создать событие": sent once; the window closes and the list shows it, or the reason stays.
    async fn event_create(&mut self, draft: events::Draft) -> Result<(), String> {
        let Some(guild) = self.guild.clone().filter(|g| g != HOME) else { return Ok(()) };
        let result = match events::create_body(&draft, &Local::now()) {
            Ok(body) => match snowflake(&guild) {
                Ok(g) => self.api.post(&format!("/guilds/{g}/scheduled-events"), body).await.map_err(|e| discord::refusal(&e)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match result {
            Ok(created) => {
                if let Some((_, list)) = self.events.get_mut(&guild) {
                    events::apply(list, "GUILD_SCHEDULED_EVENT_CREATE", &created, &self.me_id, &mut self.events_interested);
                }
                on_ui(&self.ui, |ui| {
                    ui.set_event_busy(false);
                    ui.set_event_step(0);
                });
                self.show_events();
            }
            Err(e) => on_ui(&self.ui, move |ui| {
                ui.set_event_busy(false);
                ui.set_event_error(e.into());
            }),
        }
        Ok(())
    }

    /// GUILD_SCHEDULED_EVENT_*: applied to the kept list of that server.
    fn on_scheduled_event(&mut self, kind: &str, data: &Value) {
        let Some(guild) = data["guild_id"].as_str() else { return };
        let Some((_, list)) = self.events.get_mut(guild) else { return };
        if events::apply(list, kind, data, &self.me_id, &mut self.events_interested) && self.guild.as_deref() == Some(guild) {
            self.show_events();
        }
    }

    /// The play button of a voice message: pauses the one playing, or plays this one (fetched and
    /// decoded on the first click) from where it was paused.
    async fn voice_play(&mut self, message: String) -> Result<(), String> {
        if let Some((id, playback)) = self.playing.take() {
            self.clip_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let total = self.clip_sound.get(&id).map_or(0, |s| s.len());
            let was_playing = !playback.finished();
            let at = playback.position();
            drop(playback);
            // Played to the end: the next click starts it again.
            let at = if at >= total { 0 } else { at };
            self.clip_paused.insert(id.clone(), at);
            show_clip_at(&self.ui, &id, at, total, false);
            if id == message && was_playing {
                return Ok(());
            }
        }
        let Some(clip) = self.clips.get(&message).cloned() else { return Ok(()) };
        let sound = match self.clip_sound.get(&message).cloned() {
            Some(sound) => sound,
            None => {
                let sound = Arc::new(self.fetch_clip(&clip.url).await?);
                // Only a few are kept: each is a few MB of sound.
                if self.clip_sound.len() >= 4 {
                    self.clip_sound.clear();
                }
                self.clip_sound.insert(message.clone(), sound.clone());
                sound
            }
        };
        if sound.is_empty() {
            return Ok(());
        }
        let from = self.clip_paused.remove(&message).filter(|&at| at < sound.len()).unwrap_or(0);
        let playback = audio::Playback::start(sound.clone(), from, voice::saved_speaker());
        self.watch_clip(message.clone(), &playback, sound.len());
        show_clip_at(&self.ui, &message, from, sound.len(), true);
        self.playing = Some((message, playback));
        Ok(())
    }

    /// A voice message's sound: the file from Discord's servers (size-limited), decoded.
    async fn fetch_clip(&self, url: &str) -> Result<Vec<i16>, String> {
        if !is_trusted_media_host(url) {
            return Err(i18n::t("Голосовое сообщение лежит не на сервере Discord"));
        }
        let bytes = self.images.download(url, voicemsg::MAX_BYTES).await.ok_or_else(|| i18n::t("Не удалось скачать голосовое сообщение"))?;
        let decoded = tokio::task::spawn_blocking(move || voicemsg::decode(&bytes)).await.map_err(|e| e.to_string())?;
        decoded.map_err(|e| {
            log_line(&format!("voice message: {e}"));
            i18n::t("Не удалось воспроизвести голосовое сообщение")
        })
    }

    /// Moves the played part of the player row along while the sound plays.
    fn watch_clip(&self, message: String, playback: &audio::Playback, total: usize) {
        use std::sync::atomic::Ordering;
        let (position, done) = playback.watch();
        let generation = self.clip_generation.clone();
        let mine = generation.load(Ordering::Relaxed);
        let ui = self.ui.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if generation.load(Ordering::Relaxed) != mine {
                    return;
                }
                let finished = done.load(Ordering::Relaxed);
                let at = position.load(Ordering::Relaxed) as usize;
                // At the end the row goes back to its start, as in Discord.
                let at = if finished && at >= total { 0 } else { at };
                show_clip_at(&ui, &message, at, total, !finished);
                if finished {
                    return;
                }
            }
        });
    }

    /// Stops the voice message playing and forgets the sounds (another channel is opened).
    fn stop_clip(&mut self) {
        self.clip_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.playing = None;
        self.clip_sound.clear();
        self.clip_paused.clear();
    }

    /// The typing line of the open channel, when it changed.
    fn show_typing(&mut self) {
        let line = self.channel.as_deref().map(|c| self.typing.line(c, Instant::now())).unwrap_or_default();
        if line != self.typing_shown {
            self.typing_shown = line.clone();
            on_ui(&self.ui, move |ui| ui.set_typing_line(line.into()));
        }
    }

    /// Sends one change of the account's privacy settings; the answer (or, after an error, what Discord
    /// really has) is what the settings page then shows.
    async fn change_privacy(&mut self, body: Value) -> Result<(), String> {
        let presence_changed = body.get("status").is_some() || body.get("custom_status").is_some();
        match self.api.patch("/users/@me/settings", body).await {
            Ok(settings) => {
                push_privacy(&self.ui, &settings);
                // The setting is kept on the account; what others see comes from this session's presence.
                if presence_changed {
                    if let Some(presence) = discord::PRESENCE.lock().unwrap().clone() {
                        let _ = self.gateway.send(json!({ "op": 3, "d": presence }));
                    }
                }
                Ok(())
            }
            Err(e) => {
                if let Ok(settings) = self.api.get("/users/@me/settings").await {
                    push_privacy(&self.ui, &settings);
                }
                Err(e)
            }
        }
    }

    /// Sends a change of our profile; when it is the account itself (`/users/@me`), what Discord answers
    /// with is shown, so the new name, avatar or banner appear without a restart.
    async fn change_profile(&mut self, path: &str, body: Value) -> Result<(), String> {
        let answer = self.api.patch(path, body).await?;
        if path == "/users/@me" {
            show_profile(&self.ui, &self.images, &answer);
        }
        notice(&self.ui, i18n::t("Профиль сохранён"));
        Ok(())
    }

    /// Asks for a picture and sends it as our avatar or banner (or takes the banner off). Discord's own
    /// refusal (a banner needs Nitro, the file is too big for it) comes back as the error.
    async fn profile_picture(&mut self, kind: &str) -> Result<(), String> {
        let mut body = serde_json::Map::new();
        match kind {
            "banner-clear" => {
                body.insert("banner".into(), Value::Null);
            }
            "avatar" | "banner" => {
                let picked = tokio::task::spawn_blocking(|| {
                    rfd::FileDialog::new().set_title(i18n::t("Выбери картинку")).add_filter(i18n::t("Картинки"), &["png", "jpg", "jpeg", "gif", "webp"]).pick_file()
                })
                .await
                .map_err(|e| e.to_string())?;
                let Some(path) = picked else { return Ok(()) };
                body.insert(kind.to_string(), Value::String(picture_data_uri(&path)?));
            }
            _ => return Ok(()),
        }
        self.change_profile("/users/@me", Value::Object(body)).await
    }

    /// Tells the others we're typing, at most every 8 seconds.
    async fn send_typing(&mut self) {
        let Some(channel) = self.channel.clone() else { return };
        let fresh = self.typing_sent.as_ref().is_some_and(|(c, at)| *c == channel && at.elapsed() < chat::TYPING_EVERY);
        if fresh {
            return;
        }
        self.typing_sent = Some((channel.clone(), Instant::now()));
        // Not worth a message if it fails: the text still goes when sent.
        let _ = self.api.typing(&channel).await;
    }

    /// Our click on a reaction: shown at once, then sent; put back if Discord refuses.
    async fn react(&mut self, message: String, key: String) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let list = self.reactions.entry(message.clone()).or_default();
        let before = list.clone();
        let Some(add) = chat::toggle(list, &key) else { return Ok(()) };
        let shown = list.clone();
        self.show_reactions(message.clone(), shown);
        if let Err(e) = self.api.react(&channel, &message, &chat::reaction_path(&key), add).await {
            self.reactions.insert(message.clone(), before.clone());
            self.show_reactions(message, before);
            return Err(e);
        }
        Ok(())
    }

    /// Joining a voice channel: its server's address comes from the channel topic, so an
    /// address not seen for this server is asked about first (see `trust`).
    fn join_voice(&mut self, id: String) -> Result<(), String> {
        let guild = self.guild.clone().unwrap_or_default();
        let addr = trust::parse(&self.guild_server)?;
        trust::check_transport(&addr)?;
        match trust::decide(&guild, &addr, &prefs::read()["servers"]) {
            trust::Decision::Connect => self.start_join(id),
            trust::Decision::Refuse => Err(i18n::tf(
                "Ты отказался подключаться к голосовому серверу этого сообщества ({}). Передумать можно: Настройки → Система → «Забыть решения».",
                &[&addr.normal()],
            )),
            trust::Decision::Ask => {
                let address = addr.normal();
                let channel = id.clone();
                on_ui(&self.ui, move |ui| {
                    ui.set_confirm_user(channel.into());
                    ui.set_confirm_name(address.into());
                    ui.set_confirm_action("voice-server".into());
                });
                Ok(())
            }
        }
    }

    /// The answer to that question: remembered for this server and address; yes joins.
    fn answer_server(&mut self, channel: String, allowed: bool) -> Result<(), String> {
        let guild = self.guild.clone().unwrap_or_default();
        let addr = trust::parse(&self.guild_server)?;
        let answers = trust::with_answer(&prefs::read()["servers"], &guild, &addr, allowed);
        prefs::write("servers", answers);
        if allowed {
            self.start_join(channel)
        } else {
            Ok(())
        }
    }

    fn start_join(&mut self, id: String) -> Result<(), String> {
        self.voice_channel = Some(id.clone());
        self.ring_pending = None;
        let name = self.channels.get(&id).map(|c| c.name.clone()).unwrap_or_default();
        let guild_name = self.guild.as_ref().and_then(|g| self.guild_names.get(g)).cloned().unwrap_or_default();
        let guild_id = self.guild.clone().filter(|g| g != "@me").unwrap_or_default();
        self.send_join(LastJoin { guild_id, dm: false, channel_id: id, channel_name: name, guild_name, server: self.guild_server.clone() })
    }

    async fn reply(&mut self, id: String, text: String) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let text = self.with_mentions(text);
        let body = json!({ "content": text, "message_reference": { "message_id": id, "channel_id": channel } });
        self.api.post(&format!("/channels/{}/messages", snowflake(&channel)?), body).await?;
        Ok(())
    }

    async fn send(&mut self, text: String) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        let (typed, picked) = (text.clone(), self.picked_mentions.clone());
        let text = self.with_mentions(text);
        self.slash_hide();
        // Pasted files go with the text, in one message; they stay in the box if Discord refuses.
        if !self.pending_files.is_empty() {
            let files: Vec<(String, Vec<u8>)> = self.pending_files.iter().map(|f| (f.name.clone(), f.bytes.clone())).collect();
            on_ui(&self.ui, |ui| ui.set_status(i18n::t("Отправка файлов…").into()));
            let sent = self.api.upload_files(&channel, &files, &text).await;
            let ok = sent.is_ok();
            on_ui(&self.ui, move |ui| ui.set_status(if ok { i18n::t("В сети").into() } else { "".into() }));
            if let Err(e) = sent {
                log_line(&format!("files not sent: {e}"));
                flash(&self.ui, discord::refusal(&e));
                self.input_text = typed.clone();
                on_ui(&self.ui, move |ui| ui.invoke_set_input(typed.into()));
                return Ok(());
            }
            self.pending_files.clear();
            self.show_pending();
            return Ok(());
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        // `/name …` of a known command runs it instead of sending the text.
        if text.starts_with('/') {
            if let Some(ran) = self.run_slash(&text, typed, picked).await {
                return ran;
            }
        }
        // The message comes back through the gateway, so there's no local echo.
        self.api.post(&format!("/channels/{}/messages", snowflake(&channel)?), json!({ "content": text })).await?;
        Ok(())
    }

    /// The GIF panel for what's typed (trending when nothing is). The UI asks once per pause in typing.
    async fn gif_query(&mut self, query: String) -> Result<(), String> {
        let found = self.api.get(&gifs::path(&query, gifs::locale(i18n::english()))).await;
        self.gifs = found.as_ref().map(gifs::parse).unwrap_or_default();
        self.gifs_shown = 0;
        on_ui(&self.ui, |ui| {
            GIF_TILES.with(|t| t.borrow_mut().clear());
            ui.set_gif_items(ModelRc::new(VecModel::<GifItem>::default()));
            ui.set_gif_busy(false);
        });
        self.show_more_gifs();
        found.map(|_| ())
    }

    /// The next few tiles of the GIF panel (it was opened, or scrolled to its end), their pictures loading in.
    fn show_more_gifs(&mut self) {
        let page: Vec<gifs::Gif> = self.gifs.iter().skip(self.gifs_shown).take(gifs::PAGE).cloned().collect();
        if page.is_empty() {
            return;
        }
        self.gifs_shown += page.len();
        let jobs: Vec<ImageJob> = page
            .iter()
            .filter_map(|g| {
                let url = g.preview.clone().or_else(|| g.still.clone())?;
                let fallbacks = g.still.iter().filter(|s| **s != url).cloned().collect();
                Some(ImageJob { message: gif_key(g), url, fallbacks, w: gifs::TILE_W, h: gifs::TILE_H, animated: g.preview.is_some(), max_frames: gifs::MAX_FRAMES, ..Default::default() })
            })
            .collect();
        on_ui(&self.ui, move |ui| {
            let model = ui.get_gif_items();
            if let Some(list) = model.as_any().downcast_ref::<VecModel<GifItem>>() {
                for g in &page {
                    list.push(GifItem { key: gif_key(g).into(), url: g.url.clone().into(), label: g.category.clone().into(), category: !g.category.is_empty(), image: Image::default(), has_image: false });
                }
            }
        });
        let ui = self.ui.clone();
        load_images(self.images.clone(), jobs, move |job, loaded| {
            let Some(loaded) = loaded else { return };
            let key = job.message.clone();
            let first = loaded.first();
            let frames: Option<Vec<(Pixels, u32)>> = match loaded {
                Loaded::Frames(frames) => Some(frames),
                Loaded::Still(_) => None,
            };
            on_ui(&ui, move |ui| {
                let model = ui.get_gif_items();
                let Some(row) = (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|g| g.key.as_str() == key)) else { return };
                update_row(model, row, |g| {
                    g.image = Image::from_rgba8(first);
                    g.has_image = true;
                });
                if let Some(frames) = frames {
                    let frames: Vec<(Image, u32)> = frames.into_iter().map(|(px, delay)| (Image::from_rgba8(px), delay)).collect();
                    let next = Instant::now() + Duration::from_millis(frames[0].1 as u64);
                    GIF_TILES.with(|t| t.borrow_mut().push(TileAnimation { key, frames, current: 0, next }));
                }
            });
        });
    }

    /// A GIF picked in the panel goes to the open chat as its address, the way Discord sends one. Only
    /// what the panel offered is sent, once per click.
    async fn send_gif(&mut self, url: String) -> Result<(), String> {
        let channel = self.channel.clone().ok_or_else(|| i18n::t("канал не выбран"))?;
        if !self.gifs.iter().any(|g| g.category.is_empty() && g.url == url) {
            return Ok(());
        }
        self.api.post(&format!("/channels/{}/messages", snowflake(&channel)?), json!({ "content": url })).await?;
        Ok(())
    }

    /// Message menu "Переслать": remembers the message and lists where it can go.
    fn forward_open(&mut self, message: String) {
        let Some(channel) = self.channel.clone() else { return };
        let guild = self.guild.clone().filter(|g| g != HOME);
        self.forward_targets = self.forward_places(guild.as_deref());
        self.forward_from = Some((message, channel, guild));
        self.show_forward_targets("");
    }

    /// Where a message can be forwarded: the open server's text channels we see, then the DMs.
    fn forward_places(&self, guild: Option<&str>) -> Vec<forward::Target> {
        let mut out = Vec::new();
        if let Some((gid, (_, list, me))) = guild.and_then(|g| self.guild_cache.get_key_value(g)) {
            let server = self.guild_names.get(gid).cloned().unwrap_or_default();
            let text: Vec<Value> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| matches!(c["type"].as_u64(), Some(0 | 5 | 4)) && me.as_ref().is_none_or(|me| me.can_view(c)))
                .cloned()
                .collect();
            for row in channel_rows(&text).into_iter().filter(|r| r.kind == 0) {
                out.push(forward::Target { id: row.id, name: format!("#{}", row.name), detail: server.clone(), dm: false });
            }
        }
        let private = self.ready["private_channels"].as_array().cloned().unwrap_or_default();
        let look = home::Look { name: &display_name, avatar: &avatar_url, cdn: CDN };
        for dm in home::dm_rows(&private, &self.presences, &self.me_name, &look) {
            out.push(forward::Target { id: dm.id, name: readable(&dm.name), detail: i18n::t("Личные сообщения"), dm: true });
        }
        out
    }

    /// The picker's list for what's typed in its search field.
    fn show_forward_targets(&self, query: &str) {
        let shown = forward::filter(&self.forward_targets, query);
        on_ui(&self.ui, move |ui| {
            let items: Vec<ForwardTarget> = shown
                .into_iter()
                .map(|t| ForwardTarget { id: t.id.into(), name: t.name.into(), detail: t.detail.into(), dm: t.dm })
                .collect();
            ui.set_forward_targets(ModelRc::new(VecModel::from(items)));
        });
    }

    /// The place picked: one request, then the picker is done (a second click sends nothing).
    async fn forward_to(&mut self, dest: String) -> Result<(), String> {
        if !self.forward_targets.iter().any(|t| t.id == dest) {
            return Ok(());
        }
        let Some((message, channel, guild)) = self.forward_from.take() else { return Ok(()) };
        self.api.forward(&dest, &message, &channel, guild.as_deref()).await?;
        Ok(())
    }

    /// Right-click on a server or one of its channels: what it is set to tell about, in its menu.
    fn notify_menu(&mut self, guild: String, channel: String) {
        let settings = &self.unread.settings;
        let (level, muted, inherits, name) = if channel.is_empty() {
            let place = settings.guild(&guild);
            (place.level.unwrap_or_default(), place.muted, false, self.guild_names.get(&guild).cloned().unwrap_or_default())
        } else {
            let place = settings.channel(&channel);
            let name = self.channels.get(&channel).map(|c| format!("#{}", c.name)).unwrap_or_default();
            (settings.level(&channel, Some(&guild)), place.muted, place.level.is_none(), name)
        };
        on_ui(&self.ui, move |ui| {
            ui.set_notify_menu(NotifyMenuInfo { guild: guild.into(), channel: channel.into(), name: name.into(), level: level.code() as i32, inherits, muted });
            ui.invoke_show_notify_menu();
        });
    }

    /// A choice in that menu: one request; Discord's answer (the server's whole entry) is taken in at once.
    async fn notify_change(&mut self, guild: String, channel: String, what: &str) -> Result<(), String> {
        let Some(change) = notify::Change::parse(what) else { return Ok(()) };
        let channel = Some(channel).filter(|c| !c.is_empty());
        let entry = self.api.guild_settings(&guild, channel.as_deref(), change).await?;
        // The DM side's entry names no server.
        if entry["guild_id"].as_str() == Some(guild.as_str()) || (guild == HOME && entry["guild_id"].is_null()) {
            self.unread.settings.apply(&entry, chrono::Utc::now());
        }
        self.refresh_badges();
        self.refresh_home();
        Ok(())
    }

    /// The servers of invite links in messages about to be shown: each code is asked about once a session,
    /// in the background; the cards fill in when the answer comes.
    fn load_invites(&self, msgs: &[Msg]) {
        let mut fresh = Vec::new();
        {
            let mut seen = INVITES.lock().unwrap();
            for code in msgs.iter().flat_map(|m| m.invites.iter()) {
                if !seen.contains_key(code) {
                    seen.insert(code.clone(), SeenInvite::default());
                    fresh.push(code.clone());
                }
            }
        }
        for code in fresh {
            let (api, images, ui) = (self.api.clone(), self.images.clone(), self.ui.clone());
            let ours: HashSet<String> = self.guild_names.keys().cloned().collect();
            tokio::spawn(async move {
                fetch_invite(&api, &images, &code, &ours).await;
                on_ui(&ui, move |ui| show_invite(&ui, &code));
            });
        }
    }

    /// The join window's "Найти": the code read from what was typed, and its preview (asked once per code).
    async fn invite_lookup(&mut self, typed: &str) {
        let Some(code) = invite::code_of(typed) else {
            let text = i18n::t("Это не похоже на приглашение Discord");
            on_ui(&self.ui, move |ui| {
                ui.set_join_error(text.into());
                ui.set_join_state(0);
            });
            return;
        };
        let asked = INVITES.lock().unwrap().get(&code).map(|s| s.done);
        match asked {
            None => {
                INVITES.lock().unwrap().insert(code.clone(), SeenInvite::default());
                let ours: HashSet<String> = self.guild_names.keys().cloned().collect();
                fetch_invite(&self.api, &self.images, &code, &ours).await;
            }
            // A card in the chat is asking about it right now: its answer is waited for, not asked again.
            Some(false) => {
                for _ in 0..150 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if INVITES.lock().unwrap().get(&code).is_some_and(|s| s.done) {
                        break;
                    }
                }
            }
            Some(true) => {}
        }
        let refused = INVITES.lock().unwrap().get(&code).and_then(|s| s.preview.is_none().then(|| i18n::t("Приглашение недействительно или истекло")));
        on_ui(&self.ui, move |ui| {
            show_invite(&ui, &code);
            ui.set_join_state(if refused.is_some() { 5 } else { 2 });
            ui.set_join_error(refused.unwrap_or_default().into());
            ui.set_join_preview(invite_item(&code));
        });
    }

    /// The join click: one request for the code whose preview was shown; Discord's answer is shown as it is.
    async fn invite_join(&mut self, code: &str) {
        let preview = INVITES.lock().unwrap().get(code).and_then(|s| s.preview.clone());
        let Some(preview) = preview else { return };
        let result = self.api.join_invite(&preview).await;
        let (state, error) = match result {
            Ok(_) => {
                if let Some(seen) = INVITES.lock().unwrap().get_mut(code) {
                    seen.joined = true;
                }
                self.add_joined_guild(&preview);
                (4, String::new())
            }
            // Discord wants a captcha: not solved here, never asked again from here.
            Err(e) if invite::needs_captcha(&e) => (6, i18n::t("Discord просит пройти проверку. Войдите по приглашению в официальном клиенте")),
            Err(e) => (5, discord::refusal(&e)),
        };
        let code = code.to_string();
        on_ui(&self.ui, move |ui| {
            show_invite(&ui, &code);
            ui.set_join_state(state);
            ui.set_join_error(error.into());
        });
    }

    /// A server just joined here goes into the server list right away.
    fn add_joined_guild(&mut self, p: &invite::Preview) {
        if self.guild_names.contains_key(&p.guild_id) {
            return;
        }
        self.guild_names.insert(p.guild_id.clone(), p.name.clone());
        let item = Guild { id: p.guild_id.clone(), name: p.name.clone(), ..Default::default() };
        let icon = INVITES.lock().unwrap().get(&p.code).and_then(|s| s.icon.clone());
        on_ui(&self.ui, move |ui| {
            let model = ui.get_guilds();
            if let Some(list) = model.as_any().downcast_ref::<VecModel<GuildItem>>() {
                let mut g = item.item();
                if let Some(px) = icon {
                    g.icon = Image::from_rgba8(px);
                    g.has_icon = true;
                }
                list.push(g);
            }
            sync_rail(&ui);
        });
    }

    /// A channel menu's "Пригласить": one request, then the link (or Discord's refusal) in the window.
    async fn create_invite(&mut self, channel: &str) {
        let result = self.api.create_invite(channel).await;
        let made = result.and_then(|v| v["code"].as_str().filter(|c| invite::valid_code(c)).map(invite::link).ok_or_else(|| "Discord did not give a code".to_string()));
        on_ui(&self.ui, move |ui| match made {
            Ok(link) => {
                ui.set_made_invite_link(link.into());
                ui.set_made_invite_state(2);
            }
            Err(e) => {
                ui.set_made_invite_error(discord::refusal(&e).into());
                ui.set_made_invite_state(3);
            }
        });
    }

    /// "Создать ветку" on a message of the open channel: one request, then the new thread opens.
    async fn create_thread(&mut self, message: &str, name: &str) {
        let Some(channel) = self.channel.clone() else { return };
        if forum::valid_name(name).is_none() {
            return self.thread_refused(i18n::t("Название должно быть от 1 до 100 символов"));
        }
        match self.api.create_thread(&channel, message, name).await {
            Ok(thread) => self.open_created(&thread, Some(&channel)).await,
            Err(e) => self.thread_refused(discord::refusal(&e)),
        }
    }

    /// "Новая публикация" in the open forum: one request, then the new post opens.
    async fn create_post(&mut self, forum_id: &str, title: &str, text: &str) {
        let is_forum = self.channel.as_deref() == Some(forum_id) && self.channels.get(forum_id).is_some_and(|c| c.forum);
        if !is_forum {
            return self.thread_refused(String::new());
        }
        if forum::valid_name(title).is_none() {
            return self.thread_refused(i18n::t("Заголовок должен быть от 1 до 100 символов"));
        }
        if text.trim().is_empty() || text.trim().chars().count() > forum::MAX_TEXT {
            return self.thread_refused(i18n::t("Текст должен быть от 1 до 2000 символов"));
        }
        match self.api.create_post(forum_id, title, text).await {
            Ok(post) => self.open_created(&post, None).await,
            Err(e) => self.thread_refused(discord::refusal(&e)),
        }
    }

    fn thread_refused(&self, text: String) {
        on_ui(&self.ui, move |ui| {
            ui.set_thread_busy(false);
            ui.set_thread_error(text.into());
        });
    }

    /// A thread or post Discord just made: it is named where channels are looked up, put under its
    /// channel in the list (`parent`, for a thread of a text channel), and opened.
    async fn open_created(&mut self, thread: &Value, parent: Option<&str>) {
        let Some(id) = thread["id"].as_str().and_then(|i| snowflake(i).ok()).map(String::from) else {
            return self.thread_refused(i18n::t("Discord не вернул новую ветку"));
        };
        let name = readable(thread["name"].as_str().unwrap_or(""));
        self.channels.insert(id.clone(), ChannelInfo { name: name.clone(), topic: String::new(), dm: false, forum: false });
        self.thread_ids.insert(id.clone());
        let (row, parent) = (Channel { id: id.clone(), name: format!("↳ {name}"), kind: 0, clickable: true, is_thread: true, ..Default::default() }, parent.map(String::from));
        let shown = id.clone();
        on_ui(&self.ui, move |ui| {
            ui.set_thread_dialog(0);
            ui.set_thread_busy(false);
            ui.set_selected_channel(shown.into());
            let model = ui.get_channels();
            let (Some(parent), Some(list)) = (parent, model.as_any().downcast_ref::<VecModel<ChannelItem>>()) else { return };
            if let Some(at) = (0..list.row_count()).find(|&i| list.row_data(i).is_some_and(|c| c.id.as_str() == parent)) {
                list.insert(at + 1, row.item());
            }
        });
        if let Err(e) = self.open_messages(id, None).await {
            flash(&self.ui, discord::refusal(&e));
        }
    }

    /// The mic button: starts recording a voice message for the open chat, unless the mic is busy with
    /// the check in the voice settings.
    fn record_start(&mut self) {
        if self.recording.as_ref().is_some_and(|(_, r)| !r.failed()) {
            return;
        }
        self.record_stop();
        let refused = match &self.channel {
            None => Some(i18n::t("Сначала откройте чат")),
            Some(_) if audio::mic_testing() => Some(i18n::t("Микрофон занят проверкой в настройках голоса")),
            Some(_) => None,
        };
        if let Some(text) = refused {
            on_ui(&self.ui, |ui| ui.set_rec_state(0));
            return flash(&self.ui, text);
        }
        let channel = self.channel.clone().unwrap_or_default();
        let recorder = audio::Recorder::start(voice::saved_mic(), voicemsg::MAX_RECORD_SECS * 48_000);
        self.watch_recording(&recorder);
        self.recording = Some((channel, recorder));
        on_ui(&self.ui, |ui| {
            ui.set_rec_state(1);
            ui.set_rec_full(false);
            ui.set_rec_time("0:00".into());
        });
    }

    /// The timer of the recording bar; says so when the mic can't be opened.
    fn watch_recording(&self, recorder: &audio::Recorder) {
        use std::sync::atomic::Ordering;
        let (count, failed) = recorder.progress();
        let generation = self.rec_generation.clone();
        let mine = generation.load(Ordering::Relaxed);
        let ui = self.ui.clone();
        let max = (voicemsg::MAX_RECORD_SECS * 48_000) as u64;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if generation.load(Ordering::Relaxed) != mine {
                    return;
                }
                if failed.load(Ordering::Relaxed) {
                    on_ui(&ui, |ui| ui.set_rec_state(0));
                    return flash(&ui, i18n::t("Не удалось открыть микрофон"));
                }
                let n = count.load(Ordering::Relaxed);
                let time = voicemsg::clock(n as f64 / 48_000.0);
                // At the limit the mic is let go; the recording waits for the send (or cancel) click.
                on_ui(&ui, move |ui| {
                    ui.set_rec_time(time.into());
                    ui.set_rec_full(n >= max);
                });
            }
        });
    }

    /// Throws the recording away and lets the mic go.
    fn record_stop(&mut self) {
        self.rec_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.recording = None;
        on_ui(&self.ui, |ui| {
            ui.set_rec_state(0);
            ui.set_rec_full(false);
        });
    }

    /// The send click: the mic is let go, the sound encoded, and Discord's voice message flow followed once
    /// (a place for the file, the file, the message); a refusal is shown and nothing is tried again.
    async fn record_send(&mut self) {
        self.rec_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some((channel, recorder)) = self.recording.take() else {
            on_ui(&self.ui, |ui| ui.set_rec_state(0));
            return;
        };
        let samples = recorder.finish();
        let result = if samples.len() < 48_000 / 2 {
            Err(i18n::t("Голосовое сообщение слишком короткое"))
        } else {
            self.send_voice_message(&channel, samples).await
        };
        on_ui(&self.ui, |ui| {
            ui.set_rec_state(0);
            ui.set_rec_full(false);
        });
        if let Err(e) = result {
            log_line(&format!("voice message not sent: {e}"));
            flash(&self.ui, discord::refusal(&e));
        }
    }

    async fn send_voice_message(&self, channel: &str, samples: Vec<i16>) -> Result<(), String> {
        let secs = voicemsg::duration_secs(samples.len());
        let (file, wave) = tokio::task::spawn_blocking(move || voicemsg::encode(&samples).map(|f| (f, voicemsg::waveform(&samples))))
            .await
            .map_err(|e| e.to_string())??;
        let slot = self.api.upload_slot(channel, voicemsg::FILENAME, file.len() as u64).await?;
        let (url, uploaded) = voicemsg::slot_of(&slot).ok_or_else(|| i18n::t("Discord не дал места для файла"))?;
        self.api.put_upload(&url, file).await?;
        let (path, body) = voicemsg::message_request(channel, &uploaded, secs, &wave)?;
        self.api.post(&path, body).await?;
        Ok(())
    }
}

/// Asks Discord about one invite (once a session) and keeps the answer, with its server's icon.
async fn fetch_invite(api: &Api, images: &Images, code: &str, ours: &HashSet<String>) {
    let preview = match invite::preview_path(code) {
        Ok(path) => api.get(&path).await.ok().as_ref().and_then(invite::parse),
        Err(_) => None,
    };
    let icon = match preview.as_ref().and_then(|p| p.icon.clone()) {
        Some(url) => images.get(url, GUILD_ICON_PX, GUILD_ICON_PX).await,
        None => None,
    };
    let joined = preview.as_ref().is_some_and(|p| ours.contains(&p.guild_id));
    INVITES.lock().unwrap().insert(code.to_string(), SeenInvite { done: true, preview, icon, joined });
}

/// Bots' icons in the slash panel are drawn at 28 px at most.
const SLASH_ICON_PX: u32 = 64;
/// Heights of the slash panel's section headers and command rows (as in app.slint).
const SLASH_HEADER_H: f32 = 32.0;
const SLASH_ROW_H: f32 = 56.0;

thread_local! {
    /// The slash panel's bot icons, by bot id; lives on the UI thread.
    static SLASH_ICONS: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

/// A line of the slash panel as plain data (pictures can't cross threads; they're added on the UI thread).
#[derive(Clone)]
struct SlashLine {
    header: bool,
    recent: bool,
    app: String,
    title: String,
    detail: String,
    bot: String,
    pills: Vec<String>,
    letter: String,
    y: f32,
}

/// The first letter of a name, for an avatar without a picture.
fn initial(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

/// The panel's lines, its strip of bots (id, name, recent, letter, where its section starts) and the
/// list's whole height.
fn slash_panel(index: &slash::Index, rows: &[slash::Row]) -> (Vec<SlashLine>, Vec<SlashLine>, f32) {
    let (mut lines, mut strip, mut y) = (Vec::new(), Vec::new(), 0.0);
    for row in rows {
        let line = match row {
            slash::Row::Header(app) => {
                let bot = app.as_deref().and_then(|a| index.app(a));
                let title = bot.map_or_else(|| i18n::t("Часто используемые"), |b| b.name.clone());
                let id = bot.map_or_else(String::new, |b| b.id.clone());
                let line = SlashLine { header: true, recent: bot.is_none(), app: id, letter: initial(&title), title, detail: String::new(), bot: String::new(), pills: Vec::new(), y };
                strip.push(line.clone());
                y += SLASH_HEADER_H;
                line
            }
            slash::Row::Command(i) => {
                let entry = &index.entries[*i];
                let bot = index.app(&entry.app).map_or_else(String::new, |b| b.name.clone());
                let line = SlashLine { header: false, recent: false, app: entry.app.clone(), title: format!("/{}", entry.name), detail: entry.description.clone(), letter: initial(&bot), bot, pills: entry.pills(), y };
                y += SLASH_ROW_H;
                line
            }
        };
        lines.push(line);
    }
    (lines, strip, y)
}

/// Opens the slash panel with these lines; the first command is picked.
fn show_slash(ui: &AppWindow, lines: Vec<SlashLine>, strip: Vec<SlashLine>, height: f32) {
    let icon = |app: &str| SLASH_ICONS.with(|icons| icons.borrow().get(app).cloned());
    let first = lines.iter().position(|l| !l.header).map_or(0, |i| i as i32);
    let rows: Vec<SlashRow> = lines
        .into_iter()
        .map(|l| {
            let picture = icon(&l.app);
            SlashRow {
                header: l.header,
                recent: l.recent,
                app: l.app.into(),
                title: l.title.into(),
                detail: l.detail.into(),
                bot: l.bot.into(),
                pills: ModelRc::new(VecModel::from(l.pills.into_iter().map(slint::SharedString::from).collect::<Vec<_>>())),
                letter: l.letter.into(),
                has_icon: picture.is_some(),
                icon: picture.unwrap_or_default(),
                y: l.y,
            }
        })
        .collect();
    let apps: Vec<SlashApp> = strip
        .into_iter()
        .map(|l| {
            let picture = icon(&l.app);
            SlashApp { id: l.app.into(), name: l.title.into(), recent: l.recent, letter: l.letter.into(), has_icon: picture.is_some(), icon: picture.unwrap_or_default(), y: l.y }
        })
        .collect();
    ui.set_slash_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_slash_apps(ModelRc::new(VecModel::from(apps)));
    ui.set_slash_height(height);
    ui.set_slash_index(first);
    ui.set_slash_open(true);
}

/// The soundboard's tiles (three to a row, as wide as app.slint draws them), section headers and gaps.
const SOUND_TILE: (f32, f32) = (92.0, 40.0);
const SOUND_HEADER_H: f32 = 28.0;
const SOUND_GAP: f32 = 6.0;

/// A soundboard tile or section header as plain data (emoji pictures are added on the UI thread).
struct SoundTile {
    header: bool,
    id: String,
    guild: String,
    name: String,
    emoji: String,
    emoji_key: String,
    x: f32,
    y: f32,
}

/// The soundboard's tiles, its strip of sections (kind, name, where it starts) and the list's whole height.
/// `server` is the call's server's name.
fn sound_panel(sections: &[soundboard::Section], server: &str) -> (Vec<SoundTile>, Vec<(i32, String, f32)>, f32) {
    let (placed, height) = soundboard::layout(sections, 3, SOUND_TILE, SOUND_HEADER_H, SOUND_GAP);
    let (mut tiles, mut strip) = (Vec::new(), Vec::new());
    for (si, sound, x, y) in placed {
        let section = &sections[si];
        let tile = match sound.map(|i| &section.sounds[i]) {
            None => {
                let name = match section.kind {
                    0 => i18n::t("Недавние"),
                    1 if !server.is_empty() => server.to_string(),
                    1 => i18n::t("Этот сервер"),
                    _ => i18n::t("Стандартные"),
                };
                strip.push((section.kind, name.clone(), y));
                SoundTile { header: true, id: String::new(), guild: String::new(), name, emoji: String::new(), emoji_key: String::new(), x, y }
            }
            Some(s) => SoundTile {
                header: false,
                id: s.id.clone(),
                guild: s.guild.clone().unwrap_or_default(),
                name: s.name.clone(),
                // A server emoji is drawn as its picture, not its name.
                emoji: if s.emoji_id.is_some() { String::new() } else { s.emoji.clone() },
                emoji_key: s.emoji_key().unwrap_or_default(),
                x,
                y,
            },
        };
        tiles.push(tile);
    }
    (tiles, strip, height)
}

/// Fills the soundboard with these tiles.
fn show_sound_panel(ui: &AppWindow, tiles: Vec<SoundTile>, strip: Vec<(i32, String, f32)>, height: f32) {
    let items: Vec<SoundItem> = tiles
        .into_iter()
        .map(|t| {
            let picture = Some(t.emoji_key.as_str()).filter(|k| !k.is_empty()).and_then(emoji_image);
            SoundItem {
                header: t.header,
                id: t.id.into(),
                guild: t.guild.into(),
                name: t.name.into(),
                emoji: t.emoji.into(),
                has_emoji_image: picture.is_some(),
                emoji_image: picture.unwrap_or_default(),
                emoji_key: t.emoji_key.into(),
                x: t.x,
                y: t.y,
            }
        })
        .collect();
    let sections: Vec<SoundSection> =
        strip.into_iter().map(|(kind, name, y)| SoundSection { kind, letter: initial(&name).into(), name: name.into(), icon: Image::default(), has_icon: false, y }).collect();
    ui.set_sound_items(ModelRc::new(VecModel::from(items)));
    ui.set_sound_sections(ModelRc::new(VecModel::from(sections)));
    ui.set_sound_height(height);
}

/// A GIF tile's key in the panel: its address, or a category's name.
fn gif_key(g: &gifs::Gif) -> String {
    if g.category.is_empty() {
        g.url.clone()
    } else {
        format!("category:{}", g.category)
    }
}

fn saved_token() -> Option<String> {
    keyring::Entry::new("nimbus-client", "discord-token").and_then(|e| e.get_password()).ok().filter(|t| !t.trim().is_empty())
}

/// The sign-in screen: waits until there is a token, typed here or saved by the other
/// Nimbus (the one with the discord.com sign-in window) while this one waited.
async fn wait_for_login(ui: &Weak<AppWindow>, cmds: &mut UnboundedReceiver<Cmd>, reason: &str) -> Option<String> {
    let reason = reason.to_string();
    on_ui(ui, move |ui| {
        ui.set_login_error(reason.into());
        ui.set_login_needed(true);
    });
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    let token = loop {
        tokio::select! {
            _ = poll.tick() => {
                if std::env::var_os("NIMBUS_FORCE_LOGIN").is_none() {
                    if let Some(token) = saved_token() {
                        break token;
                    }
                }
            }
            cmd = cmds.recv() => match cmd {
                None => return None,
                Some(Cmd::Login(raw)) => {
                    let token = raw.trim().trim_matches('"').to_string();
                    if token.is_empty() {
                        continue;
                    }
                    match Api::new(token.clone()).get("/users/@me").await {
                        Ok(_) => {
                            let _ = keyring::Entry::new("nimbus-client", "discord-token").and_then(|e| e.set_password(&token));
                            break token;
                        }
                        Err(e) => {
                            let text = if e.starts_with("Discord API") {
                                i18n::tf("Discord не принял токен: {}", &[&e])
                            } else {
                                log_line(&format!("sign-in: {e}"));
                                netcheck::set_down(ui, true);
                                discord::friendly(&e)
                            };
                            on_ui(ui, move |ui| ui.set_login_error(text.into()));
                        }
                    }
                }
                Some(_) => {}
            },
        }
    };
    on_ui(ui, |ui| ui.set_login_needed(false));
    Some(token)
}

/// The biggest picture sent as our avatar or banner.
const PROFILE_PICTURE_MAX: u64 = 8 * 1024 * 1024;

/// The type of a picture by its first bytes (the file name is not trusted).
fn picture_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A picture file as a data URI for Discord's profile API; refused when it is too big or no picture.
fn picture_data_uri(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| i18n::t("Не удалось открыть файл"))?;
    // One byte more than allowed: a file that grew meanwhile is caught too.
    let mut bytes = Vec::new();
    file.take(PROFILE_PICTURE_MAX + 1).read_to_end(&mut bytes).map_err(|_| i18n::t("Не удалось прочитать файл"))?;
    if bytes.len() as u64 > PROFILE_PICTURE_MAX {
        return Err(i18n::t("Картинка больше 8 МБ"));
    }
    let mime = picture_mime(&bytes).ok_or_else(|| i18n::t("Нужна картинка PNG, JPEG, GIF или WebP"))?;
    Ok(format!("data:{mime};base64,{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)))
}

/// The still picture of a profile banner (the CDN scales it to a size we ask for).
fn banner_url(u: &Value) -> Option<String> {
    let (id, hash) = (u["id"].as_str()?, u["banner"].as_str().filter(|h| !h.is_empty() && !h.contains('/'))?);
    Some(format!("{CDN}/banners/{id}/{hash}.webp?size=480"))
}

/// Shows our profile on the card and the editor: names, banner (or its colour), avatar; `user` is
/// `/users/@me` or the answer to a change of it.
fn show_profile(ui: &Weak<AppWindow>, images: &Arc<Images>, user: &Value) {
    let (name, username) = (display_name(user), readable(user["username"].as_str().unwrap_or("")));
    let global = user["global_name"].as_str().unwrap_or("").to_string();
    let color = user["banner_color"]
        .as_str()
        .and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok())
        .or_else(|| user["accent_color"].as_u64().map(|c| c as u32 & 0xff_ffff));
    on_ui(ui, move |ui| {
        ui.set_me_name(name.into());
        ui.set_me_username(username.into());
        ui.set_prof_global_name(global.into());
        ui.set_prof_has_color(color.is_some());
        if let Some(c) = color {
            ui.set_prof_color(rgb_color(c));
        }
    });
    let (avatar, banner) = (avatar_url(user), banner_url(user));
    let (images, ui) = (images.clone(), ui.clone());
    tokio::spawn(async move {
        if let Some(px) = images.get(avatar, 128, 128).await {
            on_ui(&ui, move |ui| ui.set_me_avatar(Image::from_rgba8(px)));
        }
        match banner {
            Some(url) => {
                if let Some(px) = images.get(url, 480, 192).await {
                    on_ui(&ui, move |ui| {
                        ui.set_prof_banner(Image::from_rgba8(px));
                        ui.set_prof_has_banner(true);
                    });
                }
            }
            None => on_ui(&ui, |ui| ui.set_prof_has_banner(false)),
        }
    });
}

/// The body of a privacy change that comes as a name and a number from the settings page.
fn privacy_body(key: &str, value: i64) -> Option<Value> {
    match key {
        // Which direct messages are scanned for explicit pictures: 0 none, 1 from non-friends, 2 all.
        "explicit" if (0..=2).contains(&value) => Some(json!({ "explicit_content_filter": value })),
        // How others see us: 0 online, 1 idle, 2 do not disturb, 3 invisible.
        "status" if (0..=3).contains(&value) => {
            let status = ["online", "idle", "dnd", "invisible"][value as usize];
            Some(json!({ "status": status }))
        }
        "show-game" => Some(json!({ "show_current_game": value != 0 })),
        _ => None,
    }
}

/// Shows the privacy settings Discord keeps for the account on the settings pages.
fn push_privacy(ui: &Weak<AppWindow>, settings: &Value) {
    let explicit = settings["explicit_content_filter"].as_i64();
    let flags = &settings["friend_source_flags"];
    let (all, friends, guilds) = (flags["all"].as_bool(), flags["mutual_friends"].as_bool(), flags["mutual_guilds"].as_bool());
    let loaded = explicit.is_some() && flags.is_object();
    let status = match settings["status"].as_str() {
        Some("idle") => 1,
        Some("dnd") => 2,
        Some("invisible") => 3,
        _ => 0,
    };
    let custom = settings["custom_status"]["text"].as_str().unwrap_or("").to_string();
    let show_game = settings["show_current_game"].as_bool().unwrap_or(true);
    if let Some(name) = settings["status"].as_str() {
        *discord::PRESENCE.lock().unwrap() = Some(discord::presence(name, &custom));
    }
    on_ui(ui, move |ui| {
        ui.set_my_status(status);
        ui.set_my_custom_status(custom.into());
        ui.set_show_game(show_game);
        ui.set_priv_ready(loaded);
        ui.set_priv_explicit(explicit.unwrap_or(0) as i32);
        ui.set_priv_friends_all(all.unwrap_or(false));
        ui.set_priv_friends_friends(friends.unwrap_or(false));
        ui.set_priv_friends_guilds(guilds.unwrap_or(false));
    });
}

/// Forgets the saved token (Discord no longer accepts it).
fn forget_token() {
    clear_message_cache(false);
    pages::forget_recent();
    gifcache::clear();
    let _ = keyring::Entry::new("nimbus-client", "discord-token").and_then(|e| e.delete_credential());
}

/// Whether an error from Discord says the token is no longer valid.
fn is_unauthorized(error: &str) -> bool {
    error.starts_with("Discord API 401")
}


/// Runs the app; when Discord stops accepting the token, goes back to the sign-in screen and runs again.
async fn run(ui: Weak<AppWindow>, mut cmds: UnboundedReceiver<Cmd>) {
    let mut reason = String::new();
    // NIMBUS_FORCE_LOGIN=1 shows the sign-in screen even with a saved account (for trying it out).
    let mut first = true;
    loop {
        discord::AUTH_LOST.store(false, std::sync::atomic::Ordering::Relaxed);
        let saved = if first && std::env::var_os("NIMBUS_FORCE_LOGIN").is_some() { None } else { saved_token() };
        first = false;
        if !run_once(ui.clone(), &mut cmds, saved, &reason).await {
            return;
        }
        reason = i18n::t("Discord больше не принимает вход (аккаунт вышел или сменил пароль). Войди заново.");
    }
}

/// One signed-in run. True when it ended because the token stopped working (sign in again), false to quit.
async fn run_once(ui: Weak<AppWindow>, cmds: &mut UnboundedReceiver<Cmd>, saved: Option<String>, reason: &str) -> bool {
    let token = match saved {
        Some(token) => token,
        None => match wait_for_login(&ui, cmds, reason).await {
            Some(token) => token,
            None => return false,
        },
    };
    let api = Api::new(token);
    let me = loop {
        match api.get("/users/@me").await {
            Ok(me) => break me,
            Err(e) if is_unauthorized(&e) => {
                forget_token();
                return true;
            }
            // Nothing can work without this answer, and the network may come back: keep asking, and offer to look into it.
            Err(e) => {
                log_line(&format!("start: {e}"));
                let text = discord::friendly(&e);
                on_ui(&ui, move |ui| ui.set_status(text.into()));
                netcheck::set_down(&ui, true);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    cmd = cmds.recv() => {
                        if cmd.is_none() {
                            return false;
                        }
                    }
                }
            }
        }
    };
    discord::AUTH_LOST.store(false, std::sync::atomic::Ordering::Relaxed);
    let images = Arc::new(Images { http: api.http.clone(), cache: Mutex::new(HashMap::new()), gifs: Mutex::new(gifcache::Kept::new(gifcache::MEMORY_BYTES)) });
    let _ = VOICE_AVATARS.set(VoiceAvatars {
        images: images.clone(),
        ui: ui.clone(),
        urls: Mutex::default(),
        pixels: Mutex::default(),
        frame_urls: Mutex::default(),
        frames: Mutex::default(),
    });
    note_user(&me);

    let me_name = display_name(&me);
    let me_id = me["id"].as_str().unwrap_or("").to_string();
    let (me_email, me_phone, me_mfa) =
        (me["email"].as_str().unwrap_or("").to_string(), me["phone"].as_str().unwrap_or("").to_string(), me["mfa_enabled"].as_bool().unwrap_or(false));
    on_ui(&ui, move |ui| {
        ui.set_me_name(me_name.into());
        ui.set_me_id(me_id.into());
        ui.set_me_email(me_email.into());
        ui.set_me_phone(me_phone.into());
        ui.set_me_mfa(me_mfa);
    });
    // What Discord has for the privacy pages of the settings (they say so when this doesn't come).
    match api.get("/users/@me/settings").await {
        Ok(settings) => push_privacy(&ui, &settings),
        Err(e) => log_line(&format!("settings: no privacy settings ({e})")),
    }
    // The "about me" of our own profile.
    if let Ok(profile) = api.get(&format!("/users/{}/profile", me["id"].as_str().unwrap_or("@me"))).await {
        let bio = profile["user_profile"]["bio"].as_str().unwrap_or("").to_string();
        let pronouns = profile["user_profile"]["pronouns"].as_str().unwrap_or("").to_string();
        on_ui(&ui, move |ui| {
            ui.set_my_bio(bio.into());
            ui.set_prof_pronouns(pronouns.into());
        });
    }
    show_profile(&ui, &images, &me);

    // What the person bought for their profile: the nameplate behind the user card, the frame on the avatar.
    let look = profile_look(&me);
    let palette = look.palette.clone();
    if look.nameplate.is_some() || look.decoration.is_some() {
        let (cosmetic_images, cosmetic_ui) = (images.clone(), ui.clone());
        tokio::spawn(async move {
            if let Some(url) = look.nameplate {
                if let Some(px) = cosmetic_images.get(url, PLATE_W, PLATE_H).await {
                    let px = under_palette(px, &palette);
                    on_ui(&cosmetic_ui, move |ui| {
                        ui.set_me_banner(Image::from_rgba8(px));
                        ui.set_me_has_banner(true);
                    });
                }
            }
            if let Some(url) = look.decoration {
                if let Some(px) = cosmetic_images.get(url, 160, 160).await {
                    on_ui(&cosmetic_ui, move |ui| {
                        ui.set_me_decoration(Image::from_rgba8(px));
                        ui.set_me_has_decoration(true);
                    });
                }
            }
        });
    }

    let (event_tx, mut events) = mpsc::unbounded_channel();
    let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
    tokio::spawn(discord::run_gateway(api.token().to_string(), event_tx, gateway_rx));
    let (voice_tx, voice_rx) = mpsc::unbounded_channel();
    let members_gateway = gateway_tx.clone();
    tokio::spawn(voice::run(ui.clone(), api.http.clone(), voice_rx, gateway_tx));
    static UPDATES: std::sync::Once = std::sync::Once::new();
    UPDATES.call_once(|| {
        tokio::spawn(update::watch(ui.clone()));
    });

    let mut worker = Worker {
        ui,
        api,
        images,
        user_profiles: HashMap::new(),
        ready: Value::Null,
        guild: None,
        guild_names: HashMap::new(),
        channel: None,
        channels: HashMap::new(),
        last: None,
        message_rows: 0,
        history: History::default(),
        forum_posts: Vec::new(),
        voice_rooms: HashMap::new(),
        menu_guild: None,
        spotify: HashMap::new(),
        raw: HashMap::new(),
        role_of: HashMap::new(),
        thread_ids: HashSet::new(),
        dm_call_of: HashMap::new(),
        dm_call_declined: HashSet::new(),
        ring_pending: None,
        last_join: None,
        premium_type: {
            let kind = me["premium_type"].as_u64().unwrap_or(0);
            golive_send::set_premium(kind); // Go Live's limits follow it
            kind
        },
        input_text: String::new(),
        guild_emojis: HashMap::new(),
        member_chunks: members::FIRST_CHUNKS,
        suggest_at: 0,
        suggestions: Vec::new(),
        picked_mentions: Vec::new(),
        gateway: members_gateway,
        members: members::List::default(),
        members_guild: None,
        past_sessions: Vec::new(),
        members_channel: None,
        members_kept: HashMap::new(),
        members_asked: None,
        group_shown: None,
        role_tables: HashMap::new(),
        discord_voice: HashMap::new(),
        users: HashMap::new(),
        voice: voice_tx,
        me_id: { let id = me["id"].as_str().unwrap_or("").to_string(); let _ = ME_ID.set(id.clone()); id },
        me_name: display_name(&me),
        guild_server: DEFAULT_SERVER.to_string(),
        voice_channel: None,
        presences: HashMap::new(),
        people_page: 0,
        trail: fold::Trail::default(),
        friends_tab: 0,
        home_dirty: false,
        search_results: Vec::new(),
        recent_picks: prefs::read()["recent_search"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
        unread: unread::Unread::default(),
        typing: chat::Typing::default(),
        typing_shown: String::new(),
        typing_sent: None,
        reactions: HashMap::new(),
        emoji_asked: HashSet::new(),
        polls: HashMap::new(),
        components: HashMap::new(),
        select_chosen: HashMap::new(),
        interactions: HashMap::new(),
        pending_files: Vec::new(),
        events: HashMap::new(),
        events_interested: HashSet::new(),
        event_avatars: HashMap::new(),
        clips: HashMap::new(),
        clip_sound: HashMap::new(),
        clip_paused: HashMap::new(),
        playing: None,
        clip_generation: Arc::default(),
        preview_at: HashMap::new(),
        guild_cache: HashMap::new(),
        active_threads: HashMap::new(),
        pages: Arc::default(),
        preloaded: false,
        gifs: Vec::new(),
        gifs_shown: 0,
        forward_from: None,
        forward_targets: Vec::new(),
        notes: HashMap::new(),
        recording: None,
        rec_generation: Arc::default(),
        slash: HashMap::new(),
        slash_rows: Vec::new(),
        slash_recent: prefs::read()["slash_recent"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
        slash_icons_asked: HashSet::new(),
        slash_shown: false,
        sound_defaults: None,
        sound_servers: HashMap::new(),
        sound_recent: prefs::read()["sound_recent"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
        sound_query: String::new(),
        sounds_kept: Arc::new(Mutex::new(soundboard::Kept::new(soundboard::KEPT))),
        sounds_muted: prefs::read()["sounds_muted"].as_bool().unwrap_or(false),
    };
    // Presences change often; the DM list catches up once a second.
    let mut home_tick = tokio::time::interval(Duration::from_secs(1));
    let mut auth_tick = tokio::time::interval(Duration::from_secs(3));
    let mut signed_out = false;
    loop {
        tokio::select! {
            _ = auth_tick.tick() => {
                // A 401 or a gateway close 4004 was seen: ask once more before giving up the account.
                if discord::AUTH_LOST.load(std::sync::atomic::Ordering::Relaxed) {
                    match worker.api.get("/users/@me").await {
                        Err(e) if is_unauthorized(&e) => {
                            forget_token();
                            let _ = worker.voice.send(voice::VoiceCmd::Leave);
                            signed_out = true;
                            break;
                        }
                        _ => discord::AUTH_LOST.store(false, std::sync::atomic::Ordering::Relaxed),
                    }
                }
            }
            Some(event) = events.recv() => worker.on_event(event).await,
            Some(cmd) = cmds.recv() => worker.on_cmd(cmd).await,
            _ = home_tick.tick() => {
                worker.ask_members_again();
                if std::mem::take(&mut worker.home_dirty) {
                    worker.refresh_home();
                }
                // Typing runs out by itself after ten seconds.
                worker.show_typing();
                worker.refresh_previews();
                worker.push_guild_voice();
            }
            else => break,
        }
    }
    signed_out
}

/// `--demo` fills the window with made-up data, for screenshots without an account.
fn fill_demo(ui: &AppWindow) {
    netcheck::demo(ui);
    let guilds = [("@me", "Личные сообщения"), ("1", "Тестовый сервер"), ("2", "Nimbus Dev"), ("3", "Rust")]
        .map(|(id, name)| Guild { id: id.into(), name: name.into(), ..Default::default() }.item());
    // Each of the voice badges, once: someone in voice, someone showing a screen, you in voice.
    let guilds = {
        let mut guilds = guilds;
        for (i, g) in guilds.iter_mut().enumerate() {
            g.voice = if i == 0 { 0 } else { i as i32 };
        }
        guilds
    };
    let channel = |id: &str, name: &str, kind: i32| Channel { id: id.into(), name: name.into(), kind, clickable: kind == 0, folded: std::env::args().any(|a| a == "--demo-folded"), ..Default::default() }.item();
    let channels = vec![
        channel("c1", "ВАЖНОЕ", 2),
        channel("10", "rules", 0),
        channel("11", "announcements", 0),
        channel("c2", "ОБЩЕНИЕ", 2),
        channel("12", "main-chat", 0),
        channel("13", "stream", 0),
        channel("c3", "ВОЙСЫ", 2),
        channel("14", "Main", 1),
        channel("15", "For friends", 1),
    ];
    let mut channels = channels;
    let person = |name: &str, muted: bool, streaming: bool, discord: bool| VoiceMember {
        identity: name.into(),
        name: name.into(),
        initial: name.chars().next().unwrap_or('?').to_string().into(),
        avatar: Image::default(),
        has_avatar: false,
        muted,
        deafened: false,
        speaking: name == "demo_alice",
        streaming,
        discord,
        decoration: Image::default(),
        has_decoration: false,
    };
    channels[7].people = ModelRc::new(VecModel::from(vec![person("demo_alice", false, true, true), person("demo_bob", true, false, true)]));
    let msg = |author: &str, time: &str, text: &str, grouped: bool, system: bool, picture: Option<(f32, f32)>| {
        Msg {
            picture_links: PictureLinks::default(),
            id: String::new(),
            author_id: author.into(),
            author: readable(author),
            time: time.into(),
            short_time: "21:27".into(),
            blocks: blocks_of(&readable(text)),
            grouped,
            system,
            system_line: None,
            picture,
            edited: false,
            reactions: Vec::new(),
            highlight: false,
            color: if author == "demo_bob" { Some(0xf0be3c) } else { None },
            mention: author == "demo_alice" && !system,
            pinned: false,
            cards: if author == "demo_alice" && !system { vec![Card { kind: 0, title: "nimbus-setup.zip".into(), text: human_size(13_002_342).into(), url: "https://example.com/f".into(), color: None }, Card { kind: 1, title: "Nimbus на GitHub".into(), text: "Неофициальный клиент Discord с демонстрацией экрана через свой сервер.".into(), url: "https://example.com".into(), color: Some(0x7c6cf6) }] } else { Vec::new() },
            emojis: Vec::new(),
            sticker: None,
            poll: None,
            voice: None,
            forwarded: false,
            invites: Vec::new(),
            components: Vec::new(),
            target: components::Target::default(),
            ephemeral: false,
        }
        .item()
    };
    let mut messages = vec![
        msg("demo_alice", "", &format!("demo_alice {} · {}", system_action(7).unwrap_or_default(), i18n::tf("Вчера, {}", &[&"20:32"])), false, true, None),
        msg("𝐆𝐨𝐥𝐝𝐌𝐚𝐧", &i18n::tf("Сегодня, {}", &[&"21:27"]), "кто сегодня в раст? собираемся в **9 вечера**", false, false, None),
        msg("𝐆𝐨𝐥𝐝𝐌𝐚𝐧", &i18n::tf("Сегодня, {}", &[&"21:27"]), "https://example.com/rust-server", true, false, None),
        msg("Алекс", &i18n::tf("Сегодня, {}", &[&"21:52"]), "## План на вечер\nя буду, только `патч` докачаю\n```toml\n[stream]\nfps = 60\nbitrate = \"20M\"\n```", false, false, None),
        msg("demo_alice", &i18n::tf("Сегодня, {}", &[&"22:20"]), &format!("> {}\n@Алекс стримить будешь через Nimbus?", i18n::tf("в ответ {}: {}", &[&"Алекс", &"я буду, только патч докачаю"])), false, false, None),
        msg("demo_bob", &i18n::tf("Сегодня, {}", &[&"22:21"]), "**жирный**, *курсив*, ~~зачёркнутый~~, `код`, __подчёркнутый__ и ссылка https://example.com/page. Длинная строка, чтобы проверить, что текст переносится по словам внутри колонки сообщений.\nвторая строка\n- пункт один\n- пункт два", false, false, None),
        msg("Алекс", &i18n::tf("Сегодня, {}", &[&"22:22"]), "да, 1440p60 ||без лагов||", false, false, Some((400.0, 225.0))),
    ];
    // A gradient stands in for a downloaded picture.
    let mut pixels = Pixels::new(800, 450);
    for (i, px) in pixels.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 800) as u32, (i / 800) as u32);
        *px = Rgba8Pixel { r: (30 + x * 60 / 800) as u8, g: (58 - y * 20 / 450) as u8, b: (95 + x * 40 / 800) as u8, a: 255 };
    }
    if let Some(last) = messages.last_mut() {
        last.picture = Image::from_rgba8(pixels);
    }
    // An edited message with reactions, and a deleted one folded away.
    let reaction = |key: &str, count: u32, me: bool| chat::Reaction { key: key.into(), text: key.into(), image: None, count, me };
    messages[3].edited = true;
    messages[3].reactions = reaction_items(&[reaction("👍", 3, true), reaction("🔥", 1, false), reaction(":pog:", 2, false)]);
    messages[2].deleted = true;
    if std::env::args().any(|a| a == "--demo-media") {
        messages = demo_media();
    }
    // `--demo-events`: the server's events window with two made-up events (one on a stage now, one elsewhere
    // tomorrow); `--demo-events-create`: the second step of creating one.
    let events_create = std::env::args().any(|a| a == "--demo-events-create");
    if events_create || std::env::args().any(|a| a == "--demo-events") {
        demo_events(ui, events_create);
    }
    // `--demo-paste`: a pasted picture (a made-up bitmap, through the same conversion as a real one) and a
    // pasted file waiting above the message box.
    if std::env::args().any(|a| a == "--demo-paste") {
        let mut dib = Vec::new();
        let (w, h) = (320i32, 200i32);
        for v in [40u32, w as u32, h as u32] {
            dib.extend(v.to_le_bytes());
        }
        dib.extend(1u16.to_le_bytes());
        dib.extend(24u16.to_le_bytes());
        dib.extend([0u8; 24]);
        for y in 0..h {
            for x in 0..w {
                dib.extend([(140 + x * 100 / w) as u8, (60 + y * 120 / h) as u8, (90 + (x + y) * 60 / (w + h)) as u8]);
            }
        }
        let mut cards = Vec::new();
        if let Ok(p) = clipboard::prepare(clipboard::Pasted::Dib(dib), 10 << 20) {
            let thumb = p.thumb.as_ref().map(|t| Pixels::clone_from_slice(t.as_raw(), t.width(), t.height()));
            cards.push(pending_item(p.name, human_size(p.bytes.len() as u64), thumb));
        }
        cards.push(pending_item("отчёт-за-октябрь.pdf".into(), human_size(1_843_200), None));
        ui.set_pending_files(ModelRc::new(VecModel::from(cards)));
        ui.invoke_set_input("вот скрин с ошибкой".into());
    }
    // `--demo-components`: a bot's message with buttons and a select menu (made up); `--demo-components-open`
    // with the menu open.
    let components_open = std::env::args().any(|a| a == "--demo-components-open");
    if components_open || std::env::args().any(|a| a == "--demo-components") {
        messages.truncate(2);
        messages.extend(demo_components(ui, components_open));
    }
    // `--demo-forwarded`: a forwarded message (made up), read the way one from Discord is.
    if std::env::args().any(|a| a == "--demo-forwarded") {
        messages.insert(1, demo_forwarded());
    }
    // `--demo-viewer`: a picture open in the viewer; `--demo-card`: a person's card over the chat;
    // `--demo-dm-panel`: a DM with the person's panel. All made up.
    let demo_card = || UserCardItem {
        id: "77".into(),
        name: "Паук".into(),
        username: "spiderkaras".into(),
        pronouns: "они/их".into(),
        bio: "Строю базы в Rust.\nПишите, если нужна помощь.".into(),
        tag: "RUST".into(),
        mutual: i18n::tf("Общие друзья: {} • Общие серверы: {}", &[&26, &4]).into(),
        since: clock::joined("805000000000000000").unwrap_or_default().into(),
        status: 2,
        me: false,
        has_color: false,
        banner: demo_gradient(680, 240),
        has_banner: true,
        avatar: demo_ball(176, (235, 170, 90), true),
        badges: ModelRc::new(VecModel::from(vec![(90, 160, 255), (230, 90, 200), (120, 220, 130)].into_iter().map(|rgb| BadgeItem { picture: demo_ball(44, rgb, true), has_picture: true, ..Default::default() }).collect::<Vec<_>>())),
        ..Default::default()
    };
    if std::env::args().any(|a| a == "--demo-login") {
        ui.set_login_needed(true);
    }
    // `--demo-login-busy`: the button while the sign-in window is open.
    if std::env::args().any(|a| a == "--demo-login-busy") {
        ui.set_login_needed(true);
        ui.set_login_signing(true);
    }
    // `--demo-login-leave=MS`: the sign-in succeeds after that many milliseconds.
    if let Some(after) = std::env::args().find_map(|a| a.strip_prefix("--demo-login-leave=").and_then(|n| n.parse::<u64>().ok())) {
        ui.set_login_needed(true);
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(after), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_login_needed(false);
            }
        });
    }
    if std::env::args().any(|a| a == "--demo-card") {
        ui.set_user_card(demo_card());
        ui.set_user_card_open(true);
        ui.invoke_place_user_card(380.0, 170.0);
    }
    if std::env::args().any(|a| a == "--demo-dm-panel") {
        ui.set_dm_card(demo_card());
        ui.set_channel_is_dm(true);
        ui.set_channel_title("Паук".into());
        ui.set_channel_topic("".into());
    }
    if std::env::args().any(|a| a == "--demo-viewer") {
        let mut item = messages[0].clone();
        item.author = "demo_bob".into();
        item.time = i18n::tf("Вчера, {}", &[&"21:27"]).into();
        item.avatar = demo_ball(72, (235, 170, 90), true);
        item.picture = demo_gradient(480, 270);
        item.picture_width = 480.0;
        item.picture_height = 270.0;
        ui.set_viewer_item(item);
        ui.set_viewer_image(demo_gradient(1280, 720));
        ui.set_viewer_has_image(true);
        ui.set_viewer_open(true);
    }
    // `--demo-mentions`: a reply that pings people, roles and a channel (all made up).
    if std::env::args().any(|a| a == "--demo-mentions") {
        messages.insert(1, demo_mentions());
    }
    // `--demo-notify`: a muted channel (dimmed) and a server's notification menu; with `--demo-notify-channel`,
    // a channel's menu instead (it follows the server).
    if std::env::args().any(|a| a == "--demo-notify") {
        channels[2].muted = true;
        let of_channel = std::env::args().any(|a| a == "--demo-notify-channel");
        let (channel, name, at) = if of_channel { ("13", "#stream", (230.0, 290.0)) } else { ("", "Тестовый сервер", (64.0, 150.0)) };
        ui.set_notify_menu(NotifyMenuInfo { guild: "1".into(), channel: channel.into(), name: name.into(), level: 1, inherits: of_channel, muted: false });
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_open_notify_menu("1".into(), channel.into(), at.0, at.1);
                ui.invoke_show_notify_menu();
            }
        });
    }
    // `--demo-invite-card`: messages with invite links, drawn as cards (made-up servers, one invite gone).
    if std::env::args().any(|a| a == "--demo-invite-card") {
        let at = 1.min(messages.len());
        messages.splice(at..at, demo_invites());
    }
    // `--demo-join`: the join window with a made-up server's preview.
    if std::env::args().any(|a| a == "--demo-join") {
        demo_invites();
        ui.set_join_text("https://discord.gg/nimbus-dev".into());
        ui.set_join_preview(invite_item("nimbus-dev"));
        ui.set_join_state(2);
        ui.set_join_open(true);
    }
    // `--demo-join-check`: the same, when Discord wants its own check.
    if std::env::args().any(|a| a == "--demo-join-check") {
        demo_invites();
        ui.set_join_text("https://discord.gg/nimbus-dev".into());
        ui.set_join_preview(invite_item("nimbus-dev"));
        ui.set_join_state(6);
        ui.set_join_error(i18n::t("Discord просит пройти проверку. Войдите по приглашению в официальном клиенте").into());
        ui.set_join_open(true);
    }
    // `--demo-made-invite`: a channel's new invite, ready to copy (made up).
    if std::env::args().any(|a| a == "--demo-made-invite") {
        ui.set_made_invite_place("#main-chat".into());
        ui.set_made_invite_link(invite::link("Xk7pQ2mN").into());
        ui.set_made_invite_state(2);
    }
    // `--demo-guild-invite`: a server's invite window with made-up friends, one already invited.
    if std::env::args().any(|a| a == "--demo-guild-invite") {
        let friend = |id: &str, name: &str, tag: &str, kind: i32| PersonItem {
            id: id.into(),
            name: name.into(),
            initial: name.chars().next().map(|c| c.to_string()).unwrap_or_default().into(),
            subtitle: tag.into(),
            kind,
            ..Default::default()
        };
        INVITE_PEOPLE.set(vec![friend("1", "Alice", "demo_alice", 0), friend("2", "Bob", "demo_bob", 2), friend("3", "Zed", "demo_zed", 0)]);
        show_invite_people(ui);
        ui.set_made_invite_place("Тестовый сервер".into());
        ui.set_made_invite_channel("main-chat".into());
        ui.set_made_invite_link(invite::link("Xk7pQ2mN").into());
        ui.set_invite_rich(true);
        ui.set_made_invite_state(2);
    }
    // `--demo-thread`: the window for a thread from a message, a name typed in.
    if std::env::args().any(|a| a == "--demo-thread") {
        ui.set_thread_draft("План на вечер".into());
        ui.set_thread_dialog(1);
    }
    // `--demo-recording`: a voice message being recorded.
    if std::env::args().any(|a| a == "--demo-recording") {
        ui.set_rec_time("0:42".into());
        ui.set_rec_state(1);
    }
    // `--demo-forward`: the picker of where to forward a message (made-up places).
    if std::env::args().any(|a| a == "--demo-forward") {
        let place = |id: &str, name: &str, detail: &str, dm: bool| ForwardTarget { id: id.into(), name: name.into(), detail: detail.into(), dm };
        ui.set_forward_targets(ModelRc::new(VecModel::from(vec![
            place("12", "#main-chat", "Тестовый сервер", false),
            place("13", "#stream", "Тестовый сервер", false),
            place("11", "#announcements", "Тестовый сервер", false),
            place("21", "demo_carol", "Личные сообщения", true),
            place("22", "demo_eve", "Личные сообщения", true),
            place("23", "Группа demo_user", "Личные сообщения", true),
        ])));
        ui.set_forwarding(true);
    }
    // `--demo-gifs`: the GIF panel with made-up tiles (one of them moving) and two categories.
    if std::env::args().any(|a| a == "--demo-gifs") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_open_gif();
                show_demo_gifs(&ui);
            }
        });
    }
    ui.set_typing_line(i18n::tf("{} печатает…", &[&"demo_alice"]).into());
    ui.set_guilds(ModelRc::new(VecModel::from(guilds.to_vec())));
    sync_rail(ui);
    ui.set_channels(ModelRc::new(VecModel::from(channels)));
    // `--demo-many=N`: the same messages again and again, to see how a long list behaves.
    let many = std::env::args().find_map(|a| a.strip_prefix("--demo-many=").and_then(|n| n.parse::<usize>().ok()));
    if let Some(n) = many {
        let base = messages.clone();
        messages = base.iter().cycle().take(n).cloned().collect();
        let line = format!("--demo-many: {n} messages in the list");
        log_line(&line);
    }
    // `--demo-incoming`: a made-up message arrives every 1.5 s, to watch them come in.
    let incoming = messages.last().cloned().filter(|_| std::env::args().any(|a| a == "--demo-incoming"));
    ui.set_messages(ModelRc::new(VecModel::from(messages)));
    if let Some(sample) = incoming {
        let weak = ui.as_weak();
        let count = std::cell::Cell::new(0);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(1500), move || {
            let Some(ui) = weak.upgrade() else { return };
            count.set(count.get() + 1);
            let row = MessageItem { id: format!("demo-incoming-{}", count.get()).into(), grouped: false, ..sample.clone() };
            push_fresh(&ui, row);
            ui.invoke_follow_bottom();
        });
        std::mem::forget(timer);
    }
    ui.set_selected_guild("1".into());
    ui.set_selected_channel("12".into());
    ui.set_sidebar_title("Тестовый сервер".into());
    ui.set_channel_title("main-chat".into());
    ui.set_channel_topic("Болтаем обо всём, кроме политики".into());
    ui.set_placeholder(i18n::tf("Написать в #{}", &[&"main-chat"]).into());
    ui.set_window_title("#main-chat · Тестовый сервер — Nimbus".into());
    ui.set_me_name("Алекс".into());
    ui.set_status(i18n::t("В сети").into());
    ui.set_online(true);
    ui.set_notice("".into());
    // `--demo-cosmetics`: a made-up banner behind the user card and a ring around the avatar.
    if std::env::args().any(|a| a == "--demo-cosmetics") {
        let mut banner = SharedPixelBuffer::<slint::Rgba8Pixel>::new(480, 192);
        for (i, p) in banner.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 480) as u32, (i / 480) as u32);
            *p = slint::Rgba8Pixel { r: (200 - x / 4) as u8, g: (60 + y / 3) as u8, b: (120 + x / 5) as u8, a: 255 };
        }
        let mut ring = SharedPixelBuffer::<slint::Rgba8Pixel>::new(160, 160);
        for (i, p) in ring.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 160) as f32 - 79.5, (i / 160) as f32 - 79.5);
            let d = (x * x + y * y).sqrt();
            let on = d > 62.0 && d < 78.0;
            *p = slint::Rgba8Pixel { r: 255, g: 120, b: 190, a: if on { 255 } else { 0 } };
        }
        ui.set_me_banner(Image::from_rgba8(banner));
        ui.set_me_has_banner(true);
        ui.set_me_decoration(Image::from_rgba8(ring));
        ui.set_me_has_decoration(true);
    }
    // `--demo-share`: the "what to show" window, with made-up screens and windows.
    if std::env::args().any(|a| a == "--demo-share") {
        let picture = |r: u8, g: u8, b: u8| {
            let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(320, 180);
            for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
                let (x, y) = ((i % 320) as u32, (i / 320) as u32);
                *p = slint::Rgba8Pixel { r: (r as u32 * (60 + y / 2) / 150).min(255) as u8, g: (g as u32 * (60 + x / 4) / 140).min(255) as u8, b, a: 255 };
            }
            Image::from_rgba8(buf)
        };
        let item = |name: &str, window: bool, thumb: Option<Image>| ShareSourceItem {
            name: name.into(),
            detail: "".into(),
            is_window: window,
            has_thumb: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
        };
        let items = vec![
            item("Экран 1", false, Some(picture(60, 90, 200))),
            item("Roworld", true, Some(picture(200, 120, 40))),
            item("Редактор", true, Some(picture(70, 70, 90))),
            item("Браузер", true, Some(picture(40, 150, 110))),
            item("Музыка", true, None),
        ];
        ui.set_share_monitors(1);
        ui.set_share_windows(4);
        ui.set_share_sources(ModelRc::new(VecModel::from(items)));
        ui.set_share_height(0);
        ui.set_share_fps(60);
        ui.set_share_sound(false);
        ui.set_share_picker_open(true);
    }
    // `--demo-home`: the DM side; with `--demo-friends`, its friends page.
    let dm_call = std::env::args().any(|a| a == "--demo-dm-call");
    if dm_call || std::env::args().any(|a| a == "--demo-home") {
        let dm = |id: &str, name: &str, subtitle: &str, status: i32| {
            Channel { id: id.into(), name: name.into(), kind: 0, clickable: true, has_avatar: true, subtitle: subtitle.into(), status, ..Default::default() }.item()
        };
        let rows = vec![
            Channel { id: "friends".into(), name: i18n::t("Друзья"), kind: 3, clickable: true, ..Default::default() }.item(),
            Channel { id: "requests".into(), name: i18n::t("Запросы общения"), kind: 3, clickable: true, badge: 2, ..Default::default() }.item(),
            Channel { id: "h".into(), name: i18n::t("Личные сообщения"), kind: 2, ..Default::default() }.item(),
            dm("1", "demo_friend", "", 1),
            dm("2", "demo_carol", "", 2),
            dm("3", "demo_eve", "ROBLOX", 1),
            dm("4", "demo_dan", "Goat Simulator", 3),
            dm("5", &i18n::tf("Группа {}", &[&"demo_user"]), &home::members_label(1), 0),
            dm("6", "demo_frank", &home::members_label(2), 0),
            dm("7", "demo_bob", "", 4),
        ];
        ui.set_selected_guild("@me".into());
        ui.set_sidebar_title(i18n::t("Личные сообщения").into());
        ui.set_channels(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_channel("1".into());
        if std::env::args().any(|a| a == "--demo-friends") {
            let person = |name: &str, subtitle: &str, status: i32, kind: i32| PersonItem {
                id: name.into(),
                name: name.into(),
                initial: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                subtitle: subtitle.into(),
                avatar: Image::default(),
                has_avatar: false,
                status,
                kind,
            };
            ui.set_people(ModelRc::new(VecModel::from(vec![
                person("demo_dan", "Goat Simulator", 3, 0),
                person("demo_eve", "ROBLOX", 1, 0),
                person("demo_carol", &home::status_text(2), 2, 0),
                person("demo_new", "", 4, 1),
            ])));
            ui.set_selected_channel("friends".into());
            ui.set_people_page(1);
        }
    }
    // `--demo-dm-call`: in the call of the DM with demo_carol, on the call screen (Discord's voice only).
    if dm_call {
        ui.set_selected_channel("2".into());
        ui.set_channel_title("demo_carol".into());
        ui.set_channel_is_dm(true);
        ui.set_voice_channel("2".into());
        ui.set_voice_channel_name("demo_carol".into());
        ui.set_voice_status(i18n::t("Голос подключён").into());
        ui.set_voice_connected(true);
        ui.set_voice_dm(true);
        ui.set_voice_members(ModelRc::new(VecModel::from(vec![VoiceMember { discord: false, ..person("Алекс", false, false, false) }])));
        let demo_carol = DmPerson { id: "demo_carol".into(), name: "demo_carol".into(), muted: false, deafened: false, avatar: None, frame: None };
        ui.set_call_people(ModelRc::new(VecModel::from(vec![VoiceMember { speaking: true, ..demo_carol.member() }])));
        ui.set_dm_call(dm_call_line(&["ты".into(), "demo_carol".into()]).into());
        ui.set_dm_call_joined(true);
        ui.set_show_call(true);
        hide_doubles(ui);
    }
    // `--demo-call`: in voice in Main, with the call screen open.
    if std::env::args().any(|a| a == "--demo-call") {
        ui.set_voice_channel("14".into());
        ui.set_voice_channel_name("Main / Тестовый сервер".into());
        ui.set_voice_status(i18n::t("Голос подключён").into());
        ui.set_voice_connected(true);
        let me = VoiceMember { discord: false, ..person("Алекс", false, false, false) };
        // `--demo-two`: two streams on screen at once.
        let two = std::env::args().any(|a| a == "--demo-two");
        let friend = VoiceMember { speaking: true, ..person("demo_zed", false, two, false) };
        ui.set_voice_members(ModelRc::new(VecModel::from(vec![me, friend])));
        ui.set_show_call(true);
        // A made-up still preview on demo_alice's stream tile.
        let mut preview = Pixels::new(320, 180);
        for (i, px) in preview.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 320) as u32, (i / 320) as u32);
            *px = Rgba8Pixel { r: (60 + x * 120 / 320) as u8, g: (120 - y * 60 / 180) as u8, b: 70, a: 255 };
        }
        STREAM_PREVIEWS.lock().unwrap().insert("demo_alice".into(), preview.clone());
        hide_doubles(ui);
        if two {
            let mut other = Pixels::new(320, 180);
            for (i, px) in other.make_mut_slice().iter_mut().enumerate() {
                let (x, y) = ((i % 320) as u32, (i / 320) as u32);
                *px = Rgba8Pixel { r: 50, g: (70 + y * 60 / 180) as u8, b: (90 + x * 120 / 320) as u8, a: 255 };
            }
            set_stream_watched(ui, "demo_alice", true);
            set_stream_watched(ui, "demo_zed", true);
            show_stream_picture(ui, "demo_alice", Image::from_rgba8(preview.clone()));
            show_stream_picture(ui, "demo_zed", Image::from_rgba8(other));
            show_stream_quality(ui, "demo_alice", "1080P 60 FPS".into());
            show_stream_quality(ui, "demo_zed", "720P 30 FPS".into());
            focus_stream(ui, "demo_alice");
            // `--demo-grid`: all of them side by side.
            ui.set_call_grid(std::env::args().any(|a| a == "--demo-grid"));
        }
        // `--demo-update`: the button offering an update.
        if std::env::args().any(|a| a == "--demo-update") {
            ui.set_update_text(i18n::tf("Вышла версия {}. Обновить сейчас?", &[&"0.1.8"]).into());
        }
        // `--demo-taken`: the line shown when another client took our voice over.
        if std::env::args().any(|a| a == "--demo-taken") {
            ui.set_voice_taken(true);
        }
        // `--demo-click=x,y`: a click there once the window is up (checks tile focus).
        let click = std::env::args().find_map(|a| a.strip_prefix("--demo-click=").map(String::from));
        if let Some((x, y)) = click.as_deref().and_then(|c| c.split_once(',')) {
            let position = slint::LogicalPosition::new(x.parse().unwrap_or(0.0), y.parse().unwrap_or(0.0));
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(1500), move || {
                let Some(ui) = weak.upgrade() else { return };
                use slint::platform::{PointerEventButton, WindowEvent};
                let w = ui.window();
                w.dispatch_event(WindowEvent::PointerMoved { position });
                w.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                w.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            });
        }
        // `--demo-pip`: the stream keeps playing small while a chat is read.
        if std::env::args().any(|a| a == "--demo-pip") {
            ui.set_show_call(false);
            ui.set_watching(true);
            ui.set_stream_name("demo_alice · Rust".into());
            ui.set_stream_frame(Image::from_rgba8(preview));
        }
    }
    // `--demo-members`: the member list and a message search (made-up).
    if std::env::args().any(|a| a == "--demo-members") {
        let row = |kind: i32, name: &str, subtitle: &str, status: i32, color: Option<(u8, u8, u8)>| MemberItem {
            kind,
            id: name.into(),
            name: name.into(),
            subtitle: subtitle.into(),
            status,
            has_color: color.is_some(),
            color: color.map(|(r, g, b)| slint::Color::from_rgb_u8(r, g, b)).unwrap_or_default(),
            avatar: Image::default(),
            has_avatar: false,
            decoration: Image::default(),
            has_decoration: false,
            plate: Image::default(),
            has_plate: false,
        };
        ui.set_members(ModelRc::new(VecModel::from(vec![
            row(0, "Владелец сервера — 2", "", 0, None),
            row(1, "demo_user", "Roblox", 1, Some((255, 90, 120))),
            row(1, "demo_bob", "Rust", 2, Some((240, 190, 60))),
            row(0, "В сети — 3", "", 0, None),
            row(1, "spiderkaras", "Roblox with Medal", 1, Some((90, 160, 255))),
            row(1, "Llanik", "хрюкает", 3, None),
            row(0, "Не в сети — 40", "", 0, None),
            row(1, "mwnl", "", 4, None),
        ])));
        ui.set_members_open(true);
        ui.set_found_total(2);
        ui.set_found(ModelRc::new(VecModel::from(vec![
            FoundItem { channel: "1".into(), message: "1".into(), author: "demo_bob".into(), detail: "#main-chat · Вчера, 21:27".into(), text: "кто сегодня в раст? собираемся в 9 вечера".into() },
            FoundItem { channel: "1".into(), message: "2".into(), author: "Алекс".into(), detail: "#main-chat · 26.10.2026, 21:47".into(), text: "раст в субботу?".into() },
        ])));
        ui.set_found_open(true);
    }
    // `--demo-dmcall`: the line above a DM that has a call going on.
    if std::env::args().any(|a| a == "--demo-dmcall") {
        ui.set_dm_call(dm_call_line(&["Аня".into(), "Борис".into()]).into());
        // The line shows above DMs only: a group with its call button in the header.
        ui.set_channel_is_dm(true);
        ui.set_channel_title("Аня, Борис".into());
        ui.set_channel_topic("".into());
    }
    // `--demo-emoji`: the emoji panel (a few emoji and a server's, made-up).
    if std::env::args().any(|a| a == "--demo-emoji") {
        let mut items: Vec<EmojiItem> = emoji::search("серд")
            .into_iter()
            .map(|e| EmojiItem { glyph: e.into(), insert: e.into(), key: e.into(), image: Image::default(), has_image: false })
            .collect();
        let mut px = Pixels::new(48, 48);
        for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
            *p = Rgba8Pixel { r: 120, g: (i % 48 * 4) as u8, b: 200, a: 255 };
        }
        items.push(EmojiItem { glyph: "".into(), insert: "<:pog:1>".into(), key: "pog:1".into(), image: Image::from_rgba8(px), has_image: true });
        ui.set_emoji_items(ModelRc::new(VecModel::from(items)));
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_open_emoji("".into());
            }
        });
    }
    // `--demo-suggest`: the @ list above the message box (made-up names).
    if std::env::args().any(|a| a == "--demo-suggest") {
        let item = |label: &str, detail: &str| SuggestItem { label: label.into(), detail: detail.into() };
        ui.set_suggest(ModelRc::new(VecModel::from(vec![item("Алекс", "demo_user"), item("Мария", "masha_k"), item("Максим", "max")])));
        ui.set_suggest_open(true);
    }
    // `--demo-slash`: the slash command panel with made-up bots and commands (nothing is asked);
    // `--demo-slash-hint`: a picked command's options over the box instead.
    if std::env::args().any(|a| a == "--demo-slash" || a == "--demo-slash-hint") {
        let index = slash::parse_index(&demo_slash_index());
        SLASH_ICONS.with(|icons| {
            let mut icons = icons.borrow_mut();
            icons.insert("100".into(), demo_ball(64, (235, 90, 70), true));
            icons.insert("200".into(), demo_ball(64, (80, 160, 240), false));
        });
        if std::env::args().any(|a| a == "--demo-slash-hint") {
            let entry = index.entries.iter().find(|e| e.name == "play").unwrap();
            let (pills, active) = slash::hint(entry, "never gonna ");
            ui.set_slash_hint_name("/play".into());
            ui.set_slash_hint(ModelRc::new(VecModel::from(pills.into_iter().map(slint::SharedString::from).collect::<Vec<_>>())));
            ui.set_slash_hint_active(active);
            ui.invoke_set_input("/play never gonna ".into());
        } else {
            let recent: Vec<String> = ["play", "ban"].iter().filter_map(|n| index.entries.iter().find(|e| e.name == *n)).map(|e| e.key()).collect();
            let rows = slash::rows(&index, "", &recent);
            let (lines, strip, height) = slash_panel(&index, &rows);
            show_slash(ui, lines, strip, height);
            ui.invoke_set_input("/".into());
        }
    }
    // `--demo-soundboard`: in a call, with the soundboard open over the voice panel (made-up sounds, nothing asked).
    if std::env::args().any(|a| a == "--demo-soundboard") {
        ui.set_voice_channel("14".into());
        ui.set_voice_channel_name("Main / Тестовый сервер".into());
        ui.set_voice_status(i18n::t("Голос подключён").into());
        ui.set_voice_connected(true);
        let sound = |id: &str, name: &str, emoji: &str, guild: Option<&str>| soundboard::Sound {
            id: id.into(),
            name: name.into(),
            volume: 1.0,
            emoji: emoji.into(),
            emoji_id: None,
            guild: guild.map(String::from),
        };
        let server = vec![
            sound("10", "bruh", "💀", Some("1")),
            sound("11", "вжух", "✨", Some("1")),
            sound("12", "кринж", "😬", Some("1")),
            sound("13", "сюрприз", "", Some("1")),
            sound("14", "ля какой", "🤌", Some("1")),
        ];
        let defaults = vec![
            sound("1", "quack", "🦆", None),
            sound("2", "airhorn", "🔊", None),
            sound("3", "cricket", "🦗", None),
            sound("4", "golf clap", "👏", None),
            sound("5", "sad horn", "🎺", None),
            sound("6", "ba dum tss", "🥁", None),
        ];
        let sections = soundboard::sections(&server, &defaults, &["2".into(), "10".into()], "");
        let (tiles, strip, height) = sound_panel(&sections, "Тестовый сервер");
        show_sound_panel(ui, tiles, strip, height);
        ui.set_soundboard_open(true);
    }
    // `--demo-forum`: a forum channel's posts (made-up).
    if std::env::args().any(|a| a == "--demo-forum") {
        let post = |title: &str, author: &str, preview: &str, reaction: &str, reactions: i32, replies: i32, when: &str, pinned: bool| ForumPostItem {
            id: title.into(),
            title: title.into(),
            author: author.into(),
            preview: preview.into(),
            reaction: reaction.into(),
            reactions,
            replies,
            when: when.into(),
            image: Image::default(),
            has_image: false,
            pinned,
        };
        ui.set_channel_title("домашние-животные".into());
        ui.set_forum_posts(ModelRc::new(VecModel::from(vec![
            post("Правила раздела", "модератор", "Одна публикация — один питомец", "", 0, 0, "9 д. назад", true),
            post("миша", "spiderkaras", "мой кот Миша, 4 года", "❤️", 3, 14, "54 мин. назад", false),
            post("Sirius", "demo_gina", "", "❤️", 2, 8, "2 д. назад", false),
        ])));
        ui.set_forum_open(true);
        // `--demo-new-post`: the window for a new post, filled in.
        if std::env::args().any(|a| a == "--demo-new-post") {
            ui.set_thread_draft("Мой пёс Бублик".into());
            ui.set_thread_draft_text("Ему три года, любит мячики и спать на диване. Делитесь своими!".into());
            ui.set_thread_dialog(2);
        }
    }
    // `--demo-full-profile`: a person's whole profile (made up).
    if std::env::args().any(|a| a == "--demo-full-profile") {
        ui.set_user_card(demo_card());
        let row = |id: &str, name: &str| ProfileRow { id: id.into(), name: name.into(), ..Default::default() };
        ui.set_profile_extra(ProfileExtra {
            activity: "Rust".into(),
            friends_since: "10.09.2025".into(),
            friend: true,
            accounts: ModelRc::new(VecModel::from(vec![ProfileAccount { kind: "Steam".into(), name: "demo_alice".into(), url: "https://example.com".into() }])),
            friends: ModelRc::new(VecModel::from(vec![row("1", "demo_bob"), row("2", "demo_zed")])),
            servers: ModelRc::new(VecModel::from(vec![row("3", "Nimbus Dev")])),
            ..Default::default()
        });
        ui.set_full_profile_open(true);
    }
    // `--demo-badges`: the badges window of someone (made up).
    if std::env::args().any(|a| a == "--demo-badges") {
        let badge = |id: &str, text: &str| badge_item(Some(Pixels::new(32, 32)), userprofile::Badge { id: id.into(), text: text.into(), link: "https://example.com".into() });
        let mut card = demo_card();
        card.badges = ModelRc::new(VecModel::from(vec![badge("premium_tenure_12_month_v2", "Подписчик с 24.12.2024"), badge("guild_booster_lvl4", "Бустер с 19.03.2026"), badge("hypesquad_house_1", "HypeSquad")]));
        ui.set_badges_card(card);
        ui.set_badges_open(true);
    }
    // `--demo-menu`: someone's right-click menu, with a moderator's rights.
    if std::env::args().any(|a| a == "--demo-menu") {
        let targets = vec![MoveTarget { id: "1".into(), name: "Main".into() }, MoveTarget { id: "2".into(), name: "AFK".into() }];
        ui.set_user_menu(UserMenuInfo {
            id: "1".into(),
            name: "demo_alice".into(),
            me: false,
            friend: true,
            in_voice: true,
            volume: 80.0,
            can_mute: true,
            can_deafen: true,
            can_move: true,
            can_kick: true,
            can_ban: false,
            server_muted: true,
            server_deafened: false,
            targets: ModelRc::new(VecModel::from(targets)),
            note: if std::env::args().any(|a| a == "--demo-note") { "знакомы по серверу Rust, зовут Костя".into() } else { "".into() },
            in_guild: true,
            can_nick: true,
            can_roles: true,
            roles: ModelRc::new(VecModel::from(vec![RoleItem { id: "1".into(), name: "Модератор".into(), has: true, editable: true }, RoleItem { id: "2".into(), name: "Гость".into(), has: false, editable: true }])),
            invite_guilds: ModelRc::new(VecModel::from(vec![MoveTarget { id: "3".into(), name: "Nimbus Dev".into() }])),
            ..Default::default()
        });
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_show_user_menu();
            }
        });
    }
    // `--demo-settings`: the voice settings, custom profile values.
    if std::env::args().any(|a| a == "--demo-settings") {
        ui.set_voice_settings(VoiceSettings { noise: 2, noise_strength: 100, echo: true, auto_gain: false, mic_volume: 120.0, speaker_volume: 90.0, auto_threshold: false, threshold: -45.0, voice_mode: 1 });
        ui.set_mic_devices(ModelRc::new(VecModel::from(vec![DeviceItem { name: "Микрофон (USB)".into(), selected: true }])));
        ui.set_speaker_devices(ModelRc::new(VecModel::from(vec![DeviceItem { name: "Наушники".into(), selected: true }])));
        ui.set_mic_meter(0.62);
        ui.set_mic_sending(true);
        ui.set_ptt_key_name("Ctrl + Пробел".into());
        let page = std::env::args().find_map(|a| a.strip_prefix("--demo-page=").and_then(|p| p.parse().ok()));
        ui.set_settings_page(page.unwrap_or(6));
        ui.set_me_email("demo_user@example.com".into());
        ui.set_me_mfa(true);
        ui.set_priv_ready(true);
        ui.set_my_status(2);
        ui.set_my_custom_status("Пишу клиент".into());
        ui.set_my_bio("Делаю Nimbus по вечерам.".into());
        ui.set_show_game(true);
        ui.set_priv_explicit(1);
        ui.set_priv_friends_all(false);
        ui.set_priv_friends_friends(true);
        ui.set_priv_friends_guilds(true);
        ui.set_me_id("123456789012345678".into());
        ui.set_version(env!("CARGO_PKG_VERSION").into());
        ui.set_settings_open(true);
    }
    // `--demo-profile`, `--demo-profile-status`, `--demo-profile-edit`: the profile card over the user panel
    // (with the status list opened), and the profile editor; made-up names and pictures only.
    let demo_profile = std::env::args().any(|a| a == "--demo-profile" || a == "--demo-profile-status" || a == "--demo-profile-edit");
    if demo_profile {
        let mut banner = SharedPixelBuffer::<slint::Rgba8Pixel>::new(480, 192);
        for (i, p) in banner.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 480) as u32, (i / 480) as u32);
            *p = slint::Rgba8Pixel { r: (40 + y / 3) as u8, g: (90 + x / 6) as u8, b: (180 - x / 8) as u8, a: 255 };
        }
        let mut avatar = SharedPixelBuffer::<slint::Rgba8Pixel>::new(128, 128);
        for (i, p) in avatar.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 128) as u32, (i / 128) as u32);
            *p = slint::Rgba8Pixel { r: (230 - y) as u8, g: (150 + x / 3) as u8, b: 90, a: 255 };
        }
        ui.set_me_avatar(Image::from_rgba8(avatar));
        ui.set_prof_banner(Image::from_rgba8(banner));
        ui.set_prof_has_banner(true);
        ui.set_me_username("demo_user".into());
        ui.set_me_name("Алекс".into());
        ui.set_prof_global_name("Алекс".into());
        ui.set_prof_pronouns("они/их".into());
        ui.set_my_bio("Делаю клиент по вечерам.\nПишите, если нужна помощь.".into());
        ui.set_me_id("123456789012345678".into());
        ui.set_my_status(0);
        ui.set_priv_ready(true);
        if std::env::args().any(|a| a == "--demo-profile-edit") {
            ui.set_profile_edit_open(true);
        } else {
            let weak = ui.as_weak();
            let with_status = std::env::args().any(|a| a == "--demo-profile-status");
            slint::Timer::single_shot(std::time::Duration::from_millis(800), move || {
                if let Some(ui) = weak.upgrade() {
                    if with_status {
                        ui.invoke_open_profile_status();
                    }
                    ui.invoke_show_profile_popup();
                }
            });
        }
    }
    // `--demo-camera`: in voice in Main with the call screen open and cameras on: ours (mirrored) and two
    // others', from invented frames (no camera is opened), one person with just an avatar, the camera button on.
    // With `--demo-settings` the settings' camera section instead, its preview on.
    if std::env::args().any(|a| a == "--demo-camera") {
        demo_camera(ui, &person);
    }
    // `--demo-theme=N`: the window in that theme (0 black, 1 graphite, 2 midnight, 3 light), whatever is saved.
    if let Some(theme) = std::env::args().find_map(|a| a.strip_prefix("--demo-theme=").and_then(|n| n.parse::<i32>().ok())) {
        ui.global::<Appearance>().set_theme(theme.clamp(0, 3));
        names::set_light(theme == 3);
    }
    // `--demo --snapshot=<file.png>`: draws the demo window to a picture and quits (no screen capture is involved).
    if std::env::args().any(|a| a == "--demo") {
        // NIMBUS_DEMO_AT="x,y": where the demo window opens (kept off the main monitor).
        if let Some((x, y)) = std::env::var("NIMBUS_DEMO_AT").ok().as_deref().and_then(demo_position) {
            ui.window().set_position(slint::PhysicalPosition::new(x, y));
        }
        if let Some(path) = std::env::args().find_map(|a| a.strip_prefix("--snapshot=").map(str::to_string)) {
            ui.window().set_size(slint::PhysicalSize::new(1400, 950));
            let weak = ui.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_secs(4), move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(buf) = ui.window().take_snapshot() {
                        let _ = image::save_buffer(&path, buf.as_bytes(), buf.width(), buf.height(), image::ColorType::Rgba8);
                    }
                    let _ = slint::quit_event_loop();
                }
            });
        }
    }
}

/// `--demo-camera`: made-up camera pictures through the same conversion as real frames.
fn demo_camera(ui: &AppWindow, person: &dyn Fn(&str, bool, bool, bool) -> VoiceMember) {
    let picture = |back: (u8, u8, u8), mark: (u8, u8, u8), phase: u32, mirror: bool| {
        let (w, h) = (1280, 720);
        let nv12 = camera::test_frame(w, h, phase, back, mark);
        let (pw, ph) = camera::preview_size(w, h, 640);
        let mut pixels = Pixels::new(pw, ph);
        pixels.make_mut_bytes().copy_from_slice(&camera::nv12_to_rgba(&nv12, w, h, pw, ph, mirror));
        pixels
    };
    // Ours is mirrored: its orange mark, near the left of the frame, shows on the right.
    let ours = picture((40, 70, 150), (240, 140, 40), 0, true);
    {
        let mut frames = CAMERA_FRAMES.lock().unwrap();
        frames.insert("Алекс".into(), ours.clone());
        frames.insert("demo_zed".into(), picture((40, 120, 80), (150, 90, 230), 30, false));
        frames.insert("demo_alice".into(), picture((120, 60, 70), (60, 200, 220), 60, false));
    }
    let cameras = vec![DeviceItem { name: "Встроенная камера".into(), selected: true }, DeviceItem { name: "USB-камера".into(), selected: false }];
    ui.set_camera_devices(ModelRc::new(VecModel::from(cameras)));
    if ui.get_settings_open() {
        let mut settings = ui.get_voice_settings();
        settings.voice_mode = 0;
        ui.set_voice_settings(settings);
        ui.set_camera_preview_on(true);
        ui.set_camera_preview_frame(Image::from_rgba8(ours));
        // The camera section is further down the page: scroll to it once the window is up.
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(1500), move || {
            let Some(ui) = weak.upgrade() else { return };
            let position = slint::LogicalPosition::new(800.0, 500.0);
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled { position, delta_x: 0.0, delta_y: -560.0 });
        });
        return;
    }
    ui.set_voice_channel("14".into());
    ui.set_voice_channel_name("Main / Тестовый сервер".into());
    ui.set_voice_status(i18n::t("Голос подключён").into());
    ui.set_voice_connected(true);
    let me = VoiceMember { discord: false, ..person("Алекс", false, false, false) };
    let friend = VoiceMember { speaking: true, ..person("demo_zed", false, false, false) };
    ui.set_voice_members(ModelRc::new(VecModel::from(vec![me, friend])));
    ui.set_camera_on(true);
    ui.set_show_call(true);
    hide_doubles(ui);
}

/// A round made-up picture: a soft ball of colour, with two eyes when `face`.
fn demo_ball(size: u32, (r, g, b): (u8, u8, u8), face: bool) -> Image {
    let mut px = Pixels::new(size, size);
    let half = size as f32 / 2.0;
    for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32 - half, (i as u32 / size) as f32 - half);
        let d = (x * x + y * y).sqrt() / half;
        let eye = face && ((x.abs() - half * 0.3).powi(2) + (y + half * 0.2).powi(2)).sqrt() < half * 0.12;
        let light = (1.2 - d * 0.5).clamp(0.0, 1.0);
        *p = if d > 0.95 {
            Rgba8Pixel { r: 0, g: 0, b: 0, a: 0 }
        } else if eye {
            Rgba8Pixel { r: 20, g: 20, b: 30, a: 255 }
        } else {
            Rgba8Pixel { r: (r as f32 * light) as u8, g: (g as f32 * light) as u8, b: (b as f32 * light) as u8, a: 255 }
        };
    }
    Image::from_rgba8(px)
}

/// `--demo-media`: a sticker, a sticker that is only named, a poll, a message of emoji only with an
/// animated reaction, and a voice message; everything made up.
fn demo_media() -> Vec<MessageItem> {
    use base64::Engine as _;
    // Made-up server emoji; the second one moves.
    EMOJIS.with(|e| {
        let mut e = e.borrow_mut();
        e.insert("9001".into(), demo_ball(48, (240, 190, 60), true));
        e.insert("9002".into(), demo_ball(48, (124, 108, 246), true));
        e.insert("9003".into(), demo_ball(48, (90, 200, 140), false));
    });
    let frames: Vec<(Image, u32)> = [(124, 108, 246), (246, 108, 180), (108, 200, 246), (246, 200, 108)].into_iter().map(|c| (demo_ball(48, c, true), 150)).collect();
    EMOJI_ANIMATIONS.with(|a| a.borrow_mut().insert("9002".into(), EmojiAnimation { frames, current: 0, next: Instant::now() }));

    let base = |id: &str, author: &str, time: &str, text: &str| Msg {
        id: id.into(),
        author_id: author.into(),
        author: author.into(),
        time: time.into(),
        short_time: "21:40".into(),
        blocks: blocks_of(text),
        grouped: false,
        system: false,
        system_line: None,
        picture_links: PictureLinks::default(),
        picture: None,
        edited: false,
        reactions: Vec::new(),
        highlight: false,
        color: None,
        cards: Vec::new(),
        mention: false,
        pinned: false,
        emojis: Vec::new(),
        sticker: None,
        poll: None,
        voice: None,
        forwarded: false,
        invites: Vec::new(),
        components: Vec::new(),
        target: components::Target::default(),
        ephemeral: false,
    };
    let sticker = Msg { sticker: Some(Sticker { name: "Привет".into(), url: Some("demo".into()), animated: false }), ..base("m1", "demo_alice", "Сегодня, 21:40", "смотри какой стикер") };
    let lottie = Msg { sticker: Some(Sticker { name: "Вампус машет".into(), url: None, animated: false }), ..base("m2", "demo_bob", "Сегодня, 21:41", "") };
    let mut demo_poll = poll::parse(&json!({ "poll": {
        "question": { "text": "Во что играем в субботу?" },
        "answers": [
            { "answer_id": 1, "poll_media": { "text": "Rust", "emoji": { "name": "🦀" } } },
            { "answer_id": 2, "poll_media": { "text": "Roblox", "emoji": { "id": "9001", "name": "pog" } } },
            { "answer_id": 3, "poll_media": { "text": "Просто поболтать", "emoji": { "name": "💬" } } },
        ],
        "allow_multiselect": false,
        "results": { "is_finalized": false, "answer_counts": [{ "id": 1, "count": 5, "me_voted": true }, { "id": 2, "count": 2, "me_voted": false }, { "id": 3, "count": 1, "me_voted": false }] },
    } }));
    if let Some(p) = demo_poll.as_mut() {
        p.expiry_ms = Some(Local::now().timestamp_millis() + 26 * 3600 * 1000);
    }
    let poll_msg = Msg { poll: demo_poll, ..base("m3", "Алекс", "Сегодня, 21:43", "") };
    let reaction = |key: &str, image: Option<&str>, count: u32, me: bool| chat::Reaction { key: key.into(), text: key.into(), image: image.map(String::from), count, me };
    let emojis = Msg {
        emojis: chat::jumbo("<:pog:9001> <a:dance:9002> 🔥 <:cat:9003>").unwrap_or_default(),
        reactions: vec![reaction("dance:9002", Some("demo"), 3, true), reaction("👍", None, 1, false)],
        ..base("m4", "demo_zed", "Сегодня, 21:44", "")
    };
    let wave: Vec<u8> = (0..64u32).map(|i| (60.0 + 150.0 * ((i as f32 * 0.45).sin().abs() * (1.0 - i as f32 / 90.0))) as u8).collect();
    let clip = voicemsg::Clip { url: "demo".into(), secs: 12.4, bars: voicemsg::bars(&base64::engine::general_purpose::STANDARD.encode(wave), voicemsg::BARS) };
    let voice = Msg { voice: Some(clip), ..base("m5", "demo_bob", "Сегодня, 21:46", "") };

    let mut items: Vec<MessageItem> = [sticker, lottie, poll_msg, emojis, voice].iter().map(Msg::item).collect();
    items[0].sticker = demo_ball(160, (255, 140, 90), true);
    // The voice message is a third of the way in.
    items[4].voice.playing = true;
    items[4].voice.progress = 0.35;
    items[4].voice.elapsed = voicemsg::clock(4.3).into();
    items
}

/// `--demo-components`: a made-up bot message with an embed, a row of buttons in every style and a select menu,
/// and an ephemeral answer; read the way real ones are. With `open`, the menu opens by itself.
fn demo_components(ui: &AppWindow, open: bool) -> Vec<MessageItem> {
    let m = json!({
        "id": "9300", "channel_id": "12", "guild_id": "1", "type": 0, "flags": 0, "application_id": "9301",
        "content": "Набор на субботний рейд открыт!",
        "timestamp": "2026-11-01T19:10:00+00:00",
        "author": { "id": "9301", "username": "Nimbus Bot", "bot": true },
        "embeds": [{ "type": "rich", "title": "Рейд «Чёрная крепость»", "description": "Суббота, 20:00 МСК. Нужны 2 танка, 3 лекаря и 5 бойцов. Отметьтесь кнопками ниже.", "color": 0x5865f2 }],
        "components": [
            { "type": 1, "components": [
                { "type": 2, "style": 1, "label": "Записаться", "custom_id": "join" },
                { "type": 2, "style": 2, "label": "Может быть", "custom_id": "maybe" },
                { "type": 2, "style": 3, "label": "Готов", "emoji": { "name": "✅" }, "custom_id": "ready" },
                { "type": 2, "style": 4, "label": "Отменить рейд", "custom_id": "cancel", "disabled": true },
                { "type": 2, "style": 5, "label": "Гайд", "url": "https://example.com/guide" },
            ] },
            { "type": 1, "components": [
                { "type": 3, "custom_id": "role", "placeholder": "Выберите роль", "min_values": 1, "max_values": 2, "options": [
                    { "label": "Танк", "value": "tank", "description": "Держит босса на себе", "emoji": { "name": "🛡️" } },
                    { "label": "Лекарь", "value": "heal", "description": "Лечит группу", "emoji": { "name": "💚" }, "default": true },
                    { "label": "Боец", "value": "dps", "description": "Наносит урон", "emoji": { "name": "⚔️" } },
                    { "label": "Запасной", "value": "bench", "description": "Если кто-то не придёт", "emoji": { "name": "🪑" } },
                ] },
            ] },
        ],
    });
    let ephemeral = json!({
        "id": "9302", "channel_id": "12", "guild_id": "1", "type": 20, "flags": 64, "application_id": "9301",
        "content": "Вы записаны как **Лекарь**. Напоминание придёт за час до начала.",
        "timestamp": "2026-11-01T19:11:00+00:00",
        "author": { "id": "9301", "username": "Nimbus Bot", "bot": true },
        "components": [{ "type": 1, "components": [{ "type": 2, "style": 2, "label": "Выписаться", "emoji": { "name": "🚪" }, "custom_id": "leave" }] }],
    });
    let msg = to_msg(&m, &mut None, &mut Vec::new());
    if open {
        if let Some(select) = msg.components.iter().flatten().find(|c| c.is_select()) {
            let chosen: Vec<String> = select.options.iter().filter(|o| o.default).map(|o| o.value.clone()).collect();
            ui.set_select_options(ModelRc::new(VecModel::from(select_option_items(select, &chosen))));
            ui.set_select_multi(select.max > 1);
            ui.set_select_max(select.max as i32);
            ui.set_select_tall(true);
        }
    }
    let first = msg.item();
    if open {
        if let Some(items) = first.components.row_data(1).map(|r| r.items) {
            update_row(items, 0, |c| c.demo_open = true);
        }
    } else if let Some(items) = first.components.row_data(0).map(|r| r.items) {
        // One button waits for the bot's answer.
        update_row(items, 0, |c| c.pending = true);
    }
    vec![first, to_msg(&ephemeral, &mut None, &mut Vec::new()).item()]
}

/// `--demo-events`: two made-up events read the way Discord's list is; with `create`, step 2 of a new one.
fn demo_events(ui: &AppWindow, create: bool) {
    let now = chrono::Utc::now();
    let iso = |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339();
    let tomorrow = (Local::now() + chrono::Duration::days(1)).date_naive().and_hms_opt(19, 0, 0).and_then(|t| t.and_local_timezone(Local).earliest()).map(|t| t.with_timezone(&chrono::Utc)).unwrap_or(now);
    let list = events::parse_list(&json!([
        { "id": "8001", "guild_id": "1", "name": "Вечерний подкаст: итоги недели", "description": "Обсуждаем новости сервера, отвечаем на вопросы из #вопросы и разыгрываем роль «Слушатель месяца».",
          "scheduled_start_time": iso(now - chrono::Duration::minutes(25)), "status": 2, "entity_type": 1, "channel_id": "8101",
          "creator_id": "77", "creator": { "id": "77", "username": "demo_alice" }, "user_count": 14 },
        { "id": "8002", "guild_id": "1", "name": "Турнир по Rust: 5 на 5", "description": "Собираемся у входа в парк, оттуда вместе в компьютерный клуб. Берите свои мышки!",
          "scheduled_start_time": iso(tomorrow), "scheduled_end_time": iso(tomorrow + chrono::Duration::hours(3)), "status": 1, "entity_type": 3,
          "entity_metadata": { "location": "Клуб «Респаун», ул. Ленина, 12" }, "creator_id": "0", "creator": { "id": "78", "username": "Алекс" }, "user_count": 6 },
    ]));
    let today = Local::now().date_naive();
    let items: Vec<EventItem> = list
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let when = DateTime::from_timestamp_millis(e.start_ms).map(|t| events::when(t.with_timezone(&Local).naive_local(), today)).unwrap_or_default();
            let place = if e.entity == events::STAGE { "Подкаст-трибуна".to_string() } else { e.location.clone() };
            let avatar = (i == 0).then(|| {
                let mut px = Pixels::new(40, 40);
                for (k, p) in px.make_mut_slice().iter_mut().enumerate() {
                    let (x, y) = ((k % 40) as u32, (k / 40) as u32);
                    *p = Rgba8Pixel { r: (200 + x) as u8, g: (90 + y * 2) as u8, b: 60, a: 255 };
                }
                px
            });
            event_item(e, when, place, i == 1, avatar, "0")
        })
        .collect();
    ui.set_events_title(events::count_title(items.len()).into());
    ui.set_events(ModelRc::new(VecModel::from(items)));
    ui.set_events_can_create(true);
    ui.set_events_open(true);
    if create {
        let start = events::default_start(Local::now().naive_local());
        let (date, time) = events::date_field(start);
        ui.set_event_kind(2);
        ui.set_event_channel("8102".into());
        ui.set_event_channels(ModelRc::new(VecModel::from(vec![
            EventChannelItem { id: "8101".into(), name: "Подкаст-трибуна".into(), stage: true },
            EventChannelItem { id: "8102".into(), name: "Общий голосовой".into(), stage: false },
        ])));
        ui.set_event_name("Киновечер: «Интерстеллар»".into());
        ui.set_event_date(date.into());
        ui.set_event_time(time.into());
        ui.set_event_desc("Смотрим вместе через демонстрацию экрана. Попкорн — свой.".into());
        ui.set_event_step(2);
        // `--demo-events-step=1` or `=3`: the first step, or the preview, instead.
        match std::env::args().find_map(|a| a.strip_prefix("--demo-events-step=").and_then(|n| n.parse::<i32>().ok())) {
            Some(1) => ui.set_event_step(1),
            Some(3) => event_preview(ui),
            _ => {}
        }
    }
}

/// `--demo-slash`: two made-up bots and their commands, as `application-command-index` brings them.
fn demo_slash_index() -> Value {
    json!({
        "applications": [{ "id": "100", "name": "Jockie Music", "icon": "x" }, { "id": "200", "name": "Модератор", "icon": "y" }],
        "application_commands": [
            { "id": "1", "application_id": "100", "version": "1", "type": 1, "name": "play", "description": "Включить трек по ссылке или названию",
              "options": [{ "type": 3, "name": "ссылка", "description": "ссылка или название", "required": true }, { "type": 4, "name": "громкость" }] },
            { "id": "2", "application_id": "100", "version": "1", "type": 1, "name": "skip", "description": "Пропустить текущий трек" },
            { "id": "3", "application_id": "100", "version": "1", "type": 1, "name": "queue", "description": "Показать очередь" },
            { "id": "4", "application_id": "100", "version": "1", "type": 1, "name": "loop", "description": "Повторять трек или очередь",
              "options": [{ "type": 3, "name": "режим", "required": true, "choices": [{ "name": "трек", "value": "track" }] }] },
            { "id": "5", "application_id": "200", "version": "1", "type": 1, "name": "ban", "description": "Забанить участника",
              "options": [{ "type": 6, "name": "участник", "required": true }, { "type": 3, "name": "причина" }] },
            { "id": "6", "application_id": "200", "version": "1", "type": 1, "name": "warn", "description": "Выдать предупреждение",
              "options": [{ "type": 6, "name": "участник", "required": true }] },
        ],
    })
}

/// The start window in the middle of the main monitor.
fn center_splash(splash: &SplashWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let (screen_w, screen_h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let scale = splash.window().scale_factor();
    let size = ((splash.get_width_px() * scale) as i32, (splash.get_height_px() * scale) as i32);
    let (x, y) = centered((screen_w, screen_h), size);
    splash.window().set_position(slint::PhysicalPosition::new(x, y));
}

/// Where a window of `size` goes to sit in the middle of a screen.
fn centered(screen: (i32, i32), size: (i32, i32)) -> (i32, i32) {
    ((screen.0 - size.0) / 2, (screen.1 - size.1) / 2)
}

/// `--demo --demo-splash=N`: the start window in one of its phases, with made-up numbers; no update is looked for.
fn demo_splash(phase: i32) -> Result<(), slint::PlatformError> {
    let splash = SplashWindow::new()?;
    // A negative phase: the real check runs (it asks only the update address), then the window closes and no app opens.
    if phase < 0 {
        let weak = splash.as_weak();
        let skip = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::spawn(move || {
            let started = Instant::now();
            tokio::runtime::Runtime::new().expect("failed to start the tokio runtime").block_on(update::at_start(weak, skip));
            log_line(&format!("start window: done in {} ms", started.elapsed().as_millis()));
            let _ = slint::quit_event_loop();
        });
        return splash.run();
    }
    splash.set_phase(phase);
    splash.set_progress(0.46);
    splash.set_detail(
        match phase {
            1 => i18n::tf("Версия {}, {} из {} МБ", &[&"0.1.7", &16, &34]),
            2 => i18n::t("Nimbus перезапустится сам"),
            3 => String::new(),
            4 => i18n::t("Запускаю текущую версию"),
            _ => i18n::tf("Версия {}", &[&update::VERSION]),
        }
        .into(),
    );
    if let Some((x, y)) = std::env::var("NIMBUS_DEMO_AT").ok().as_deref().and_then(demo_position) {
        splash.window().set_position(slint::PhysicalPosition::new(x, y));
    }
    let weak = splash.as_weak();
    let path = std::env::args().find_map(|a| a.strip_prefix("--snapshot=").map(str::to_string));
    if let Some(path) = path {
        slint::Timer::single_shot(Duration::from_millis(1500), move || {
            if let Some(splash) = weak.upgrade() {
                if let Ok(buf) = splash.window().take_snapshot() {
                    let _ = image::save_buffer(&path, buf.as_bytes(), buf.width(), buf.height(), image::ColorType::Rgba8);
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    splash.run()
}

/// A made-up picture: a soft diagonal blend, for demo windows.
fn demo_gradient(w: u32, h: u32) -> Image {
    let mut px = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % w) as f32 / w as f32, (i as u32 / w) as f32 / h as f32);
        *p = Rgba8Pixel { r: (40.0 + 150.0 * x) as u8, g: (70.0 + 60.0 * y) as u8, b: (190.0 - 70.0 * x) as u8, a: 255 };
    }
    Image::from_rgba8(px)
}

/// `--demo-mentions`: a made-up reply with mentions of people, roles and a channel, read the way a real one is.
fn demo_mentions() -> MessageItem {
    names::set_light(std::env::args().any(|a| a == "--demo-theme=3"));
    names::note_roles(&[json!({"id": "501", "name": "Модераторы", "color": 0xe67e22}), json!({"id": "502", "name": "Игроки", "color": 0x3498db}), json!({"id": "503", "name": "все-тестеры", "color": 0})]);
    names::note_channels(&[json!({"id": "601", "name": "объявления"})]);
    let m = json!({
        "id": "9200",
        "type": 19,
        "content": "## Техработы\nПеренесли на 02.11.2026 06:30, подробности в <#601>.\n<@&501> <@&502> <@&503> спасибо <@77>!",
        "timestamp": "2026-11-01T19:05:00+00:00",
        "author": { "id": "78", "username": "nimbus-bot" },
        "mentions": [{ "id": "77", "username": "spiderkaras", "global_name": "Паук" }],
        "referenced_message": { "content": "<@&501> <@&502> когда работы?", "author": { "id": "77", "username": "spiderkaras" }, "mentions": [] },
    });
    to_msg(&m, &mut None, &mut Vec::new()).item()
}

/// `--demo-forwarded`: a made-up forwarded message, through the same reading as a real one.
fn demo_forwarded() -> MessageItem {
    let m = json!({
        "id": "9100",
        "type": 0,
        "content": "",
        "timestamp": "2026-11-01T19:05:00+00:00",
        "author": { "id": "77", "username": "spiderkaras" },
        "message_reference": { "type": 1, "channel_id": "5", "message_id": "6", "guild_id": "2" },
        "message_snapshots": [{ "message": {
            "content": "завтра в 20:00 общий созвон по релизу\nкто не может — отпишитесь в треде",
            "timestamp": "2026-11-01T18:40:00+00:00",
            "attachments": [],
            "embeds": [],
        } }],
    });
    to_msg(&m, &mut None, &mut Vec::new()).item()
}

/// `--demo-invite-card`: two made-up messages with invite links, read the way real ones are; the servers
/// they lead to are made up and put where Discord's answers would go (no request is made).
fn demo_invites() -> Vec<MessageItem> {
    let server = |code: &str, id: &str, name: &str, members: u64, online: u64| invite::Preview {
        code: code.into(),
        guild_id: id.into(),
        name: name.into(),
        icon: None,
        members: Some(members),
        online: Some(online),
        channel_id: String::new(),
        channel_type: 0,
    };
    let icon = |c: (u8, u8, u8)| {
        let mut px = Pixels::new(96, 96);
        for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 96) as u32, (i / 96) as u32);
            *p = Rgba8Pixel { r: c.0.saturating_add((x / 3) as u8), g: c.1.saturating_add((y / 4) as u8), b: c.2, a: 255 };
        }
        px
    };
    {
        let mut seen = INVITES.lock().unwrap();
        seen.insert("nimbus-dev".into(), SeenInvite { done: true, preview: Some(server("nimbus-dev", "9001", "Nimbus Dev", 340, 12)), icon: Some(icon((90, 70, 200))), joined: false });
        seen.insert("rust-ru".into(), SeenInvite { done: true, preview: Some(server("rust-ru", "3", "Rust", 1820, 214)), icon: None, joined: true });
        seen.insert("old-party".into(), SeenInvite { done: true, preview: None, icon: None, joined: false });
    }
    let message = |id: &str, author: &str, text: &str| {
        let m = json!({ "id": id, "type": 0, "content": text, "timestamp": "2026-11-01T19:30:00+00:00", "author": { "id": author, "username": author } });
        to_msg(&m, &mut None, &mut Vec::new()).item()
    };
    vec![
        message("9200", "spiderkaras", "заходите к нам: https://discord.gg/nimbus-dev"),
        message("9201", "demo_bob", "а вот старое: discord.gg/old-party и наш discord.com/invite/rust-ru"),
    ]
}

/// `--demo-gifs`: made-up tiles in the GIF panel; the first one moves, the last two are categories.
fn show_demo_gifs(ui: &AppWindow) {
    let tile = |(r, g, b): (u8, u8, u8), shift: u32| {
        let (w, h) = (gifs::TILE_W, gifs::TILE_H);
        let mut px = Pixels::new(w, h);
        for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            // A soft band that moves along with `shift`.
            let wave = ((((x + shift) as f32 / w as f32) * std::f32::consts::TAU).sin() * 40.0 + 40.0) as u8;
            *p = Rgba8Pixel { r: r.saturating_add(wave), g: (g as u32 * (h + y) / (2 * h)) as u8, b: b.saturating_sub(wave / 2), a: 255 };
        }
        Image::from_rgba8(px)
    };
    let colors = [(200, 80, 120), (60, 140, 220), (90, 190, 120), (230, 160, 60), (130, 90, 220), (40, 170, 170), (220, 100, 60), (100, 120, 160), (180, 60, 180)];
    let mut items: Vec<GifItem> = colors
        .iter()
        .enumerate()
        .map(|(i, c)| GifItem { key: format!("demo{i}").into(), url: format!("https://tenor.com/view/demo-{i}").into(), label: "".into(), category: false, image: tile(*c, 0), has_image: true })
        .collect();
    for (name, c) in [("привет", (70, 70, 110)), ("танцы", (110, 60, 90))] {
        items.push(GifItem { key: format!("category:{name}").into(), url: "".into(), label: name.into(), category: true, image: tile(c, 0), has_image: true });
    }
    let frames: Vec<(Image, u32)> = (0..6).map(|f| (tile(colors[0], f * 25), 120)).collect();
    GIF_TILES.with(|t| t.borrow_mut().push(TileAnimation { key: "demo0".into(), frames, current: 0, next: Instant::now() }));
    ui.set_gif_items(ModelRc::new(VecModel::from(items)));
    ui.set_gif_busy(false);
}

/// "x,y" (whole numbers, may be negative) as a window position; None if it isn't that.
fn demo_position(text: &str) -> Option<(i32, i32)> {
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Logical pixels per mouse-wheel notch. Slint's default step is small next
/// to what Windows apps usually scroll.
const WHEEL_STEP_PX: f32 = 140.0;
/// Touchpads report exact pixels; a little extra makes them feel less sluggish.
const TOUCHPAD_BOOST: f32 = 1.5;

/// Replaces wheel events with bigger steps, for every scrollable list.
/// The Latin letter of an editing shortcut pressed on a layout that types something else there
/// (None when the key already gives the Latin letter, which Slint handles itself).
fn latin_shortcut(event: &slint::winit_030::winit::event::KeyEvent) -> Option<char> {
    use slint::winit_030::winit::keyboard::{Key, KeyCode, PhysicalKey};
    let letter = match event.physical_key {
        PhysicalKey::Code(KeyCode::KeyA) => 'a',
        PhysicalKey::Code(KeyCode::KeyC) => 'c',
        PhysicalKey::Code(KeyCode::KeyV) => 'v',
        PhysicalKey::Code(KeyCode::KeyX) => 'x',
        PhysicalKey::Code(KeyCode::KeyY) => 'y',
        PhysicalKey::Code(KeyCode::KeyZ) => 'z',
        _ => return None,
    };
    match &event.logical_key {
        Key::Character(typed) if typed.eq_ignore_ascii_case(&letter.to_string()) => None,
        _ => Some(letter),
    }
}

fn speed_up_scrolling(ui: &AppWindow) {
    use slint::winit_030::winit::event::{ElementState, MouseScrollDelta, WindowEvent};
    use slint::winit_030::winit::keyboard::{KeyCode, PhysicalKey};
    use slint::winit_030::{EventResult, WinitWindowAccessor};

    let weak = ui.as_weak();
    let mut cursor = (0.0f32, 0.0f32);
    let mut ctrl = false;
    ui.window().on_winit_window_event(move |window, event| match event {
        // Ctrl+K opens "Find or start a conversation", whatever the keyboard layout.
        WindowEvent::ModifiersChanged(modifiers) => {
            ctrl = modifiers.state().control_key();
            EventResult::Propagate
        }
        // Esc leaves the full-screen call, F11 enters or leaves it.
        WindowEvent::KeyboardInput { event, .. }
            if event.state == ElementState::Pressed
                && matches!(
                    event.physical_key,
                    PhysicalKey::Code(KeyCode::Escape | KeyCode::F11)
                ) =>
        {
            let escape = event.physical_key == PhysicalKey::Code(KeyCode::Escape);
            // Esc closes the popups first; only the call screen is ours to close.
            let ours = weak.upgrade().is_some_and(|ui| ui.get_call_fullscreen())
                || (!escape && weak.upgrade().is_some_and(|ui| ui.get_show_call()));
            if !ours {
                return EventResult::Propagate;
            }
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    let on = if escape { false } else { !ui.get_call_fullscreen() };
                    ui.set_call_fullscreen(on);
                    ui.invoke_fullscreen_changed(on);
                }
            });
            EventResult::PreventDefault
        }
        // Ctrl+V in the message box with a picture or files on the clipboard: they are attached instead of
        // typed (text still pastes as text).
        WindowEvent::KeyboardInput { event, .. }
            if ctrl
                && event.state == ElementState::Pressed
                && !event.repeat
                && event.physical_key == PhysicalKey::Code(KeyCode::KeyV)
                && weak.upgrade().is_some_and(|ui| ui.get_input_focused())
                && clipboard::has_attachments() =>
        {
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_paste_clipboard();
                }
            });
            EventResult::PreventDefault
        }
        // Ctrl+C, V, X, A, Z, Y by the key's place: Slint knows these shortcuts only by the Latin letter,
        // so on another layout (Ctrl+С on a Russian one) copying and pasting did nothing.
        WindowEvent::KeyboardInput { event, .. } if ctrl && latin_shortcut(event).is_some() => {
            let letter = latin_shortcut(event).unwrap_or('c');
            let pressed = event.state == ElementState::Pressed;
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    let text: slint::SharedString = letter.to_string().into();
                    ui.window().dispatch_event(if pressed {
                        slint::platform::WindowEvent::KeyPressed { text }
                    } else {
                        slint::platform::WindowEvent::KeyReleased { text }
                    });
                }
            });
            EventResult::PreventDefault
        }
        WindowEvent::KeyboardInput { event, .. }
            if ctrl
                && event.state == slint::winit_030::winit::event::ElementState::Pressed
                && event.physical_key == slint::winit_030::winit::keyboard::PhysicalKey::Code(slint::winit_030::winit::keyboard::KeyCode::KeyK) =>
        {
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_open_search();
                }
            });
            EventResult::PreventDefault
        }
        WindowEvent::CursorMoved { position, .. } => {
            let scale = window.scale_factor();
            cursor = (position.x as f32 / scale, position.y as f32 / scale);
            EventResult::Propagate
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (x * WHEEL_STEP_PX, y * WHEEL_STEP_PX),
                MouseScrollDelta::PixelDelta(p) => {
                    let scale = window.scale_factor() as f64;
                    ((p.x / scale) as f32 * TOUCHPAD_BOOST, (p.y / scale) as f32 * TOUCHPAD_BOOST)
                }
            };
            let (x, y) = cursor;
            // Dispatch on the next turn of the event loop, not from inside this hook.
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(x, y),
                        delta_x: dx,
                        delta_y: dy,
                    });
                }
            });
            EventResult::PreventDefault
        }
        _ => EventResult::Propagate,
    });
}

fn main() -> Result<(), slint::PlatformError> {
    if std::env::args().any(|a| a == "--video-check") {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
        let report = runtime.block_on(voice::video_check(DEFAULT_SERVER));
        log_line(&report);
        return Ok(());
    }
    if std::env::args().any(|a| a == "--discord-mic-check") {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
        let report = runtime.block_on(discord_voice::mic_check());
        log_line(&report);
        return Ok(());
    }
    if std::env::args().any(|a| a == "--voice-check") {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
        let report = runtime.block_on(voice::self_check(DEFAULT_SERVER));
        log_line(&report);
        return Ok(());
    }
    if let Some(arg) = std::env::args().find(|a| a == "--net-check" || a.starts_with("--net-check=")) {
        let proxy = arg.strip_prefix("--net-check=").map(str::to_string);
        let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
        let report = runtime.block_on(netcheck::report(proxy));
        log_line(&report);
        return Ok(());
    }
    // One running copy; the self-checks and demo windows (which have flags) may run beside it.
    if std::env::args().len() == 1 && !system::first_instance() {
        return Ok(());
    }
    let ui = AppWindow::new()?;
    setup_language(&ui);
    setup_changelog(&ui);
    setup_rail(&ui);
    // Watches the push-to-talk key in the background: Windows gives key
    // presses only to the focused window, but the key must work anywhere.
    audio::watch_ptt();
    speed_up_scrolling(&ui);
    ui.global::<Fmt>().on_stamps(|text| clock::discord_stamps(&text).into());

    // Drives animated GIFs; only rows with a due frame are touched.
    let gif_timer = slint::Timer::default();
    let weak = ui.as_weak();
    gif_timer.start(slint::TimerMode::Repeated, Duration::from_millis(20), move || {
        if let Some(ui) = weak.upgrade() {
            tick_animations(&ui);
        }
    });

    // The mic meter in the voice settings, while they're open, and the state
    // of the push-to-talk key (the mic icon follows it).
    let meter_timer = slint::Timer::default();
    let weak = ui.as_weak();
    meter_timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_ptt_open(audio::mic_open());
        if ui.get_settings_open() {
            let (db, sending) = audio::meter();
            ui.set_mic_meter(((db + 100.0) / 100.0).clamp(0.0, 1.0));
            ui.set_mic_sending(sending);
        }
    });

    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_members_shown(prefs::read()["members_shown"].as_bool().unwrap_or(true));
    ui.on_members_toggled(|shown| prefs::write("members_shown", json!(shown)));
    // How a screen share starts, as last chosen.
    let share = prefs::Share::from_json(&prefs::read()["share"]);
    ui.set_share_height(share.height);
    ui.set_share_fps(share.fps);
    ui.set_share_sound(share.sound);
    ui.set_share_dest(share.dest);
    // Looks and reading settings, as saved (the old appearance.json only held the text size).
    let mut saved_look = prefs::read()["look"].clone();
    if !saved_look.is_object() {
        saved_look = std::env::var("APPDATA")
            .ok()
            .and_then(|d| std::fs::read(std::path::PathBuf::from(d).join("Nimbus Native").join("appearance.json")).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
    }
    apply_look(&ui, prefs::Look::from_json(&saved_look));
    let weak = ui.as_weak();
    ui.on_fullscreen_changed(move |on| {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_fullscreen(on);
        }
    });

    if std::env::args().any(|a| a == "--demo") {
        if let Some(phase) = std::env::args().find_map(|a| a.strip_prefix("--demo-splash=").and_then(|n| n.parse::<i32>().ok())) {
            return demo_splash(phase);
        }
        if let Some(variant) = std::env::args().find_map(|a| a.strip_prefix("--demo-toast=").and_then(|n| n.parse::<i32>().ok())) {
            return demo_toast(variant);
        }
        fill_demo(&ui);
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            if let Some(ui) = weak.upgrade() {
                set_window_icon(ui.window());
            }
        });
        return ui.run();
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let link_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_open_link(move |url| {
        let url = url.to_string();
        if let Some(id) = userprofile::user_of_link(&url) {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_show_user_card(id.into());
            }
            let _ = link_tx.send(Cmd::UserCard(id.to_string(), true));
        } else if url.starts_with("https://") || url.starts_with("http://") {
            let _ = open::that_detached(url);
        }
    });
    let guild_tx = tx.clone();
    ui.on_guild_clicked(move |id| {
        *LAST_GUILD_CLICK.lock().unwrap() = id.to_string();
        note_chat_click("");
        let _ = guild_tx.send(Cmd::SelectGuild(id.to_string()));
    });
    let channel_tx = tx.clone();
    let back_tx = tx.clone();
    ui.on_go_back(move || {
        let _ = back_tx.send(Cmd::GoBack);
    });
    let ahead_tx = tx.clone();
    ui.on_go_ahead(move || {
        let _ = ahead_tx.send(Cmd::GoAhead);
    });
    let weak = ui.as_weak();
    ui.on_category_toggled(move |id| {
        fold::toggle(id.as_str());
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_channels();
        let rows: Vec<ChannelItem> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
        let marks = fold::marks(rows.iter().map(|r| (r.kind, r.id.as_str())), &fold::load());
        for (i, (mut row, folded)) in rows.into_iter().zip(marks).enumerate() {
            if row.folded != folded {
                row.folded = folded;
                model.set_row_data(i, row);
            }
        }
    });
    ui.on_channel_clicked(move |id| {
        note_chat_click(&id);
        let _ = channel_tx.send(Cmd::SelectChannel(id.to_string()));
    });
    let voice_tx = tx.clone();
    ui.on_voice_clicked(move |id| {
        let _ = voice_tx.send(Cmd::JoinVoice(id.to_string()));
    });
    let call_tx = tx.clone();
    ui.on_dm_call_start(move |ring| {
        let _ = call_tx.send(Cmd::DmCall { ring });
    });
    let decline_tx = tx.clone();
    ui.on_dm_call_decline(move || {
        let _ = decline_tx.send(Cmd::DmCallDecline);
    });
    let leave_tx = tx.clone();
    ui.on_voice_leave(move || {
        let _ = leave_tx.send(Cmd::LeaveVoice);
    });
    let devices_tx = tx.clone();
    ui.on_devices_requested(move || {
        let _ = devices_tx.send(Cmd::Voice(voice::VoiceCmd::RefreshDevices));
    });
    let mic_tx = tx.clone();
    ui.on_mic_device_selected(move |index| {
        let _ = mic_tx.send(Cmd::Voice(voice::VoiceCmd::SetMic(index as usize)));
    });
    let speaker_tx = tx.clone();
    ui.on_speaker_device_selected(move |index| {
        let _ = speaker_tx.send(Cmd::Voice(voice::VoiceCmd::SetSpeaker(index as usize)));
    });
    let camera_tx = tx.clone();
    ui.on_camera_device_selected(move |index| {
        let _ = camera_tx.send(Cmd::Voice(voice::VoiceCmd::SetCamera(index as usize)));
    });
    let camera_on_tx = tx.clone();
    ui.on_toggle_camera(move |on| {
        let _ = camera_on_tx.send(Cmd::Voice(voice::VoiceCmd::Camera(on)));
    });
    let camera_preview_tx = tx.clone();
    ui.on_camera_preview_toggled(move |on| {
        let _ = camera_preview_tx.send(Cmd::Voice(voice::VoiceCmd::CameraPreview(on)));
    });
    let sources_tx = tx.clone();
    ui.on_share_sources_requested(move || {
        let _ = sources_tx.send(Cmd::Voice(voice::VoiceCmd::ListShareSources));
    });
    let share_tx = tx.clone();
    let share_weak = ui.as_weak();
    ui.on_share_picked(move |index, height, fps, sound| {
        let quality = share::Quality { height: height.max(0) as u32, fps: fps.max(1) as u32, sound };
        let dest = share_weak.upgrade().map(|ui| ui.get_share_dest() as i64).unwrap_or(1);
        let to = golive_send::ShareTo::from_index(dest).unwrap_or(golive_send::ShareTo::DEFAULT);
        let _ = share_tx.send(Cmd::Voice(voice::VoiceCmd::StartShare { index: index as usize, quality, to }));
    });
    let share_sound_tx = tx.clone();
    let share_prefs_weak = ui.as_weak();
    ui.on_share_settings_changed(move |height, fps, sound| {
        let dest = share_prefs_weak.upgrade().map(|ui| ui.get_share_dest()).unwrap_or(prefs::Share::DEFAULT.dest);
        prefs::write("share", prefs::Share { height, fps, sound, dest }.to_json());
        // While sharing, the switch is live: the sound goes out or stops at once.
        let _ = share_sound_tx.send(Cmd::Voice(voice::VoiceCmd::ShareSound(sound)));
    });
    let unshare_tx = tx.clone();
    ui.on_stop_share(move || {
        let _ = unshare_tx.send(Cmd::Voice(voice::VoiceCmd::StopShare));
    });
    let watch_tx = tx.clone();
    ui.on_watch_stream(move |identity| {
        log_line(&format!("click: open the stream of {identity}"));
        let _ = watch_tx.send(Cmd::Voice(voice::VoiceCmd::Watch(Some(identity.to_string()))));
    });
    let weak = ui.as_weak();
    ui.on_focus_stream(move |identity| {
        if let Some(ui) = weak.upgrade() {
            focus_stream(&ui, &identity);
        }
    });
    let weak = ui.as_weak();
    ui.on_set_stream_sound(move |identity| {
        if let Some(ui) = weak.upgrade() {
            toggle_stream_sound(&ui, &identity);
        }
    });
    let stop_one_tx = tx.clone();
    ui.on_stop_one(move |identity| {
        let _ = stop_one_tx.send(Cmd::Voice(voice::VoiceCmd::StopOne(identity.to_string())));
    });
    let reconnect_tx = tx.clone();
    ui.on_voice_reconnect(move || {
        let _ = reconnect_tx.send(Cmd::VoiceReconnect);
    });
    // What Discord asks for on joining is done in its own client, from the same invite.
    ui.on_invite_official(|code| {
        if invite::valid_code(&code) {
            let _ = open::that_detached(invite::link(&code));
        }
    });
    let unwatch_tx = tx.clone();
    ui.on_stop_watching(move || {
        let _ = unwatch_tx.send(Cmd::Voice(voice::VoiceCmd::Watch(None)));
    });
    let settings_tx = tx.clone();
    ui.on_voice_settings_changed(move |s| {
        let cur = audio::processing();
        let p = audio::Processing {
            noise: s.noise.clamp(0, 2) as u8,
            noise_strength: s.noise_strength.clamp(20, 100) as u32,
            echo: s.echo,
            auto_gain: s.auto_gain,
            mic_volume: s.mic_volume.round().clamp(0.0, 200.0) as u32,
            speaker_volume: s.speaker_volume.round().clamp(0.0, 200.0) as u32,
            auto_threshold: s.auto_threshold,
            threshold: s.threshold.round().clamp(-100.0, 0.0) as i32,
            voice_mode: s.voice_mode.clamp(0, 1) as u8,
            ptt_key: cur.ptt_key,
            ptt_mod: cur.ptt_mod,
            mute_key: cur.mute_key,
            mute_mod: cur.mute_mod,
            deafen_key: cur.deafen_key,
            deafen_mod: cur.deafen_mod,
        };
        let _ = settings_tx.send(Cmd::Voice(voice::VoiceCmd::SetProcessing(p)));
    });
    // A new key for push to talk: remember it and show it in the settings.
    let ptt_tx = tx.clone();
    let ptt_weak = ui.as_weak();
    ui.on_ptt_key_picked(move |key, ctrl, shift, alt| {
        let Some(vk) = audio::vk_for_key(&key) else { return };
        let mut mods = 0;
        if ctrl {
            mods |= audio::MOD_CONTROL;
        }
        if shift {
            mods |= audio::MOD_SHIFT;
        }
        if alt {
            mods |= audio::MOD_ALT;
        }
        // Without a modifier the key would fire while typing: Ctrl is added.
        if mods == 0 {
            mods = audio::MOD_CONTROL;
        }
        let cur = audio::processing();
        let p = audio::Processing { ptt_key: vk, ptt_mod: mods, ..cur };
        let name = audio::key_name(vk, mods);
        let _ = ptt_tx.send(Cmd::Voice(voice::VoiceCmd::SetProcessing(p)));
        on_ui(&ptt_weak, move |ui| ui.set_ptt_key_name(name.into()));
    });
    // Keys for muting the microphone and the sound from anywhere; Delete or Backspace clears one.
    let hot_tx = tx.clone();
    let hot_weak = ui.as_weak();
    ui.on_hotkey_picked(move |target, key, ctrl, shift, alt| {
        let Some(vk) = audio::vk_for_key(&key) else { return };
        let clear = vk == audio::VK_DELETE || vk == audio::VK_BACK;
        let mut mods = (ctrl as i32 * audio::MOD_CONTROL) | (shift as i32 * audio::MOD_SHIFT) | (alt as i32 * audio::MOD_ALT);
        // A bare letter would fire while typing: Ctrl is added.
        if mods == 0 {
            mods = audio::MOD_CONTROL;
        }
        let (vk, mods) = if clear { (0, 0) } else { (vk, mods) };
        let cur = audio::processing();
        let p = if target == 1 {
            audio::Processing { mute_key: vk, mute_mod: mods, ..cur }
        } else {
            audio::Processing { deafen_key: vk, deafen_mod: mods, ..cur }
        };
        let name = audio::hotkey_name(vk, mods);
        let _ = hot_tx.send(Cmd::Voice(voice::VoiceCmd::SetProcessing(p)));
        on_ui(&hot_weak, move |ui| if target == 1 { ui.set_mute_key_name(name.into()) } else { ui.set_deafen_key_name(name.into()) });
    });
    // While in a call: how fast Discord answers, next to the channel's name.
    let ping_timer = slint::Timer::default();
    let ping_weak = ui.as_weak();
    ping_timer.start(slint::TimerMode::Repeated, Duration::from_secs(3), move || {
        let Some(ui) = ping_weak.upgrade() else { return };
        let ms = discord::GATEWAY_PING_MS.load(std::sync::atomic::Ordering::Relaxed);
        // Discord's gateway first; once in a Nimbus room, the round trip to its server and the losses too.
        let (rtt, loss) = (voice::LINK_RTT_MS.load(std::sync::atomic::Ordering::Relaxed), voice::LINK_LOSS_PERMILLE.load(std::sync::atomic::Ordering::Relaxed));
        let mut text = if ms > 0 && !ui.get_voice_channel().is_empty() { i18n::tf(" · Discord {} мс", &[&ms]) } else { String::new() };
        if rtt > 0 && !ui.get_voice_channel().is_empty() {
            text.push_str(&i18n::tf(" · сервер {} мс", &[&rtt]));
        }
        if loss > 0 && !ui.get_voice_channel().is_empty() {
            text.push_str(&i18n::tf(" · потери {}.{}%", &[&(loss / 10), &(loss % 10)]));
        }
        ui.set_voice_ping(text.into());
    });
    std::mem::forget(ping_timer);
    // The keys themselves are watched in the background; a press arrives here.
    let hotkey_timer = slint::Timer::default();
    let hotkey_weak = ui.as_weak();
    hotkey_timer.start(slint::TimerMode::Repeated, Duration::from_millis(80), move || {
        let Some(ui) = hotkey_weak.upgrade() else { return };
        match audio::take_hotkey() {
            1 => ui.invoke_hotkey_mute(),
            2 => ui.invoke_hotkey_deafen(),
            _ => {}
        }
    });
    std::mem::forget(hotkey_timer);
    let find_tx = tx.clone();
    ui.on_find_messages(move |text| {
        let _ = find_tx.send(Cmd::FindMessages(text.to_string()));
    });
    let jump_tx = tx.clone();
    ui.on_jump_to(move |channel, message| {
        let _ = jump_tx.send(Cmd::JumpTo(channel.to_string(), message.to_string()));
    });
    ui.on_copy_text(|text| copy_text(&text));
    let copy_tx = tx.clone();
    ui.on_copy_message(move |id| {
        let _ = copy_tx.send(Cmd::CopyMessage(id.to_string()));
    });
    let link_tx = tx.clone();
    ui.on_copy_link(move |id| {
        let _ = link_tx.send(Cmd::CopyLink(id.to_string()));
    });
    let edit_tx = tx.clone();
    ui.on_begin_edit(move |id| {
        let _ = edit_tx.send(Cmd::BeginEdit(id.to_string()));
    });
    let save_tx = tx.clone();
    ui.on_edit_message(move |id, text| {
        let _ = save_tx.send(Cmd::EditMessage(id.to_string(), text.to_string()));
    });
    ui.on_open_discord_web(|| {
        let _ = open::that_detached("https://discord.com/channels/@me");
    });
    ui.on_open_sounds_folder(|| {
        if let Some(dir) = audio::sounds_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = open::that_detached(dir);
        }
    });
    let toast_tx = tx.clone();
    ui.on_toast_open(move |guild, channel| {
        let _ = toast_tx.send(Cmd::OpenChat(guild.to_string(), channel.to_string()));
    });
    let card_tx = tx.clone();
    ui.on_user_card_requested(move |id, popup| {
        let _ = card_tx.send(Cmd::UserCard(id.to_string(), popup));
    });
    let card_tx = tx.clone();
    ui.on_card_message(move |id, text| {
        let _ = card_tx.send(Cmd::CardMessage(id.to_string(), text.to_string()));
    });
    let view_tx = tx.clone();
    ui.on_viewer_load(move |url| {
        let _ = view_tx.send(Cmd::ViewImage(url.to_string()));
    });
    let view_tx = tx.clone();
    ui.on_viewer_save(move |url| {
        let _ = view_tx.send(Cmd::SaveImage(url.to_string()));
    });
    let menu_tx = tx.clone();
    ui.on_user_menu_requested(move |id| {
        let _ = menu_tx.send(Cmd::UserMenu(id.to_string()));
    });
    let widths = &prefs::read()["panel_widths"];
    if let Some(w) = widths[0].as_f64() {
        ui.set_sidebar_width(w as f32);
    }
    if let Some(w) = widths[1].as_f64() {
        ui.set_members_width(w as f32);
    }
    if let Some(w) = widths[2].as_f64() {
        ui.set_dm_panel_width(w as f32);
    }
    ui.on_panel_widths_saved(|sidebar, members, dm| prefs::write("panel_widths", json!([sidebar, members, dm])));
    let full_profile_tx = tx.clone();
    ui.on_full_profile_requested(move |id| {
        let _ = full_profile_tx.send(Cmd::FullProfile(id.to_string()));
    });
    let dm_menu_tx = tx.clone();
    ui.on_dm_menu_requested(move |channel| {
        let _ = dm_menu_tx.send(Cmd::DmMenu(channel.to_string()));
    });
    let privacy_tx = tx.clone();
    ui.on_privacy_changed(move |key, value| {
        let _ = privacy_tx.send(Cmd::Privacy(key.to_string(), value as i64));
    });
    let picture_tx = tx.clone();
    ui.on_profile_picture_pick(move |kind| {
        let _ = picture_tx.send(Cmd::ProfilePicture(kind.to_string()));
    });
    let profile_name_tx = tx.clone();
    ui.on_profile_name_saved(move |text| {
        let _ = profile_name_tx.send(Cmd::ProfileName(text.to_string()));
    });
    let pronouns_tx = tx.clone();
    ui.on_profile_pronouns_saved(move |text| {
        let _ = pronouns_tx.send(Cmd::ProfilePronouns(text.to_string()));
    });
    let custom_status_tx = tx.clone();
    ui.on_custom_status_saved(move |text| {
        let _ = custom_status_tx.send(Cmd::CustomStatus(text.to_string()));
    });
    let bio_tx = tx.clone();
    ui.on_bio_saved(move |text| {
        let _ = bio_tx.send(Cmd::Bio(text.to_string()));
    });
    let friend_sources_tx = tx.clone();
    ui.on_friend_sources_changed(move |all, friends, guilds| {
        let _ = friend_sources_tx.send(Cmd::FriendSources(all, friends, guilds));
    });
    let action_tx = tx.clone();
    ui.on_user_action(move |action, id| {
        if action == "copy-id" {
            copy_text(&id);
        } else if action == "share-profile" {
            copy_text(&format!("https://discord.com/users/{id}"));
        } else if let Some(url) = action.strip_prefix("open-link:") {
            if url.starts_with("https://") {
                let _ = open::that_detached(url.to_string());
            }
        } else if action == "report" {
            let _ = open::that_detached("https://dis.gd/report");
        } else if action == "shop" {
            let _ = open::that_detached("https://discord.com/shop");
        } else if let Some(track) = action.strip_prefix("spotify:") {
            if !track.is_empty() && track.chars().all(|c| c.is_ascii_alphanumeric()) {
                let _ = open::that_detached(format!("https://open.spotify.com/track/{track}"));
            }
        } else if let Some(channel) = action.strip_prefix("copy-channel:") {
            copy_text(channel);
        } else {
            let _ = action_tx.send(Cmd::UserAction(action.to_string(), id.to_string()));
        }
    });
    let volume_tx = tx.clone();
    ui.on_user_volume(move |id, percent| {
        let _ = volume_tx.send(Cmd::UserVolume(id.to_string(), percent.round().clamp(0.0, 200.0) as u32));
    });
    // Scrolled to the top of a chat: the worker brings older messages (it ignores repeats).
    let older_timer = slint::Timer::default();
    let (older_tx, weak) = (tx.clone(), ui.as_weak());
    older_timer.start(slint::TimerMode::Repeated, Duration::from_millis(300), move || {
        if weak.upgrade().is_some_and(|ui| ui.invoke_near_top()) {
            let _ = older_tx.send(Cmd::LoadOlder);
        }
    });
    let newer_timer = slint::Timer::default();
    let (newer_tx, weak) = (tx.clone(), ui.as_weak());
    newer_timer.start(slint::TimerMode::Repeated, Duration::from_millis(300), move || {
        if weak.upgrade().is_some_and(|ui| ui.get_detached() && ui.invoke_near_bottom()) {
            let _ = newer_tx.send(Cmd::LoadNewer);
        }
    });
    let latest_tx = tx.clone();
    ui.on_return_latest(move || {
        let _ = latest_tx.send(Cmd::ReturnLatest);
    });
    let test_tx = tx.clone();
    ui.on_mic_test(move |on| {
        let _ = test_tx.send(Cmd::Voice(voice::VoiceCmd::MicTest(on)));
    });
    let switches_tx = tx.clone();
    ui.on_switches_changed(move |muted, deafened| {
        let _ = switches_tx.send(Cmd::VoiceSwitches(muted, deafened));
    });
    let open_tx = tx.clone();
    ui.on_person_open(move |id| {
        let _ = open_tx.send(Cmd::PersonOpen(id.to_string()));
    });
    let accept_tx = tx.clone();
    ui.on_person_accept(move |id| {
        let _ = accept_tx.send(Cmd::PersonAccept(id.to_string()));
    });
    let remove_tx = tx.clone();
    ui.on_person_remove(move |id| {
        let _ = remove_tx.send(Cmd::PersonRemove(id.to_string()));
    });
    let tab_tx = tx.clone();
    ui.on_friends_tab_changed(move |tab| {
        let _ = tab_tx.send(Cmd::FriendsTab(tab));
    });
    let add_tx = tx.clone();
    ui.on_add_friend(move |name| {
        let _ = add_tx.send(Cmd::AddFriend(name.to_string()));
    });
    let search_tx = tx.clone();
    ui.on_search_changed(move |text| {
        let _ = search_tx.send(Cmd::Search(text.to_string()));
    });
    let pick_tx = tx.clone();
    ui.on_search_picked(move |index| {
        let _ = pick_tx.send(Cmd::SearchPick(index.max(0) as usize));
    });
    let typing_tx = tx.clone();
    ui.on_typing(move || {
        let _ = typing_tx.send(Cmd::Typing);
    });
    let react_tx = tx.clone();
    ui.on_react(move |message, key| {
        let _ = react_tx.send(Cmd::React(message.to_string(), key.to_string()));
    });
    let vote_tx = tx.clone();
    ui.on_poll_vote(move |message, answer| {
        if let Ok(answer) = u64::try_from(answer) {
            let _ = vote_tx.send(Cmd::PollVote(message.to_string(), answer));
        }
    });
    // Scheduled events: the window, "Интересно", cancelling ours, and the three steps of a new one.
    let events_tx = tx.clone();
    ui.on_events_opened(move || {
        let _ = events_tx.send(Cmd::EventsOpen);
    });
    let interest_tx = tx.clone();
    ui.on_event_interest(move |id, on| {
        let _ = interest_tx.send(Cmd::EventInterest(id.to_string(), on));
    });
    let event_delete_tx = tx.clone();
    ui.on_event_delete(move |id| {
        let _ = event_delete_tx.send(Cmd::EventDelete(id.to_string()));
    });
    let create_open_tx = tx.clone();
    ui.on_event_create_open(move || {
        let _ = create_open_tx.send(Cmd::EventCreateOpen);
    });
    let weak = ui.as_weak();
    ui.on_event_preview_make(move || {
        if let Some(ui) = weak.upgrade() {
            event_preview(&ui);
        }
    });
    let create_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_event_create(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = create_tx.send(Cmd::EventCreate(event_draft(&ui)));
        }
    });
    // Ctrl+V in the message box with pictures or files on the clipboard (see `speed_up_scrolling`).
    let paste_tx = tx.clone();
    ui.on_paste_clipboard(move || {
        let pasted = clipboard::read();
        if !pasted.is_empty() {
            let _ = paste_tx.send(Cmd::Paste(pasted));
        }
    });
    let remove_tx = tx.clone();
    ui.on_pending_remove(move |index| {
        if let Ok(index) = usize::try_from(index) {
            let _ = remove_tx.send(Cmd::PendingRemove(index));
        }
    });
    let component_tx = tx.clone();
    ui.on_component_click(move |message, custom| {
        let _ = component_tx.send(Cmd::ComponentClick(message.to_string(), custom.to_string()));
    });
    let select_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_select_opened(move |message, custom| {
        // The last menu's options go at once; this one's come from the network side.
        if let Some(ui) = weak.upgrade() {
            ui.set_select_options(ModelRc::new(VecModel::<SelectOptionItem>::default()));
        }
        let _ = select_tx.send(Cmd::SelectOpen(message.to_string(), custom.to_string()));
    });
    let weak = ui.as_weak();
    ui.on_select_toggle(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let options = ui.get_select_options();
        let Some(picked) = usize::try_from(index).ok().filter(|&i| i < options.row_count()) else { return };
        if ui.get_select_multi() {
            let chosen = (0..options.row_count()).filter(|&i| options.row_data(i).is_some_and(|o| o.chosen)).count();
            let on = options.row_data(picked).is_some_and(|o| o.chosen);
            if on || chosen < ui.get_select_max().max(1) as usize {
                update_row(options, picked, |o| o.chosen = !on);
            }
        } else {
            // One choice: picking it closes the menu and sends it.
            for i in 0..options.row_count() {
                update_row(options.clone(), i, |o| o.chosen = i == picked);
            }
            ui.set_select_open(false);
            ui.invoke_select_apply();
        }
    });
    let apply_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_select_apply(move || {
        let Some(ui) = weak.upgrade() else { return };
        let options = ui.get_select_options();
        let values: Vec<String> = (0..options.row_count()).filter_map(|i| options.row_data(i)).filter(|o| o.chosen).map(|o| o.value.to_string()).collect();
        if options.row_count() > 0 {
            let _ = apply_tx.send(Cmd::ComponentSelect(ui.get_select_message().to_string(), ui.get_select_custom().to_string(), values));
        }
    });
    let play_tx = tx.clone();
    ui.on_voice_play(move |message| {
        let _ = play_tx.send(Cmd::VoicePlay(message.to_string()));
    });
    let input_tx = tx.clone();
    ui.on_input_changed(move |text| {
        let _ = input_tx.send(Cmd::InputChanged(text.to_string()));
    });
    let pick_tx = tx.clone();
    ui.on_suggest_picked(move |i| {
        let _ = pick_tx.send(Cmd::PickSuggest(i.max(0) as usize));
    });
    let slash_tx = tx.clone();
    ui.on_slash_picked(move |i| {
        let _ = slash_tx.send(Cmd::SlashPick(i.max(0) as usize));
    });
    let sound_tx = tx.clone();
    ui.on_soundboard_opened(move || {
        let _ = sound_tx.send(Cmd::SoundboardOpen);
    });
    let sound_tx = tx.clone();
    ui.on_sound_query(move |text| {
        let _ = sound_tx.send(Cmd::SoundQuery(text.to_string()));
    });
    let sound_tx = tx.clone();
    ui.on_sound_play(move |id, guild| {
        let _ = sound_tx.send(Cmd::SoundPlay(id.to_string(), guild.to_string()));
    });
    let sound_tx = tx.clone();
    ui.on_sounds_muted_changed(move |muted| {
        let _ = sound_tx.send(Cmd::SoundsMuted(muted));
    });
    let more_tx = tx.clone();
    ui.on_members_near_end(move || {
        let _ = more_tx.send(Cmd::MoreMembers);
    });
    let emoji_tx = tx.clone();
    ui.on_emoji_query(move |q| {
        let _ = emoji_tx.send(Cmd::EmojiQuery(q.to_string()));
    });
    // The GIF panel's search goes out once typing has paused, not on every letter.
    let (gif_tx, gif_timer) = (tx.clone(), slint::Timer::default());
    ui.on_gif_query(move |q| {
        let (tx, q) = (gif_tx.clone(), q.to_string());
        gif_timer.start(slint::TimerMode::SingleShot, GIF_PAUSE, move || {
            let _ = tx.send(Cmd::GifQuery(q.clone()));
        });
    });
    let gif_more_tx = tx.clone();
    ui.on_gif_more(move || {
        let _ = gif_more_tx.send(Cmd::GifMore);
    });
    let gif_send_tx = tx.clone();
    ui.on_send_gif(move |url| {
        let _ = gif_send_tx.send(Cmd::SendGif(url.to_string()));
    });
    let forward_tx = tx.clone();
    ui.on_forward_open(move |id| {
        let _ = forward_tx.send(Cmd::ForwardOpen(id.to_string()));
    });
    let forward_query_tx = tx.clone();
    ui.on_forward_query(move |text| {
        let _ = forward_query_tx.send(Cmd::ForwardQuery(text.to_string()));
    });
    let forward_to_tx = tx.clone();
    ui.on_forward_to(move |id| {
        let _ = forward_to_tx.send(Cmd::ForwardTo(id.to_string()));
    });
    let note_tx = tx.clone();
    ui.on_note_saved(move |user, text| {
        let _ = note_tx.send(Cmd::SaveNote(user.to_string(), text.to_string()));
    });
    let notify_menu_tx = tx.clone();
    ui.on_notify_menu_requested(move |guild, channel| {
        let _ = notify_menu_tx.send(Cmd::NotifyMenu(guild.to_string(), channel.to_string()));
    });
    let notify_change_tx = tx.clone();
    ui.on_notify_change(move |guild, channel, what| {
        let _ = notify_change_tx.send(Cmd::NotifyChange(guild.to_string(), channel.to_string(), what.to_string()));
    });
    let lookup_tx = tx.clone();
    ui.on_invite_lookup(move |text| {
        let _ = lookup_tx.send(Cmd::InviteLookup(text.to_string()));
    });
    let join_tx = tx.clone();
    ui.on_invite_join(move |code| {
        let _ = join_tx.send(Cmd::InviteJoin(code.to_string()));
    });
    let make_invite_tx = tx.clone();
    ui.on_create_invite(move |channel| {
        let _ = make_invite_tx.send(Cmd::CreateInvite(channel.to_string()));
    });
    let guild_invite_tx = tx.clone();
    ui.on_invite_to_guild(move |guild| {
        let _ = guild_invite_tx.send(Cmd::InviteToGuild(guild.to_string()));
    });
    let invite_friend_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_invite_friend(move |user, link| {
        if let Some(ui) = weak.upgrade() {
            change_invite_person(&ui, &user, |p| p.kind = 1);
        }
        let _ = invite_friend_tx.send(Cmd::InviteFriend(user.to_string(), link.to_string()));
    });
    let thread_tx = tx.clone();
    ui.on_create_thread(move |message, name| {
        let _ = thread_tx.send(Cmd::CreateThread(message.to_string(), name.to_string()));
    });
    let post_tx = tx.clone();
    ui.on_create_post(move |forum, title, text| {
        let _ = post_tx.send(Cmd::CreatePost(forum.to_string(), title.to_string(), text.to_string()));
    });
    let record_tx = tx.clone();
    ui.on_record_start(move || {
        let _ = record_tx.send(Cmd::RecordStart);
    });
    let record_send_tx = tx.clone();
    ui.on_record_send(move || {
        let _ = record_send_tx.send(Cmd::RecordSend);
    });
    let record_cancel_tx = tx.clone();
    ui.on_record_cancel(move || {
        let _ = record_cancel_tx.send(Cmd::RecordCancel);
    });
    ui.on_install_update(update::install);
    netcheck::wire(&ui);
    let web_tx = tx.clone();
    let web_ui = ui.as_weak();
    ui.on_web_login(move || {
        // The real discord.com page in a window of its own; the token it gives back is checked
        // before it is kept.
        let (tx, ui) = (web_tx.clone(), web_ui.clone());
        // A new try: the same failure again must still be noticed.
        if let Some(window) = ui.upgrade() {
            window.set_login_error("".into());
        }
        weblogin::open(move |result| match result {
            Ok(token) => {
                let _ = tx.send(Cmd::Login(token));
            }
            Err(e) => on_ui(&ui, move |ui| ui.set_login_error(e.into())),
        });
    });
    let pins_tx = tx.clone();
    let mentions_tx = tx.clone();
    ui.on_show_mentions(move || {
        let _ = mentions_tx.send(Cmd::ShowMentions);
    });
    ui.on_show_pins(move || {
        let _ = pins_tx.send(Cmd::ShowPins);
    });
    let attach_tx = tx.clone();
    ui.on_attach_file(move || {
        let _ = attach_tx.send(Cmd::AttachFile);
    });
    let reply_tx = tx.clone();
    ui.on_reply_message(move |id, text| {
        let _ = reply_tx.send(Cmd::Reply(id.to_string(), text.to_string()));
    });
    let look_weak = ui.as_weak();
    ui.on_appearance_changed(move |_| {
        if let Some(ui) = look_weak.upgrade() {
            let look = read_look(&ui);
            clock::set(look.time12, look.date_iso);
            apply_icon(&ui, look.app_icon);
            sync_streamer(&ui);
            // The "hide from captures" switch applies at once to a share that is running.
            set_sharing(&ui, ui.get_sharing());
            prefs::write("look", look.to_json());
        }
    });
    ui.on_send(move |text| {
        let _ = tx.send(Cmd::Send(text.to_string()));
    });
    setup_system(&ui);
    // The streaming server (LiveKit and the token service) runs as long as Nimbus does, but only when
    // switched on ("own_server": true in the settings); calls and Go Live work through Discord without it.
    clear_message_cache(true);
    gifcache::tidy();
    let own_server = prefs::read()["own_server"].as_bool().unwrap_or(false);
    if !own_server {
        log_line("voice server: off (Discord voice only)");
    } else if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        let saved = prefs::read()["server_dir"].as_str().map(String::from);
        match servers::find_dir(&exe_dir, saved.as_deref()) {
            Some(dir) => servers::keep_running(dir),
            None => log_line("voice server: no server folder found next to the exe or in the settings"),
        }
    }
    setup_notifications(&ui);

    // The small window first: it looks for an update, and then the app opens (and only then signs in).
    let splash = SplashWindow::new()?;
    splash.set_still(prefs::Look::from_json(&saved_look).reduce_motion);
    let skip = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let skip_click = skip.clone();
    splash.on_skip(move || skip_click.store(true, std::sync::atomic::Ordering::Relaxed));
    let dragged = splash.as_weak();
    splash.on_drag(move || {
        use slint::winit_030::WinitWindowAccessor;
        if let Some(splash) = dragged.upgrade() {
            splash.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    let splash_weak = splash.as_weak();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
        runtime.block_on(async move {
            // The client connects while the update is checked, so it opens with everything in place;
            // with nobody signed in there is nothing to wait for.
            let signed_in = saved_token().is_some() && std::env::var_os("NIMBUS_FORCE_LOGIN").is_none();
            let shown = weak.clone();
            let opening = async move {
                update::at_start(splash_weak.clone(), skip).await;
                if signed_in {
                    let _ = tokio::time::timeout(CLIENT_READY_WAIT, CLIENT_READY.notified()).await;
                }
                let opened = splash_weak.clone();
                on_ui(&shown, move |ui| {
                    let _ = ui.show();
                    set_window_icon(ui.window());
                    if let Some(splash) = opened.upgrade() {
                        let _ = splash.hide();
                    }
                });
            };
            tokio::join!(opening, run(weak, rx));
        });
    });
    center_splash(&splash);
    splash.show()?;
    set_window_icon(splash.window());
    center_splash(&splash);
    let shown = splash.window().position();
    log_line(&format!("start window at {},{}", shown.x, shown.y));
    // Closing the window may only hide it (the tray), so the loop runs until asked to quit.
    let result = slint::run_event_loop_until_quit();
    // What Nimbus started for itself goes away with it.
    servers::stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_server_is_read_from_the_gateway() {
        let nested = json!({ "id": "1", "properties": { "name": "Basement", "icon": "abc" } });
        assert_eq!(new_guild(&nested), Some(("1".to_string(), "Basement".to_string(), Some("abc".to_string()))));
        let flat = json!({ "id": "2", "name": "Flat", "icon": null });
        assert_eq!(new_guild(&flat), Some(("2".to_string(), "Flat".to_string(), None)));
        assert_eq!(new_guild(&json!({ "id": "3", "unavailable": true })), None);
    }

    #[test]
    fn typing_shows_part_of_the_text_then_all_of_it() {
        let block = |kind: i32, text: &str| Block { kind, text: text.into(), ..Default::default() };
        let full = vec![block(TEXT, "abc"), block(SPOILER, "hidden"), block(QUOTE, "de")];
        assert_eq!(full.iter().map(typed_len).sum::<usize>(), 6);
        assert_eq!(typed_blocks(&full, 0)[0].text, "▌");
        assert_eq!(typed_blocks(&full, 2)[0].text, "ab▌");
        assert_eq!(typed_blocks(&full, 3).len(), 1, "the spoiler isn't reached yet");
        let more = typed_blocks(&full, 5);
        assert_eq!(more[1].text, "hidden");
        assert_eq!(more[2].text, "d▌");
        assert_eq!(typed_blocks(&full, 6).len(), 3);
    }

    #[test]
    fn the_start_window_sits_in_the_middle() {
        assert_eq!(centered((1920, 1080), (320, 380)), (800, 350));
        assert_eq!(centered((2560, 1440), (400, 475)), (1080, 482));
    }

    #[test]
    fn forwarded_messages_show_the_original_as_a_quote() {
        let m = json!({
            "id": "9", "author": { "id": "1", "username": "a" }, "timestamp": "2026-11-01T10:00:00+00:00", "content": "",
            "reactions": [{ "emoji": { "name": "👍" }, "count": 2, "me": false }],
            "message_reference": { "type": 1, "channel_id": "5", "message_id": "6" },
            "message_snapshots": [{ "message": { "content": "первая строка\n```\nкод\n```", "attachments": [{ "filename": "план.pdf", "content_type": "application/pdf", "url": "https://cdn.discordapp.com/a.pdf" }] } }],
        });
        let msg = to_msg(&m, &mut None, &mut Vec::new());
        assert!(msg.forwarded);
        assert_eq!(msg.blocks[0].kind, QUOTE);
        assert_eq!(msg.blocks[0].text, "первая строка");
        assert_eq!(msg.blocks[1].kind, CODE, "code stays code");
        assert!(msg.cards.iter().any(|c| c.title == "план.pdf"), "the original's files");
        assert_eq!(msg.reactions.len(), 1, "reactions are the forward's own");
        let plain = to_msg(&json!({ "id": "1", "author": { "id": "1" }, "content": "привет" }), &mut None, &mut Vec::new());
        assert!(!plain.forwarded);
        assert_eq!(plain.blocks[0].kind, TEXT);
    }

    #[test]
    fn stickers_pick_their_picture_by_format() {
        let png = "https://media.discordapp.net/stickers/42.png?size=160";
        assert_eq!(sticker_url("42", 1), Some((png.to_string(), false)));
        assert_eq!(sticker_url("42", 2), Some((png.to_string(), true)), "animated PNG");
        assert_eq!(sticker_url("42", 4), Some(("https://media.discordapp.net/stickers/42.gif?size=160".to_string(), true)));
        assert_eq!(sticker_url("42", 3), None, "Lottie: only the name");
        assert_eq!(sticker_url("4/../2", 1), None);
        let m = json!({ "sticker_items": [{ "id": "42", "name": "Wumpus", "format_type": 3 }] });
        assert_eq!(sticker_of(&m), Some(Sticker { name: "Wumpus".into(), url: None, animated: false }));
        let m = json!({ "sticker_items": [{ "id": "42", "name": "Hi", "format_type": 4 }] });
        assert!(sticker_of(&m).is_some_and(|s| s.animated && s.url.as_deref().is_some_and(|u| u.ends_with(".gif?size=160"))));
        assert_eq!(sticker_of(&json!({ "content": "x" })), None);
    }

    #[test]
    fn extras_of_a_message_are_read() {
        let m = json!({
            "id": "5", "author": { "id": "1", "username": "a" }, "timestamp": "2026-11-01T10:00:00+00:00",
            "content": " <a:dance:88> 🔥 ", "flags": 8192,
            "attachments": [{ "url": "https://cdn.discordapp.com/attachments/1/2/voice-message.ogg", "filename": "voice-message.ogg", "duration_secs": 3.0, "waveform": "AP8=", "content_type": "audio/ogg" }],
            "sticker_items": [{ "id": "42", "name": "Hi", "format_type": 1 }],
        });
        let mut jobs = Vec::new();
        let msg = to_msg(&m, &mut None, &mut jobs);
        assert_eq!(msg.emojis.len(), 2);
        assert!(msg.blocks.is_empty(), "the emoji aren't repeated as text");
        assert!(msg.cards.is_empty(), "a voice message is no file card");
        assert!(msg.voice.is_some());
        assert!(jobs.iter().any(|j| j.sticker && !j.animated));
        assert_eq!(emoji_urls([&msg]), vec!["https://cdn.discordapp.com/emojis/88.webp?size=48&animated=true".to_string()]);
    }

    #[test]
    fn profile_pictures_are_told_by_their_bytes_and_size() {
        assert_eq!(picture_mime(&[0x89, b'P', b'N', b'G', 1, 2]), Some("image/png"));
        assert_eq!(picture_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(picture_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(picture_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(picture_mime(b"MZ\x90\0 not a picture"), None);
        let dir = std::env::temp_dir().join(format!("nimbus-picture-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0, 0]).unwrap();
        assert!(picture_data_uri(&png).unwrap().starts_with("data:image/png;base64,"));
        let text = dir.join("b.png");
        std::fs::write(&text, b"just text").unwrap();
        assert!(picture_data_uri(&text).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn banner_urls_need_a_plain_hash() {
        assert_eq!(banner_url(&json!({ "id": "5", "banner": "abc" })).as_deref(), Some("https://cdn.discordapp.com/banners/5/abc.webp?size=480"));
        assert_eq!(banner_url(&json!({ "id": "5", "banner": "../x" })), None);
        assert_eq!(banner_url(&json!({ "id": "5", "banner": null })), None);
    }

    #[test]
    fn embed_gifs_for_tenor_giphy_and_plain_links() {
        let none = json!({});
        // A plain .gif link is the animation itself, query and all.
        assert_eq!(embed_gif(&none, "https://images-ext-1.discordapp.net/a.gif?size=2").as_deref(), Some("https://images-ext-1.discordapp.net/a.gif?size=2"));
        // The poster's own host is never fetched directly.
        assert_eq!(embed_gif(&none, "https://x.test/a.gif?size=2"), None);
        assert_eq!(embed_gif(&none, "http://cdn.discordapp.com/a.gif"), None);
        assert_eq!(embed_gif(&json!({ "video": { "url": "https://evil.test/x/giphy.com/giphy.mp4" } }), "https://evil.test/a.png"), None);
        assert!(is_trusted_media_host("https://media.tenor.com/a.png") && !is_trusted_media_host("https://notdiscordapp.com/a.png"));
        // Tenor's png thumbnail (…AAAAe) has its gif (…AAAAC) beside it.
        assert_eq!(
            embed_gif(&none, "https://media.tenor.com/abcAAAAe/cat.png").as_deref(),
            Some("https://media.tenor.com/abcAAAAC/cat.gif")
        );
        // Giphy: the mp4 of the embed has giphy.gif next to it.
        let giphy = json!({ "video": { "url": "https://media.giphy.com/media/xyz/giphy.mp4" } });
        assert_eq!(embed_gif(&giphy, "https://i.giphy.com/xyz.webp").as_deref(), Some("https://media.giphy.com/media/xyz/giphy.gif"));
        // Anything else stays a still picture.
        assert_eq!(embed_gif(&none, "https://x.test/a.png"), None);
        assert_eq!(embed_gif(&json!({ "video": { "url": "https://x.test/v.mp4" } }), "https://x.test/a.png"), None);
    }

    #[test]
    fn discord_markdown_becomes_commonmark() {
        assert_eq!(discord_to_commonmark("**жирный** и __подчёркнутый__"), "**жирный** и <u>подчёркнутый</u>");
        assert_eq!(discord_to_commonmark("<script>"), "\\<script>");
        assert_eq!(discord_to_commonmark("смотри https://x.test/a?b=1 тут"), "смотри <https://x.test/a?b=1> тут");
        assert_eq!(discord_to_commonmark("[ссылка](https://x.test)"), "[ссылка](https://x.test)");
        assert_eq!(discord_to_commonmark("a\nb"), "a  \nb");
        assert_eq!(strip_inline("**a** ~~b~~ `c` *d*"), "a b c d");
        // Real markdown parses; the styled text keeps the words.
        assert!(slint::StyledText::from_markdown(&discord_to_commonmark("**a** ~~b~~ `c` *d* <https://x.test>")).is_ok());
    }

    #[test]
    fn files_and_previews_become_cards() {
        assert_eq!(human_size(90), "90 Б");
        assert_eq!(human_size(340 * 1024), "340 КБ");
        assert_eq!(human_size(13 * 1024 * 1024 + 400 * 1024), "13,4 МБ");
        let m = json!({
            "attachments": [
                { "filename": "a.zip", "url": "https://c/a.zip", "size": 2048, "content_type": "application/zip" },
                { "filename": "p.png", "url": "https://c/p.png", "size": 10, "content_type": "image/png" },
            ],
            "embeds": [
                { "type": "article", "title": "Заголовок", "description": "Описание  страницы", "url": "https://x.test", "color": 255 },
                { "type": "gifv", "url": "https://tenor.com/x" },
                { "type": "link", "url": "https://empty.test" },
            ],
        });
        let cards = cards_of(&m);
        assert_eq!(cards.len(), 2, "the picture, the gif and the empty preview are not cards");
        assert_eq!((cards[0].kind, cards[0].title.as_str(), cards[0].text.as_str()), (0, "a.zip", "2,0 КБ"));
        assert_eq!((cards[1].kind, cards[1].text.as_str(), cards[1].color), (1, "Описание страницы", Some(255)));
    }

    #[test]
    fn threads_sit_under_their_channels() {
        let threads = vec![
            json!({ "id": "1", "parent_id": "10", "type": 11, "name": "старая", "last_message_id": "100" }),
            json!({ "id": "2", "parent_id": "10", "type": 11, "name": "новая", "last_message_id": "300" }),
            json!({ "id": "3", "parent_id": "20", "type": 4, "name": "не ветка" }),
        ];
        let list = threads_by_parent(&threads);
        let order: Vec<(&str, &str)> = list.iter().map(|(p, t)| (p.as_str(), t["id"].as_str().unwrap())).collect();
        assert_eq!(order, [("10", "2"), ("10", "1")], "newest activity first; only real threads");
    }

    #[test]
    fn nameplate_and_frame_come_from_the_profile() {
        let plain = profile_look(&json!({ "id": "5", "avatar": "x", "banner": "a_b1", "banner_color": "#ff8800" }));
        assert_eq!(plain, ProfileLook::default(), "the profile banner is not used");
        let fancy = profile_look(&json!({
            "id": "5",
            "avatar_decoration_data": { "asset": "a_deco", "sku_id": "9" },
            "collectibles": { "nameplate": { "sku_id": "1", "asset": "nameplates/nameplates/twilight/", "palette": "crimson" } }
        }));
        assert!(fancy.nameplate.unwrap().ends_with("/assets/collectibles/nameplates/nameplates/twilight/static.png"));
        assert!(fancy.decoration.unwrap().contains("/avatar-decoration-presets/a_deco.png"));
        // Empty or odd paths are nothing.
        assert_eq!(profile_look(&json!({ "avatar_decoration_data": { "asset": "" }, "collectibles": { "nameplate": { "asset": "" } } })), ProfileLook::default());
        assert_eq!(nameplate_url(&json!({ "collectibles": { "nameplate": { "asset": "../x/" } } })), None);
        assert_eq!(nameplate_url(&json!({ "collectibles": null })), None);
    }

    #[test]
    fn name_style_colour_is_read() {
        let styled = json!({ "id": "5", "display_name_styles": { "font_id": 3, "effect_id": 2, "colors": [16711680, 255] } });
        assert_eq!(name_style_color(&styled), Some(0xff0000));
        assert_eq!(name_style_color(&json!({ "id": "5", "display_name_styles": null })), None);
        assert_eq!(name_style_color(&json!({ "id": "5" })), None);
        assert!(decoration_url(&json!({ "avatar_decoration_data": { "asset": "a_x" } })).is_some());
    }

    #[test]
    fn channel_names_lose_separators_the_font_cannot_draw() {
        assert_eq!(tidy_name("\u{2753}\u{2758}faq"), "\u{2753}·faq");
        assert_eq!(tidy_name("\u{1F44B}\u{30FB}welcome"), "\u{1F44B}·welcome");
        assert_eq!(tidy_name("status\u{FE31}updates"), "status·updates");
        assert_eq!(tidy_name("a\u{3164}\u{3164}b"), "a b");
        assert_eq!(tidy_name("\u{2728}\u{3021}Community"), "\u{2728}·Community");
        assert_eq!(tidy_name("ttml\u{2601}\u{FE0F}\u{3021}uploads"), "ttml\u{2601}·uploads");
        assert_eq!(tidy_name("  plain-name  "), "plain-name");
        assert_eq!(tidy_name("русский-чат"), "русский-чат");
    }

    #[test]
    fn our_activity_line_counts_the_rest() {
        let game = |n: &str| json!({ "type": 0, "name": n });
        assert_eq!(activity_line(&[]), i18n::t("В сети"));
        assert_eq!(activity_line(&[game("Rust")]), "Rust");
        assert_eq!(activity_line(&[game("Rust"), game("Tidal"), game("Steam")]), "Rust +2");
    }

    #[test]
    fn nameplates_sit_on_their_palettes_colour() {
        // A fully see-through picture becomes the gradient: dark crimson on the left, lighter on the right.
        let mut blank = SharedPixelBuffer::<slint::Rgba8Pixel>::new(64, 4);
        for p in blank.make_mut_slice() {
            *p = slint::Rgba8Pixel { r: 0, g: 0, b: 0, a: 0 };
        }
        let out = under_palette(blank, "crimson");
        let (left, right) = (out.as_slice()[0], out.as_slice()[63]);
        assert_eq!((left.r, left.g, left.b, left.a), (0x1c, 0x08, 0x13, 255));
        assert_eq!((right.r, right.g, right.b), (0x4a, 0x16, 0x32));
        // An opaque pixel stays as it is; an unknown palette changes nothing.
        let mut solid = SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 1);
        for p in solid.make_mut_slice() {
            *p = slint::Rgba8Pixel { r: 200, g: 10, b: 20, a: 255 };
        }
        assert_eq!(under_palette(solid.clone(), "crimson").as_slice()[0].r, 200);
        assert_eq!(under_palette(solid, "no-such-palette").as_slice()[1].g, 10);
        assert_eq!(nameplate_palette(&json!({ "collectibles": { "nameplate": { "palette": "sky" } } })), "sky");
        assert_eq!(nameplate_palette(&json!({})), "");
    }

    #[test]
    fn server_icons_show_who_is_in_voice() {
        let state = |channel: Option<&str>, stream: bool| json!({ "channel_id": channel, "self_stream": stream });
        let states = |list: &[(&str, Option<&str>, bool)]| -> HashMap<String, Value> {
            list.iter().map(|(u, c, s)| (u.to_string(), state(*c, *s))).collect()
        };
        // Nobody.
        assert_eq!(badge_of(voice_summary(&states(&[]), "me"), false), 0);
        // Someone else in voice (even showing a screen): no badge, that picture would go stale.
        assert_eq!(badge_of(voice_summary(&states(&[("a", Some("1"), false)]), "me"), false), 0);
        assert_eq!(badge_of(voice_summary(&states(&[("a", Some("1"), false), ("b", Some("1"), true)]), "me"), false), 0);
        // We are in voice: green; showing our screen: green with a screen, even before Discord says so.
        assert_eq!(badge_of(voice_summary(&states(&[("me", Some("1"), false), ("a", Some("1"), true)]), "me"), false), 3);
        assert_eq!(badge_of(voice_summary(&states(&[("me", Some("1"), true)]), "me"), false), 4);
        assert_eq!(badge_of(voice_summary(&states(&[("me", Some("1"), false)]), "me"), true), 4);
        // Someone who just left (no channel) is not counted.
        assert_eq!(badge_of(voice_summary(&states(&[("a", None, true)]), "me"), false), 0);
    }

    #[test]
    fn only_a_401_means_the_token_is_gone() {
        assert!(is_unauthorized("Discord API 401 Unauthorized: {\"message\":\"401: Unauthorized\",\"code\":0}"));
        assert!(!is_unauthorized("Discord API 403 Forbidden: {}"));
        assert!(!is_unauthorized("Discord API 429 Too Many Requests: {}"));
        assert!(!is_unauthorized("error sending request for url (https://discord.com/api/v9/users/@me)"));
    }

    #[test]
    fn demo_window_positions() {
        assert_eq!(demo_position("599,-1015"), Some((599, -1015)));
        assert_eq!(demo_position(" -10 , 20 "), Some((-10, 20)));
        assert_eq!(demo_position("599"), None);
        assert_eq!(demo_position("a,b"), None);
        assert_eq!(demo_position(""), None);
    }

    #[test]
    fn the_dm_call_line() {
        assert_eq!(dm_call_line(&[]), "");
        assert_eq!(dm_call_line(&["Аня".into()]), "Идёт звонок: Аня");
        assert_eq!(dm_call_line(&["Аня".into(), "Борис".into()]), "Идёт звонок: Аня, Борис");
        let five: Vec<String> = ["а", "б", "в", "г", "д"].iter().map(|s| s.to_string()).collect();
        assert_eq!(dm_call_line(&five), "Идёт звонок: а, б и ещё 3");
    }

    #[test]
    fn attached_gif_and_webp_are_played() {
        let gif = json!({ "attachments": [{ "content_type": "image/gif", "url": "https://c/a.gif", "width": 10, "height": 10 }] });
        assert!(picture_of(&gif).unwrap().gif.is_some());
        let webp = json!({ "attachments": [{ "content_type": "image/webp", "url": "https://c/a.webp", "width": 10, "height": 10 }] });
        assert!(picture_of(&webp).unwrap().gif.is_some());
        let png = json!({ "attachments": [{ "content_type": "image/png", "url": "https://c/a.png", "width": 10, "height": 10 }] });
        assert!(picture_of(&png).unwrap().gif.is_none());
    }
}
