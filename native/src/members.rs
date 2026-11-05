//! The member list on the right of a server: groups by role ("Админ — 6",
//! "В сети — 4"), people with their status and what they do. Discord sends it
//! as a window of the whole list (`GUILD_MEMBER_LIST_UPDATE`) after we ask
//! for it with a "lazy guild" subscription.

use std::collections::HashMap;

use serde_json::{json, Value};

/// Chunks of 100 entries asked for at first, and the most ever asked for (1500 entries).
pub const FIRST_CHUNKS: usize = 3;
pub const MAX_CHUNKS: usize = 15;

/// The ranges for that many chunks: [[0, 99], [100, 199], ...] (Discord gives 100 per range).
pub fn ranges(chunks: usize) -> Vec<[u32; 2]> {
    (0..chunks.clamp(1, MAX_CHUNKS) as u32).map(|i| [i * 100, i * 100 + 99]).collect()
}

/// The message that subscribes to a server's list as seen from a channel.
pub fn subscribe(guild: &str, channel: &str, chunks: usize) -> Value {
    json!({
        "op": 14,
        "d": { "guild_id": guild, "typing": true, "threads": true, "activities": true, "members": [], "channels": { channel: ranges(chunks) } }
    })
}

/// The message that stops it. Discord tells a list once per subscription: without this, coming back to a
/// channel asked for earlier brings nothing at all.
pub fn unsubscribe(guild: &str, channel: &str) -> Value {
    json!({ "op": 14, "d": { "guild_id": guild, "channels": { channel: [] } } })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Group {
        id: String,
        count: u64,
    },
    Member {
        id: String,
        user: Value,
        nick: Option<String>,
        roles: Vec<String>,
        status: u8,
        activity: String,
    },
}

/// What a role looks like in the list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Role {
    pub name: String,
    pub color: u32,
    pub position: i64,
}

/// The roles of a server by id, from its role list.
pub fn role_table(roles: &[Value]) -> HashMap<String, Role> {
    roles
        .iter()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_string(),
                Role {
                    name: r["name"].as_str()?.to_string(),
                    color: r["color"].as_u64().unwrap_or(0) as u32,
                    position: r["position"].as_i64().unwrap_or(0),
                },
            ))
        })
        .collect()
}

/// Colour of a name: the highest role that has one.
pub fn name_color(roles: &[String], table: &HashMap<String, Role>) -> Option<u32> {
    roles
        .iter()
        .filter_map(|r| table.get(r))
        .filter(|r| r.color != 0)
        .max_by_key(|r| r.position)
        .map(|r| r.color)
}

/// 1 online, 2 idle, 3 do not disturb, 4 offline (as the status dots use).
fn status_of(presence: &Value) -> u8 {
    match presence["status"].as_str() {
        Some("online") => 1,
        Some("idle") => 2,
        Some("dnd") => 3,
        _ => 4,
    }
}

