//! Stored state: player records (`players.json`) and the skin cache (`skins.json`).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::{parse_skin_key, skin_key};
use crate::source::{SkinData, Source};

/// What a player wants to wear.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Choice {
    /// Their own name's skin, looked up in the configured source order.
    Auto,
    /// A specific account; `name` is lowercase.
    Skin { source: Source, name: String },
}

impl Choice {
    pub fn skin(source: Source, name: &str) -> Choice {
        debug_assert!(crate::source::valid_name(name));
        Choice::Skin {
            source,
            name: name.to_ascii_lowercase(),
        }
    }
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Choice::Auto => f.write_str("auto"),
            Choice::Skin { source, name } => f.write_str(&skin_key(*source, name)),
        }
    }
}

impl From<Choice> for String {
    fn from(c: Choice) -> String {
        c.to_string()
    }
}

impl TryFrom<String> for Choice {
    type Error = String;
    fn try_from(s: String) -> Result<Choice, String> {
        if s == "auto" {
            return Ok(Choice::Auto);
        }
        let (source, name) = parse_skin_key(&s)?;
        Ok(Choice::Skin { source, name })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRecord {
    pub name: String,
    /// Hyphenated UUID as Pumpkin reports it.
    pub uuid: String,
    pub choice: Choice,
    #[serde(default)]
    pub locked: bool,
    /// Previous choices; the last element is the top of the stack.
    #[serde(default)]
    pub history: Vec<Choice>,
}

impl PlayerRecord {
    pub fn new(name: &str, uuid: &str) -> PlayerRecord {
        PlayerRecord {
            name: name.into(),
            uuid: uuid.into(),
            choice: Choice::Auto,
            locked: false,
            history: Vec::new(),
        }
    }

    /// The player's own change: pushes the replaced choice, capped at `cap` entries.
    /// Returns false (and changes nothing) if `choice` is already current.
    pub fn set_own(&mut self, choice: Choice, cap: usize) -> bool {
        if self.choice == choice {
            return false;
        }
        let previous = std::mem::replace(&mut self.choice, choice);
        if self.history.last() != Some(&previous) {
            self.history.push(previous);
        }
        let excess = self.history.len().saturating_sub(cap);
        self.history.drain(..excess);
        true
    }

    /// Pops history entries until one is neither blocked nor the current choice and
    /// makes it current. Pushes nothing, so repeated undo walks further back.
    pub fn undo(&mut self, is_blocked: impl Fn(&Choice) -> bool) -> Option<Choice> {
        while let Some(previous) = self.history.pop() {
            if previous != self.choice && !is_blocked(&previous) {
                self.choice = previous.clone();
                return Some(previous);
            }
        }
        None
    }
}

/// Key of a player record: UUID in online mode, lowercase name in offline mode.
pub fn player_key(online_mode: bool, name: &str, uuid: &str) -> String {
    if online_mode {
        uuid.to_ascii_lowercase()
    } else {
        name.to_ascii_lowercase()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Players {
    pub players: BTreeMap<String, PlayerRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSkin {
    pub value: String,
    pub signature: String,
    /// Account UUID at the source; an empty string means unknown.
    pub uuid: String,
    /// Unix seconds.
    pub fetched_at: u64,
}

impl CachedSkin {
    pub fn skin(&self) -> SkinData {
        SkinData {
            value: self.value.clone(),
            signature: self.signature.clone(),
        }
    }

    /// True once the entry is at least `cache_minutes` old.
    pub fn is_expired(&self, now_secs: u64, cache_minutes: u64) -> bool {
        now_secs.saturating_sub(self.fetched_at) >= cache_minutes.saturating_mul(60)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// Keyed by `source:lowercase-name`.
    pub skins: BTreeMap<String, CachedSkin>,
}

impl Cache {
    pub fn get(&self, source: Source, name: &str) -> Option<&CachedSkin> {
        self.skins.get(&skin_key(source, name))
    }

    pub fn put(&mut self, source: Source, name: &str, skin: CachedSkin) {
        self.skins.insert(skin_key(source, name), skin);
    }
}

/// Reads a JSON state file. A missing file is empty state; a corrupt one is an error,
/// so it is never overwritten.
pub fn load<T: for<'de> Deserialize<'de> + Default>(text: Option<&str>) -> Result<T, String> {
    match text {
        None => Ok(T::default()),
        Some(t) => serde_json::from_str(t).map_err(|e| e.to_string()),
    }
}

pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("state always serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin(name: &str) -> Choice {
        Choice::skin(Source::Mojang, name)
    }

    #[test]
    fn choice_serialises_as_a_string() {
        assert_eq!(serde_json::to_string(&Choice::Auto).unwrap(), "\"auto\"");
        let c = Choice::skin(Source::Ely, "Notch");
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"ely:notch\"");
        let back: Choice = serde_json::from_str("\"ely:notch\"").unwrap();
        assert_eq!(back, c);
        assert!(serde_json::from_str::<Choice>("\"nope\"").is_err());
    }

    #[test]
    fn set_own_pushes_the_replaced_choice() {
        let mut p = PlayerRecord::new("Alice", "u");
        assert!(p.set_own(skin("a"), 36));
        assert!(p.set_own(skin("b"), 36));
        assert_eq!(p.history, vec![Choice::Auto, skin("a")]);
        assert!(!p.set_own(skin("b"), 36));
        assert_eq!(p.history.len(), 2);
    }

    #[test]
    fn repeated_undo_walks_back() {
        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("a"), 36);
        p.set_own(skin("b"), 36);
        p.set_own(skin("c"), 36);
        assert_eq!(p.undo(|_| false), Some(skin("b")));
        assert_eq!(p.undo(|_| false), Some(skin("a")));
        assert_eq!(p.undo(|_| false), Some(Choice::Auto));
        assert_eq!(p.undo(|_| false), None);
        assert_eq!(p.choice, Choice::Auto);
    }

    #[test]
    fn undo_skips_blocked_entries() {
        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("a"), 36);
        p.set_own(skin("b"), 36);
        p.set_own(skin("c"), 36);
        assert_eq!(p.undo(|c| *c == skin("b")), Some(skin("a")));
        assert_eq!(p.choice, skin("a"));
        assert_eq!(p.history, vec![Choice::Auto]);
    }

    #[test]
    fn history_is_capped_oldest_first() {
        let mut p = PlayerRecord::new("Alice", "u");
        for n in ["a", "b", "c", "d"] {
            p.set_own(skin(n), 2);
        }
        assert_eq!(p.history, vec![skin("b"), skin("c")]);
    }

    #[test]
    fn duplicate_top_is_not_pushed() {
        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("a"), 36);
        p.choice = Choice::Auto; // admin clear: history untouched
        p.set_own(skin("b"), 36);
        assert_eq!(p.history, vec![Choice::Auto]);
    }

    #[test]
    fn player_key_depends_on_mode() {
        assert_eq!(player_key(false, "Alice", "AB-CD"), "alice");
        assert_eq!(player_key(true, "Alice", "AB-CD"), "ab-cd");
    }

    #[test]
    fn cache_expiry() {
        let c = CachedSkin {
            value: "v".into(),
            signature: "s".into(),
            uuid: "u".into(),
            fetched_at: 1000,
        };
        assert!(!c.is_expired(1000 + 15 * 60 - 1, 15));
        assert!(c.is_expired(1000 + 15 * 60, 15));
    }

    #[test]
    fn state_files_round_trip_and_reject_corruption() {
        let mut players = Players::default();
        players
            .players
            .insert("alice".into(), PlayerRecord::new("Alice", "u"));
        let text = to_json(&players);
        assert_eq!(load::<Players>(Some(&text)).unwrap(), players);
        assert_eq!(load::<Players>(None).unwrap(), Players::default());
        assert!(load::<Players>(Some("{broken")).is_err());
    }

    #[test]
    fn undo_skips_entries_equal_to_the_current_choice() {
        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("a"), 36);
        p.choice = Choice::Auto; // admin clear
        assert_eq!(p.undo(|_| false), None);
        assert_eq!(p.choice, Choice::Auto);
        assert!(p.history.is_empty());

        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("b"), 36);
        p.set_own(skin("c"), 36);
        p.choice = skin("b"); // admin set
        assert_eq!(p.undo(|_| false), Some(Choice::Auto));
        assert_eq!(p.choice, Choice::Auto);
        assert!(p.history.is_empty());
    }

    #[test]
    fn undo_with_everything_blocked_changes_nothing() {
        let mut p = PlayerRecord::new("Alice", "u");
        p.set_own(skin("a"), 36);
        p.set_own(skin("b"), 36);
        assert_eq!(p.undo(|_| true), None);
        assert_eq!(p.choice, skin("b"));
        assert!(p.history.is_empty());
    }

    #[test]
    fn zero_cap_keeps_the_history_empty() {
        let mut p = PlayerRecord::new("Alice", "u");
        assert!(p.set_own(skin("a"), 0));
        assert!(p.set_own(skin("b"), 0));
        assert_eq!(p.choice, skin("b"));
        assert!(p.history.is_empty());
    }

    #[test]
    fn cache_entry_from_the_future_is_not_expired() {
        let c = CachedSkin {
            value: "v".into(),
            signature: "s".into(),
            uuid: String::new(),
            fetched_at: 2000,
        };
        assert!(!c.is_expired(1000, 15));
        assert!(!c.is_expired(1000, u64::MAX));
    }

    #[test]
    fn cache_keys_ignore_case() {
        let mut cache = Cache::default();
        let entry = CachedSkin {
            value: "v".into(),
            signature: "s".into(),
            uuid: "u".into(),
            fetched_at: 1000,
        };
        cache.put(Source::Ely, "NoTcH", entry.clone());
        assert_eq!(cache.get(Source::Ely, "notch"), Some(&entry));
        assert_eq!(cache.get(Source::Ely, "NOTCH"), Some(&entry));
        assert_eq!(cache.get(Source::Mojang, "notch"), None);
        assert_eq!(cache.skins.keys().collect::<Vec<_>>(), ["ely:notch"]);
    }

    #[test]
    fn full_player_record_round_trips() {
        let mut record = PlayerRecord::new("Alice", "u");
        record.choice = Choice::skin(Source::Ely, "Notch");
        record.locked = true;
        record.history = vec![Choice::Auto, skin("a")];
        let mut players = Players::default();
        players.players.insert("alice".into(), record);
        let text = to_json(&players);
        assert_eq!(load::<Players>(Some(&text)).unwrap(), players);
    }

    #[test]
    fn cache_round_trips() {
        let mut cache = Cache::default();
        cache.put(
            Source::Mojang,
            "Notch",
            CachedSkin {
                value: "v".into(),
                signature: "s".into(),
                uuid: "u".into(),
                fetched_at: 1000,
            },
        );
        let text = to_json(&cache);
        assert_eq!(load::<Cache>(Some(&text)).unwrap(), cache);
    }

    #[test]
    fn missing_locked_and_history_default() {
        let text = r#"{"players":{"alice":{"name":"Alice","uuid":"u","choice":"auto"}}}"#;
        let players = load::<Players>(Some(text)).unwrap();
        assert_eq!(players.players["alice"], PlayerRecord::new("Alice", "u"));
    }

    #[test]
    fn invalid_choice_string_is_rejected() {
        let text = r#"{"players":{"alice":{"name":"Alice","uuid":"u","choice":"nope"}}}"#;
        assert!(load::<Players>(Some(text)).is_err());
    }
}
