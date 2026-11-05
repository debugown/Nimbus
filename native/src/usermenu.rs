//! What someone's menu needs beyond the rights: the choices kept on this PC only, the roles it
//! offers, and the channel an invite is made in.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Local, Utc};
use serde_json::{json, Value};

use crate::perms;

const SOUND_KEY: &str = "muted_sound_users";
const VIDEO_KEY: &str = "hidden_video_users";
const PINNED_KEY: &str = "pinned_dms";
const FAVORITE_KEY: &str = "favorite_dms";
/// Discord's limit for a nickname.
const MAX_NICK: usize = 32;

/// Cameras we chose not to see. Asked for every picture of every camera, so kept in memory.
static HIDDEN_VIDEO: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(saved(VIDEO_KEY).into_iter().collect()));

fn parse(list: &Value) -> Vec<String> {
    list.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
}

fn saved(key: &str) -> Vec<String> {
    parse(&crate::prefs::read()[key])
}

/// The list with `id` taken out if it was in, or added if it wasn't; and whether it is in now.
fn toggled(mut list: Vec<String>, id: &str) -> (Vec<String>, bool) {
    if list.iter().any(|i| i == id) {
        list.retain(|i| i != id);
        return (list, false);
    }
    list.push(id.to_string());
    (list, true)
}

fn toggle(key: &str, id: &str) -> bool {
    let (list, on) = toggled(saved(key), id);
    crate::prefs::write(key, json!(list));
    on
}

pub fn sound_muted(user: &str) -> bool {
    saved(SOUND_KEY).iter().any(|u| u == user)
}

pub fn toggle_sound(user: &str) -> bool {
    toggle(SOUND_KEY, user)
}

pub fn video_hidden(user: &str) -> bool {
    HIDDEN_VIDEO.lock().unwrap().contains(user)
}

pub fn toggle_video(user: &str) -> bool {
    let on = toggle(VIDEO_KEY, user);
    let mut hidden = HIDDEN_VIDEO.lock().unwrap();
    if on {
        hidden.insert(user.to_string());
    } else {
        hidden.remove(user);
    }
    on
}

pub fn dm_pinned(channel: &str) -> bool {
    saved(PINNED_KEY).iter().any(|c| c == channel)
}

pub fn toggle_pinned(channel: &str) -> bool {
    toggle(PINNED_KEY, channel)
}

pub fn dm_favorite(channel: &str) -> bool {
    saved(FAVORITE_KEY).iter().any(|c| c == channel)
}

pub fn toggle_favorite(channel: &str) -> bool {
    toggle(FAVORITE_KEY, channel)
}

/// The favourite chats of the DM list and the others, in the order they had.
pub fn split_favorites<T>(rows: Vec<T>, id: impl Fn(&T) -> &str) -> (Vec<T>, Vec<T>) {
    let favorite = saved(FAVORITE_KEY);
    rows.into_iter().partition(|r| favorite.iter().any(|f| f == id(r)))
}

/// The pinned chats first, in the order they had; then the rest, also as they were.
fn pinned_in_front<T>(rows: Vec<T>, pinned: &[String], id: impl Fn(&T) -> &str) -> Vec<T> {
    let (mut first, rest): (Vec<T>, Vec<T>) = rows.into_iter().partition(|r| pinned.iter().any(|p| p == id(r)));
    first.extend(rest);
    first
}

pub fn pinned_first<T>(rows: Vec<T>, id: impl Fn(&T) -> &str) -> Vec<T> {
    pinned_in_front(rows, &saved(PINNED_KEY), id)
}

/// A row of the DM list as its menu sees it.
pub struct DmPlace {
    pub channel: String,
    pub unread: bool,
    pub muted: bool,
    pub pinned: bool,
    pub favorite: bool,
}

/// A nickname as Discord takes it; empty takes the nickname away.
pub fn nick(text: &str) -> String {
    text.trim().chars().take(MAX_NICK).collect()
}

/// What the menu's role list shows.
#[derive(Debug, PartialEq)]
pub struct RoleRow {
    pub id: String,
    pub name: String,
    pub has: bool,
    pub editable: bool,
}

/// The roles of a server, highest first: the ones they have, and the ones we could give them.
pub fn role_rows(me: &perms::Me, theirs: &[String]) -> Vec<RoleRow> {
    let mut roles: Vec<&Value> = me.all_roles.iter().filter(|r| r["id"].as_str().is_some_and(|id| id != me.guild)).collect();
    roles.sort_by_key(|r| std::cmp::Reverse(r["position"].as_i64().unwrap_or(0)));
    roles
        .into_iter()
        .filter_map(|r| {
            let id = r["id"].as_str()?.to_string();
            let has = theirs.contains(&id);
            let editable = me.role_editable(r);
            (has || editable).then(|| RoleRow { name: r["name"].as_str().unwrap_or("").to_string(), id, has, editable })
        })
        .collect()
}

/// The first text or announcement channel of a server that we may invite to: where an invite is made.
pub fn invite_channel(channels: &[Value], me: &perms::Me) -> Option<String> {
    let mut usable: Vec<&Value> = channels.iter().filter(|c| matches!(c["type"].as_u64(), Some(0 | 5)) && me.can_invite(c)).collect();
    usable.sort_by_key(|c| c["position"].as_i64().unwrap_or(0));
    usable.first().and_then(|c| c["id"].as_str()).map(String::from)
}