/// A custom status text, or what they play or listen to.
fn activity_of(presence: &Value) -> String {
    let list = presence["activities"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let custom = list.iter().find(|a| a["type"].as_u64() == Some(4));
    let other = list.iter().find(|a| a["type"].as_u64() != Some(4));
    match (custom, other) {
        (_, Some(a)) => a["name"].as_str().unwrap_or("").to_string(),
        (Some(c), None) => c["state"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

fn item_of(v: &Value) -> Option<Item> {
    if let Some(g) = v["group"].as_object() {
        return Some(Item::Group {
            id: g.get("id")?.as_str()?.to_string(),
            count: g.get("count").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    let m = &v["member"];
    Some(Item::Member {
        id: m["user"]["id"].as_str()?.to_string(),
        user: m["user"].clone(),
        nick: m["nick"].as_str().map(String::from),
        roles: m["roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from))
            .collect(),
        status: status_of(&m["presence"]),
        activity: activity_of(&m["presence"]),
    })
}

/// The list of one server's channel view, kept in step with Discord's updates.
#[derive(Default)]
pub struct List {
    /// Which list Discord means ("everyone" or a hash of the channel's permissions).
    id: String,
    items: Vec<Option<Item>>,
    /// How many people the server has, as Discord says (0 until it did).
    total: u64,
    /// How many each group has, from the list of groups that comes with each update.
    group_counts: HashMap<String, u64>,
}

impl List {
    pub fn clear(&mut self) {
        self.id.clear();
        self.items.clear();
        self.total = 0;
        self.group_counts.clear();
    }

    /// The people (not group headers) received so far, and the server's total.
    pub fn progress(&self) -> (usize, u64) {
        (
            self.items.iter().flatten().filter(|i| matches!(i, Item::Member { .. })).count(),
            self.total,
        )
    }

    /// Whether more chunks are worth asking for: the server has more people than we hold,
    /// and what was asked last has arrived (sked chunks of 100 entries).
    pub fn wants_more(&self, asked: usize) -> bool {
        let (have, total) = self.progress();
        asked < MAX_CHUNKS && (have as u64) < total && self.items.len() + 60 >= asked * 100
    }

    /// What's known so far, in order (gaps not yet received are left out).
    pub fn items(&self) -> Vec<Item> {
        let mut list: Vec<Item> = self.items.iter().flatten().cloned().collect();
        // A group's own line often says nothing of its size: the list of groups does, or else who follows it here.
        for at in 0..list.len() {
            if let Item::Group { id, count: 0 } = &list[at] {
                let told = self.group_counts.get(id).copied();
                let here = list[at + 1..].iter().take_while(|i| matches!(i, Item::Member { .. })).count() as u64;
                let id = id.clone();
                list[at] = Item::Group { id, count: told.filter(|n| *n > 0).unwrap_or(here) };
            }
        }
        list
    }

    /// Applies one `GUILD_MEMBER_LIST_UPDATE`. Returns whether anything changed.
    pub fn apply(&mut self, data: &Value) -> bool {
        let id = data["id"].as_str().unwrap_or("").to_string();
        let ops = data["ops"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let starts_over = ops.iter().any(|o| o["op"].as_str() == Some("SYNC"));
        if id != self.id {
            // Only a fresh SYNC can begin another list; the rest belongs to one we've left.
            if !starts_over {
                return false;
            }
            self.id = id;
            self.items.clear();
        }
        if let Some(n) = data["member_count"].as_u64() {
            self.total = n;
        }
        for group in data["groups"].as_array().into_iter().flatten() {
            if let (Some(id), Some(count)) = (group["id"].as_str(), group["count"].as_u64()) {
                self.group_counts.insert(id.to_string(), count);
            }
        }
        let mut changed = false;
        for op in ops {
            let at = |key: &str| op[key].as_u64().map(|i| i as usize);
            match op["op"].as_str() {
                Some("SYNC") => {
                    let Some(lo) = op["range"][0].as_u64().map(|i| i as usize) else {
                        continue;
                    };
                    let items = op["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                    if self.items.len() < lo + items.len() {
                        self.items.resize(lo + items.len(), None);
                    }
                    for (i, v) in items.iter().enumerate() {
                        self.items[lo + i] = item_of(v);
                    }
                    changed = true;
                }
                Some("INSERT") => {
                    if let (Some(i), Some(item)) = (at("index"), item_of(&op["item"])) {
                        let i = i.min(self.items.len());
                        self.items.insert(i, Some(item));
                        changed = true;
                    }
                }
                Some("UPDATE") => {
                    if let (Some(i), Some(item)) = (at("index"), item_of(&op["item"])) {
                        if i < self.items.len() {
                            self.items[i] = Some(item);
                            changed = true;
                        }
                    }
                }
                Some("DELETE") => {
                    if let Some(i) = at("index").filter(|&i| i < self.items.len()) {
                        self.items.remove(i);
                        changed = true;
                    }
                }
                Some("INVALIDATE") => {
                    let (Some(lo), Some(hi)) = (op["range"][0].as_u64(), op["range"][1].as_u64()) else {
                        continue;
                    };
                    for i in lo as usize..=(hi as usize).min(self.items.len().saturating_sub(1)) {
                        self.items[i] = None;
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }
}

/// "В сети", "Не в сети", or the role's name.
pub fn group_name(id: &str, table: &HashMap<String, Role>) -> String {
    match id {
        "online" => crate::i18n::t("В сети"),
        "offline" => crate::i18n::t("Не в сети"),
        role => table.get(role).map(|r| r.name.clone()).unwrap_or_else(|| crate::i18n::t("Участники")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_a_channel_asks_for_no_ranges_of_it() {
        let off = unsubscribe("g", "c");
        assert_eq!(off["op"], 14);
        assert_eq!(off["d"]["guild_id"], "g");
        assert_eq!(off["d"]["channels"]["c"], json!([]));
    }

    fn member(id: &str, status: &str) -> Value {
        json!({ "member": { "user": { "id": id, "username": id }, "roles": ["r1"], "presence": { "status": status, "activities": [{ "type": 4, "state": "дома" }] } } })
    }

    fn update(id: &str, ops: Value) -> Value {
        json!({ "id": id, "ops": ops })
    }

    #[test]
    fn sync_then_edits() {
        let mut list = List::default();
        let sync = update(
            "everyone",
            json!([{ "op": "SYNC", "range": [0, 2], "items": [
            { "group": { "id": "online", "count": 2 } }, member("a", "online"), member("b", "dnd")] }]),
        );
        assert!(list.apply(&sync));
        assert_eq!(list.items().len(), 3);
        assert!(list.apply(&update(
            "everyone",
            json!([{ "op": "INSERT", "index": 1, "item": member("c", "idle") }])
        )));
        assert!(matches!(&list.items()[1], Item::Member { id, status: 2, .. } if id == "c"));
        assert!(list.apply(&update("everyone", json!([{ "op": "DELETE", "index": 1 }]))));
        assert!(list.apply(&update(
            "everyone",
            json!([{ "op": "UPDATE", "index": 2, "item": member("b", "offline") }])
        )));
        assert!(matches!(&list.items()[2], Item::Member { status: 4, activity, .. } if activity == "дома"));
    }

    #[test]
    fn asks_for_the_next_chunk_only_when_there_is_more_and_the_last_arrived() {
        assert_eq!(ranges(1), vec![[0, 99]]);
        assert_eq!(ranges(3), vec![[0, 99], [100, 199], [200, 299]]);
        assert_eq!(ranges(99).len(), MAX_CHUNKS);
        let mut list = List::default();
        let many: Vec<Value> = (0..100).map(|i| member(&format!("u{i}"), "online")).collect();
        let sync = json!({ "id": "everyone", "member_count": 5000, "ops": [{ "op": "SYNC", "range": [0, 99], "items": many }] });
        assert!(list.apply(&sync));
        assert_eq!(list.progress(), (100, 5000));
        assert!(!list.wants_more(3), "300 were asked for but only 100 arrived: wait");
        assert!(list.wants_more(1), "the one chunk asked for has arrived");
        let small = json!({ "id": "x", "member_count": 40, "ops": [{ "op": "SYNC", "range": [0, 1], "items": [member("a", "online")] }] });
        let mut tiny = List::default();
        tiny.apply(&small);
        assert!(!tiny.wants_more(1), "everyone is here already");
    }

    #[test]
    fn other_lists_are_ignored_until_they_sync() {
        let mut list = List::default();
        list.apply(&update(
            "everyone",
            json!([{ "op": "SYNC", "range": [0, 0], "items": [member("a", "online")] }]),
        ));
        assert!(!list.apply(&update("other", json!([{ "op": "DELETE", "index": 0 }]))));
        assert_eq!(list.items().len(), 1);
        assert!(list.apply(&update(
            "other",
            json!([{ "op": "SYNC", "range": [0, 0], "items": [member("z", "online")] }])
        )));
        assert!(matches!(&list.items()[0], Item::Member { id, .. } if id == "z"));
    }

    #[test]
    fn colours_and_names_from_roles() {
        let table = role_table(&[
            json!({ "id": "r1", "name": "Админ", "color": 0xff0000, "position": 5 }),
            json!({ "id": "r2", "name": "Друг", "color": 0x00ff00, "position": 2 }),
            json!({ "id": "r3", "name": "Без цвета", "color": 0, "position": 9 }),
        ]);
        let roles = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(name_color(&roles(&["r2", "r1", "r3"]), &table), Some(0xff0000));
        assert_eq!(name_color(&roles(&["r3"]), &table), None);
        assert_eq!(group_name("r1", &table), "Админ");
        assert_eq!(group_name("online", &table), "В сети");
    }
}
