//! Animated pictures kept between chats and between runs: decoded in memory, as files on disk.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

/// Decoded frames in memory: a few hundred ordinary GIFs.
pub const MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// The files on disk, and how long one is kept unused.
const DISK_BYTES: u64 = 300 * 1024 * 1024;
const DISK_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

/// What stays the same for a file: Discord's addresses of attachments change their signature in the query.
pub fn key(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let signed = path.contains("cdn.discordapp.com/attachments/") || path.contains("media.discordapp.net/attachments/");
    if !signed {
        return url.to_string();
    }
    path.replace("media.discordapp.net/attachments/", "cdn.discordapp.com/attachments/")
}

/// The most recently used things, up to a size.
pub struct Kept<T> {
    limit: usize,
    used: usize,
    order: VecDeque<String>,
    items: HashMap<String, (T, usize)>,
}

impl<T: Clone> Kept<T> {
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0, order: VecDeque::new(), items: HashMap::new() }
    }

    pub fn get(&mut self, key: &str) -> Option<T> {
        let item = self.items.get(key)?.0.clone();
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_string());
        Some(item)
    }

    pub fn put(&mut self, key: String, item: T, size: usize) {
        // Bigger than all the room: not worth pushing everything else out.
        if size > self.limit {
            return;
        }
        if let Some((_, old)) = self.items.remove(&key) {
            self.used -= old;
            self.order.retain(|k| *k != key);
        }
        self.used += size;
        self.order.push_back(key.clone());
        self.items.insert(key, (item, size));
        while self.used > self.limit {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some((_, size)) = self.items.remove(&oldest) {
                self.used -= size;
            }
        }
    }
}

fn dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Nimbus Native").join("gifs"))
}

/// Where a file is kept; only for the media hosts Discord itself uses.
pub fn path_of(url: &str, trusted: bool) -> Option<PathBuf> {
    if !trusted {
        return None;
    }
    let name = key(url).bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    Some(dir()?.join(format!("{name:016x}.gif")))
}

/// Which of the files to delete: old ones, then the longest unused until the rest fits. `(name, size, age)`.
fn to_remove(mut files: Vec<(PathBuf, u64, Duration)>, limit: u64, max_age: Duration) -> Vec<PathBuf> {
    files.sort_by_key(|f| std::cmp::Reverse(f.2));
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let mut gone = Vec::new();
    for (path, size, age) in files {
        if age > max_age || total > limit {
            total -= size;
            gone.push(path);
        }
    }
    gone
}

/// At start: keeps the folder to its size.
pub fn tidy() {
    let Some(dir) = dir() else { return };
    let files = std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|entry| {
        let meta = entry.metadata().ok()?;
        Some((entry.path(), meta.len(), meta.modified().ok()?.elapsed().unwrap_or_default()))
    });
    for path in to_remove(files.collect(), DISK_BYTES, DISK_AGE) {
        let _ = std::fs::remove_file(path);
    }
}

/// Signing out: the next account starts with nothing of this one's.
pub fn clear() {
    if let Some(dir) = dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_signature_is_the_same_file() {
        let one = "https://cdn.discordapp.com/attachments/1/2/cat.gif?ex=1&is=2&hm=3";
        let two = "https://media.discordapp.net/attachments/1/2/cat.gif?ex=9&is=8&hm=7";
        assert_eq!(key(one), key(two));
        let tenor = "https://media.tenor.com/abc/cat.gif?x=1";
        assert_eq!(key(tenor), tenor, "other addresses stay whole");
        assert!(path_of(one, false).is_none(), "only trusted hosts");
    }

    #[test]
    fn the_longest_unused_goes_when_there_is_no_room() {
        let mut kept = Kept::new(10);
        kept.put("a".into(), 1, 4);
        kept.put("b".into(), 2, 4);
        assert_eq!(kept.get("a"), Some(1));
        kept.put("c".into(), 3, 4);
        assert_eq!(kept.get("b"), None, "b was used longest ago");
        assert_eq!(kept.get("a"), Some(1));
        kept.put("huge".into(), 4, 11);
        assert_eq!(kept.get("huge"), None);
        assert_eq!(kept.get("c"), Some(3), "something too big pushes nothing out");
        kept.put("c".into(), 5, 2);
        assert_eq!(kept.get("c"), Some(5));
    }

    #[test]
    fn the_folder_keeps_to_its_size_and_age() {
        let day = Duration::from_secs(24 * 3600);
        let file = |name: &str, size: u64, days: u32| (PathBuf::from(name), size, day * days);
        let gone = to_remove(vec![file("new", 40, 1), file("old", 40, 5), file("ancient", 1, 30)], 50, day * 14);
        assert_eq!(gone, vec![PathBuf::from("ancient"), PathBuf::from("old")]);
        assert!(to_remove(vec![file("a", 10, 1)], 50, day * 14).is_empty());
    }
}
