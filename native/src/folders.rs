//! The order of the server rail and its folders, kept in the settings file. It stays on this
//! computer: Discord's own folders aren't read or changed.

use std::sync::Mutex;

use serde_json::{json, Value};

const KEY: &str = "server_rail";

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Server(String),
    Folder { id: u32, servers: Vec<String>, open: bool },
}

/// A line of the rail as shown.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Server { id: String, folder: Option<u32>, last: bool },
    Folder { id: u32, open: bool, servers: Vec<String> },
}

/// Where on a line something was dropped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Before,
    Onto,
    After,
}

impl Zone {
    pub fn from_int(n: i32) -> Self {
        match n {
            0 => Zone::Before,
            1 => Zone::Onto,
            _ => Zone::After,
        }
    }
}

/// A place for what is dragged, named by what stays: rows move as soon as it is taken out.
#[derive(Debug, Clone, PartialEq)]
enum Place {
    BeforeEntry(Entry),
    AfterEntry(Entry),
    BeforeInFolder(String),
    AfterInFolder(String),
    FolderStart(u32),
    FolderEnd(u32),
    NewFolderWith(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rail(pub Vec<Entry>);

/// Text of a folder on the rail: `folder:3`.
pub fn folder_key(id: u32) -> String {
    format!("folder:{id}")
}

fn folder_of_key(key: &str) -> Option<u32> {
    key.strip_prefix("folder:")?.parse().ok()
}

impl Rail {
    fn from_json(v: &Value) -> Self {
        let entry = |e: &Value| {
            if let Some(id) = e.as_str() {
                return Some(Entry::Server(id.to_string()));
            }
            let servers = e["servers"].as_array()?.iter().filter_map(|s| s.as_str().map(String::from)).collect();
            Some(Entry::Folder { id: e["id"].as_u64()? as u32, servers, open: e["open"].as_bool().unwrap_or(false) })
        };
        Rail(v.as_array().into_iter().flatten().filter_map(entry).collect())
    }

    fn to_json(&self) -> Value {
        let entries: Vec<Value> = self
            .0
            .iter()
            .map(|e| match e {
                Entry::Server(id) => json!(id),
                Entry::Folder { id, servers, open } => json!({ "id": id, "servers": servers, "open": open }),
            })
            .collect();
        json!(entries)
    }

    fn contains(&self, server: &str) -> bool {
        self.0.iter().any(|e| match e {
            Entry::Server(id) => id == server,
            Entry::Folder { servers, .. } => servers.iter().any(|s| s == server),
        })
    }

    /// New servers go to the end. Servers that are away stay in the list, so they keep their place
    /// if they come back (and the made-up servers of `--demo` can't wipe the real order).
    fn take_in(&mut self, present: &[String]) {
        for id in present {
            if !self.contains(id) {
                self.0.push(Entry::Server(id.clone()));
            }
        }
    }

    pub fn rows(&self, present: &[String]) -> Vec<Row> {
        let mut all = self.clone();
        all.take_in(present);
        let here = |id: &String| present.contains(id);
        let mut rows = Vec::new();
        for entry in &all.0 {
            match entry {
                Entry::Server(id) if here(id) => rows.push(Row::Server { id: id.clone(), folder: None, last: false }),
                Entry::Folder { id, servers, open } => {
                    let inside: Vec<String> = servers.iter().filter(|s| here(s)).cloned().collect();
                    if inside.is_empty() {
                        continue;
                    }
                    rows.push(Row::Folder { id: *id, open: *open, servers: inside.clone() });
                    if *open {
                        let count = inside.len();
                        rows.extend(inside.into_iter().enumerate().map(|(i, s)| Row::Server { id: s, folder: Some(*id), last: i + 1 == count }));
                    }
                }
                Entry::Server(_) => {}
            }
        }
        rows
    }

    pub fn toggle(&mut self, key: &str) {
        let Some(wanted) = folder_of_key(key) else { return };
        for entry in &mut self.0 {
            if let Entry::Folder { id, open, .. } = entry {
                if *id == wanted {
                    *open = !*open;
                }
            }
        }
    }

    fn folder_entry(&self, wanted: u32) -> Option<Entry> {
        self.0.iter().find(|e| matches!(e, Entry::Folder { id, .. } if *id == wanted)).cloned()
    }

    /// What dropping `from` on a row means, or nothing when it stays where it is.
    fn place_for(&self, from: &str, target: &Row, zone: Zone) -> Option<Place> {
        let dragged_folder = folder_of_key(from);
        // A folder doesn't go into a folder: it lands beside what it was dropped on.
        let zone = if dragged_folder.is_some() && zone == Zone::Onto { Zone::Before } else { zone };
        match target {
            Row::Folder { id, .. } if dragged_folder == Some(*id) => None,
            Row::Server { id, .. } if id == from => None,
            Row::Server { folder: Some(f), .. } if dragged_folder == Some(*f) => None,
            Row::Server { id, folder: Some(f), last } => {
                if zone == Zone::Onto {
                    let already = matches!(self.folder_entry(*f), Some(Entry::Folder { servers, .. }) if servers.iter().any(|s| s == from));
                    return (!already).then_some(Place::FolderEnd(*f));
                }
                let beside_folder = dragged_folder.is_some();
                let entry = self.folder_entry(*f)?;
                match zone {
                    Zone::After if *last || beside_folder => Some(Place::AfterEntry(entry)),
                    Zone::Before if beside_folder => Some(Place::BeforeEntry(entry)),
                    Zone::After => Some(Place::AfterInFolder(id.clone())),
                    _ => Some(Place::BeforeInFolder(id.clone())),
                }
            }
            Row::Server { id, .. } => Some(match zone {
                Zone::Before => Place::BeforeEntry(Entry::Server(id.clone())),
                Zone::After => Place::AfterEntry(Entry::Server(id.clone())),
                Zone::Onto => Place::NewFolderWith(id.clone()),
            }),
            Row::Folder { id, open, .. } => {
                let entry = self.folder_entry(*id)?;
                Some(match zone {
                    Zone::Before => Place::BeforeEntry(entry),
                    Zone::Onto => Place::FolderEnd(*id),
                    Zone::After if *open && dragged_folder.is_none() => Place::FolderStart(*id),
                    Zone::After => Place::AfterEntry(entry),
                })
            }
        }
    }

    fn remove(&mut self, key: &str) -> Option<Entry> {
        if let Some(wanted) = folder_of_key(key) {
            let at = self.0.iter().position(|e| matches!(e, Entry::Folder { id, .. } if *id == wanted))?;
            return Some(self.0.remove(at));
        }
        for entry in &mut self.0 {
            if let Entry::Folder { servers, .. } = entry {
                servers.retain(|s| s != key);
            }
        }
        if let Some(at) = self.0.iter().position(|e| matches!(e, Entry::Server(id) if id == key)) {
            self.0.remove(at);
        }
        Some(Entry::Server(key.to_string()))
    }

    fn folder_with(&mut self, server: &str) -> Option<&mut Vec<String>> {
        self.0.iter_mut().find_map(|e| match e {
            Entry::Folder { servers, .. } if servers.iter().any(|s| s == server) => Some(servers),
            _ => None,
        })
    }

    fn folder_mut(&mut self, wanted: u32) -> Option<&mut Vec<String>> {
        self.0.iter_mut().find_map(|e| match e {
            Entry::Folder { id, servers, .. } if *id == wanted => Some(servers),
            _ => None,
        })
    }

    /// Where an entry is, by what it is: a folder may have lost a server by now.
    fn index_of(&self, anchor: &Entry) -> Option<usize> {
        self.0.iter().position(|e| match (e, anchor) {
            (Entry::Folder { id: a, .. }, Entry::Folder { id: b, .. }) => a == b,
            (Entry::Server(a), Entry::Server(b)) => a == b,
            _ => false,
        })
    }

    fn put(&mut self, what: Entry, place: Place) {
        let server = match &what {
            Entry::Server(id) => Some(id.clone()),
            Entry::Folder { .. } => None,
        };
        match (place, server) {
            (Place::BeforeEntry(anchor), _) => {
                let at = self.index_of(&anchor).unwrap_or(self.0.len());
                self.0.insert(at, what);
            }
            (Place::AfterEntry(anchor), _) => {
                let at = self.index_of(&anchor).map_or(self.0.len(), |i| i + 1);
                self.0.insert(at, what);
            }
            (Place::FolderStart(f), Some(id)) => {
                if let Some(list) = self.folder_mut(f) {
                    list.insert(0, id);
                }
            }
            (Place::FolderEnd(f), Some(id)) => {
                if let Some(list) = self.folder_mut(f) {
                    list.push(id);
                }
            }
            (Place::BeforeInFolder(anchor), Some(id)) => {
                if let Some(list) = self.folder_with(&anchor) {
                    let at = list.iter().position(|s| *s == anchor).unwrap_or(list.len());
                    list.insert(at, id);
                }
            }
            (Place::AfterInFolder(anchor), Some(id)) => {
                if let Some(list) = self.folder_with(&anchor) {
                    let at = list.iter().position(|s| *s == anchor).map_or(list.len(), |i| i + 1);
                    list.insert(at, id);
                }
            }
            (Place::NewFolderWith(anchor), Some(id)) => {
                let new = self.0.iter().filter_map(|e| if let Entry::Folder { id, .. } = e { Some(*id) } else { None }).max().unwrap_or(0) + 1;
                if let Some(at) = self.index_of(&Entry::Server(anchor.clone())) {
                    self.0[at] = Entry::Folder { id: new, servers: vec![anchor, id], open: false };
                }
            }
            _ => {}
        }
    }

    /// A folder needs two servers: an empty one goes, the last server left steps out in its place.
    fn tidy(&mut self) {
        self.0.retain(|e| !matches!(e, Entry::Folder { servers, .. } if servers.is_empty()));
        for entry in &mut self.0 {
            if let Entry::Folder { servers, .. } = entry {
                if servers.len() == 1 {
                    *entry = Entry::Server(servers.remove(0));
                }
            }
        }
    }

    /// `from` (a server's id or a folder's key) dropped on line `row` of `rows`.
    pub fn dropped(&mut self, present: &[String], from: &str, row: usize, zone: Zone) {
        self.take_in(present);
        let rows = self.rows(present);
        let Some(target) = rows.get(row) else { return };
        let Some(place) = self.place_for(from, target, zone) else { return };
        if let Some(what) = self.remove(from) {
            self.put(what, place);
        }
        self.tidy();
    }
}

static RAIL: Mutex<Option<Rail>> = Mutex::new(None);

fn with_rail<T>(use_it: impl FnOnce(&mut Rail) -> T) -> T {
    let mut kept = RAIL.lock().unwrap();
    use_it(kept.get_or_insert_with(|| Rail::from_json(&crate::prefs::read()[KEY])))
}

pub fn rows(present: &[String]) -> Vec<Row> {
    with_rail(|rail| rail.rows(present))
}

pub fn toggle(key: &str) {
    with_rail(|rail| {
        rail.toggle(key);
        crate::prefs::write(KEY, rail.to_json());
    });
}

/// A server we left: it goes from the order and from its folder.
pub fn forget(server: &str) {
    with_rail(|rail| {
        rail.remove(server);
        rail.tidy();
        crate::prefs::write(KEY, rail.to_json());
    });
}

pub fn dropped(present: &[String], from: &str, row: usize, zone: Zone) {
    with_rail(|rail| {
        rail.dropped(present, from, row, zone);
        crate::prefs::write(KEY, rail.to_json());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn folder(id: u32, servers: &[&str], open: bool) -> Entry {
        Entry::Folder { id, servers: ids(servers), open }
    }

    fn server(id: &str) -> Entry {
        Entry::Server(id.to_string())
    }

    #[test]
    fn a_server_dropped_on_a_server_makes_a_folder() {
        let present = ids(&["a", "b", "c"]);
        let mut rail = Rail::default();
        rail.dropped(&present, "c", 0, Zone::Onto);
        assert_eq!(rail.0, vec![folder(1, &["a", "c"], false), server("b")]);
        assert_eq!(rail.rows(&present).len(), 2, "closed: the two servers are inside");
    }

    #[test]
    fn more_servers_join_a_folder() {
        let present = ids(&["a", "b", "c"]);
        let mut rail = Rail(vec![folder(1, &["a", "b"], false), server("c")]);
        rail.dropped(&present, "c", 0, Zone::Onto);
        assert_eq!(rail.0, vec![folder(1, &["a", "b", "c"], false)]);
    }

    #[test]
    fn a_click_opens_a_folder_and_shows_its_servers() {
        let present = ids(&["a", "b"]);
        let mut rail = Rail(vec![folder(1, &["a", "b"], false)]);
        rail.toggle(&folder_key(1));
        let rows = rail.rows(&present);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], Row::Server { id: "b".into(), folder: Some(1), last: true });
        rail.toggle(&folder_key(1));
        assert_eq!(rail.rows(&present).len(), 1);
    }

    #[test]
    fn a_server_leaves_a_folder_and_the_last_one_steps_out() {
        let present = ids(&["a", "b", "c", "d"]);
        let mut rail = Rail(vec![folder(1, &["a", "b", "c"], true), server("d")]);
        // Rows: folder, a, b, c, d. Before d is outside the folder.
        rail.dropped(&present, "c", 4, Zone::Before);
        assert_eq!(rail.0, vec![folder(1, &["a", "b"], true), server("c"), server("d")]);
        rail.dropped(&present, "b", 2, Zone::After);
        assert_eq!(rail.0, vec![folder(1, &["a", "b"], true), server("c"), server("d")], "dropped on itself");
        rail.dropped(&present, "b", 3, Zone::After);
        assert_eq!(rail.0, vec![server("a"), server("c"), server("b"), server("d")]);
    }

    #[test]
    fn servers_change_places_inside_a_folder() {
        let present = ids(&["a", "b", "c"]);
        let mut rail = Rail(vec![folder(1, &["a", "b", "c"], true)]);
        rail.dropped(&present, "c", 1, Zone::Before);
        assert_eq!(rail.0, vec![folder(1, &["c", "a", "b"], true)]);
    }

    #[test]
    fn a_folder_moves_whole_and_never_goes_into_a_folder() {
        let present = ids(&["a", "b", "c", "d", "e"]);
        let mut rail = Rail(vec![folder(1, &["a", "b"], false), server("c"), folder(2, &["d", "e"], false)]);
        rail.dropped(&present, "folder:2", 0, Zone::Onto);
        assert_eq!(rail.0, vec![folder(2, &["d", "e"], false), folder(1, &["a", "b"], false), server("c")]);
        rail.dropped(&present, "folder:2", 2, Zone::After);
        assert_eq!(rail.0, vec![folder(1, &["a", "b"], false), server("c"), folder(2, &["d", "e"], false)]);
    }

    #[test]
    fn servers_that_are_away_keep_their_place() {
        let mut rail = Rail(vec![server("gone"), folder(1, &["a", "away"], false)]);
        let present = ids(&["a", "new"]);
        assert_eq!(rail.rows(&present)[0], Row::Folder { id: 1, open: false, servers: ids(&["a"]) });
        rail.dropped(&present, "new", 0, Zone::Onto);
        assert_eq!(rail.0, vec![server("gone"), folder(1, &["a", "away", "new"], false)]);
    }

    #[test]
    fn a_server_we_left_goes_from_its_folder() {
        let mut rail = Rail(vec![folder(1, &["a", "b"], true), server("c")]);
        rail.remove("a");
        rail.tidy();
        assert_eq!(rail.0, vec![server("b"), server("c")]);
    }

    #[test]
    fn the_rail_survives_the_settings_file() {
        let rail = Rail(vec![server("a"), folder(4, &["b", "c"], true)]);
        assert_eq!(Rail::from_json(&rail.to_json()), rail);
        assert_eq!(Rail::from_json(&json!(null)), Rail::default());
    }
}