/// What the moderator's view says about someone: dates, nickname, roles, a time-out in force.
pub fn mod_lines(member: &Value, roles: &[Value], created: Option<String>, now: DateTime<Utc>) -> Vec<String> {
    let when = |key: &str| member[key].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Local));
    let mut lines = Vec::new();
    if let Some(at) = created {
        lines.push(crate::i18n::tf("Аккаунт создан {}", &[&at]));
    }
    if let Some(at) = when("joined_at") {
        lines.push(crate::i18n::tf("Участник сервера с {}", &[&crate::clock::date(at.date_naive())]));
    }
    if let Some(nick) = member["nick"].as_str().filter(|n| !n.is_empty()) {
        lines.push(crate::i18n::tf("Никнейм: {}", &[&nick]));
    }
    let theirs: Vec<&str> = member["roles"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut named: Vec<&Value> = roles.iter().filter(|r| r["id"].as_str().is_some_and(|id| theirs.contains(&id))).collect();
    named.sort_by_key(|r| std::cmp::Reverse(r["position"].as_i64().unwrap_or(0)));
    let names: Vec<&str> = named.iter().filter_map(|r| r["name"].as_str()).collect();
    if !names.is_empty() {
        lines.push(crate::i18n::tf("Роли: {}", &[&names.join(", ")]));
    }
    if let Some(until) = when("communication_disabled_until").filter(|u| u.with_timezone(&Utc) > now) {
        lines.push(crate::i18n::tf("Тайм-аут до {}", &[&crate::clock::short(&until)]));
    }
    lines
}

/// The end of a time-out, as Discord takes it; none for no seconds (it is lifted).
pub fn timeout_until(seconds: i64, now: DateTime<Utc>) -> Value {
    if seconds <= 0 {
        return Value::Null;
    }
    json!((now + chrono::Duration::seconds(seconds)).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_choice_turns_on_and_off_again() {
        let (list, on) = toggled(strings(&["1"]), "2");
        assert!(on);
        assert_eq!(list, strings(&["1", "2"]));
        let (list, on) = toggled(list, "1");
        assert!(!on);
        assert_eq!(list, strings(&["2"]));
    }

    #[test]
    fn pinned_chats_come_first_and_keep_their_order() {
        let rows = strings(&["a", "b", "c", "d"]);
        let sorted = pinned_in_front(rows, &strings(&["d", "b"]), |r| r.as_str());
        assert_eq!(sorted, strings(&["b", "d", "a", "c"]));
    }

    #[test]
    fn the_moderators_view_says_what_is_known() {
        let now = DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z").unwrap().with_timezone(&Utc);
        let member = json!({ "nick": "Алекс", "joined_at": "2024-05-19T10:00:00Z", "roles": ["mod", "low"], "communication_disabled_until": "2026-11-03T13:00:00Z" });
        let lines = mod_lines(&member, &server(), Some("2023".into()), now);
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[0], "Аккаунт создан 2023");
        assert_eq!(lines[2], "Никнейм: Алекс");
        assert_eq!(lines[3], "Роли: Модератор, Гость");
        let over = json!({ "communication_disabled_until": "2026-11-03T11:00:00Z" });
        assert!(mod_lines(&over, &server(), None, now).is_empty(), "a time-out that ended isn't told");
    }

    #[test]
    fn a_time_out_ends_in_the_future_or_is_lifted() {
        let now = DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(timeout_until(0, now), Value::Null);
        let until = timeout_until(3600, now);
        let until = DateTime::parse_from_rfc3339(until.as_str().unwrap()).unwrap().with_timezone(&Utc);
        assert_eq!((until - now).num_seconds(), 3600);
    }

    #[test]
    fn nicknames_keep_to_discords_limit() {
        assert_eq!(nick("  Алекс  "), "Алекс");
        assert_eq!(nick(&"я".repeat(40)).chars().count(), 32);
        assert_eq!(nick("   "), "");
    }

    fn server() -> Vec<Value> {
        vec![
            json!({ "id": "g", "name": "@everyone", "position": 0, "permissions": "0" }),
            json!({ "id": "low", "name": "Гость", "position": 1, "permissions": "0" }),
            json!({ "id": "bot", "name": "Бот", "position": 2, "permissions": "0", "managed": true }),
            json!({ "id": "mod", "name": "Модератор", "position": 3, "permissions": (1u64 << 28).to_string() }),
            json!({ "id": "top", "name": "Админ", "position": 4, "permissions": "0" }),
        ]
    }

    #[test]
    fn roles_we_cant_give_show_only_when_they_have_them() {
        let me = perms::Me::new("g", "u", false, &server(), vec!["mod".into()]);
        let rows = role_rows(&me, &strings(&["top"]));
        let seen: Vec<(&str, bool, bool)> = rows.iter().map(|r| (r.id.as_str(), r.has, r.editable)).collect();
        assert_eq!(seen, vec![("top", true, false), ("low", false, true)]);
    }

    #[test]
    fn an_invite_goes_to_the_first_channel_we_may_invite_to() {
        let me = perms::Me::new("g", "u", false, &[json!({ "id": "g", "permissions": "1025" })], vec![]);
        let hidden = json!([{ "id": "g", "type": 0, "allow": "0", "deny": "1024" }]);
        let channels = [
            json!({ "id": "voice", "type": 2, "position": 0 }),
            json!({ "id": "secret", "type": 0, "position": 1, "permission_overwrites": hidden }),
            json!({ "id": "chat", "type": 0, "position": 3 }),
            json!({ "id": "news", "type": 5, "position": 2 }),
        ];
        assert_eq!(invite_channel(&channels, &me), Some("news".to_string()));
        assert_eq!(invite_channel(&channels[..2], &me), None);
    }
}
