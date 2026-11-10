//! `config.toml`: parsing, validation and edits that keep the owner's comments.

use toml_edit::{Array, DocumentMut, Item, Value};

use crate::source::{Source, valid_name};

/// Written when no config file exists yet.
pub const DEFAULT_CONFIG: &str = r#"# pumpkin-skinrestorer configuration. `/skin reload` re-reads this file.

# Sources tried, in order, for names given without an `ely:` / `mojang:` prefix.
source_order = ["ely", "mojang"]

# Minutes before a cached skin is fetched again (it is still used until then).
cache_minutes = 15

# Operator level (0-4) that makes a player an admin.
admin_op_level = 4

# Player UUIDs that are admins regardless of operator level.
admins = []

# Previous skin choices kept per player for `/skin undo`.
history_length = 36

# Blocked skins as "source:name", e.g. "mojang:Notch". `/skin block` edits this list.
blocked = []
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub source_order: Vec<Source>,
    pub cache_minutes: u64,
    pub admin_op_level: u8,
    /// Lowercase, hyphenated UUIDs.
    pub admins: Vec<String>,
    pub history_length: usize,
    /// Normalised `source:lowercase-name` entries.
    pub blocked: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        parse(DEFAULT_CONFIG).expect("DEFAULT_CONFIG is valid")
    }
}

impl Config {
    pub fn is_blocked(&self, source: Source, name: &str) -> bool {
        let key = skin_key(source, name);
        self.blocked.contains(&key)
    }

    pub fn is_admin_uuid(&self, uuid: &str) -> bool {
        let uuid = uuid.to_ascii_lowercase();
        self.admins.contains(&uuid)
    }
}

/// `source:lowercase-name`, the key for the block list and the skin cache.
pub fn skin_key(source: Source, name: &str) -> String {
    format!("{}:{}", source.as_str(), name.to_ascii_lowercase())
}

/// Parses `source:name` as used by `/skin block`.
pub fn parse_skin_key(entry: &str) -> Result<(Source, String), String> {
    let (source, name) = entry
        .split_once(':')
        .ok_or_else(|| format!("`{entry}` is not source:name"))?;
    let source = Source::parse(source).ok_or_else(|| format!("unknown source `{source}`"))?;
    if !valid_name(name) {
        return Err(format!("`{name}` is not a valid account name"));
    }
    Ok((source, name.to_ascii_lowercase()))
}

fn strings(doc: &DocumentMut, key: &str) -> Result<Vec<String>, String> {
    let Some(item) = doc.get(key) else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or_else(|| format!("`{key}` must be a list of strings"))?;
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("`{key}` must be a list of strings"))
        })
        .collect()
}

fn integer(doc: &DocumentMut, key: &str, default: i64, range: (i64, i64)) -> Result<i64, String> {
    let value = match doc.get(key) {
        None => default,
        Some(item) => item
            .as_integer()
            .ok_or_else(|| format!("`{key}` must be an integer"))?,
    };
    if value < range.0 || value > range.1 {
        return Err(format!(
            "`{key}` must be between {} and {}",
            range.0, range.1
        ));
    }
    Ok(value)
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn parse(text: &str) -> Result<Config, String> {
    let doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;

    let mut source_order = Vec::new();
    for s in strings(&doc, "source_order")? {
        let source = Source::parse(&s).ok_or_else(|| format!("unknown source `{s}`"))?;
        if source_order.contains(&source) {
            return Err(format!("source `{s}` is listed twice"));
        }
        source_order.push(source);
    }
    if doc.get("source_order").is_none() {
        source_order = vec![Source::Ely, Source::Mojang];
    }
    if source_order.is_empty() {
        return Err("`source_order` must list at least one source".into());
    }

    let mut admins = Vec::new();
    for a in strings(&doc, "admins")? {
        if !is_uuid(&a) {
            return Err(format!("`{a}` in `admins` is not a hyphenated UUID"));
        }
        admins.push(a.to_ascii_lowercase());
    }

    let mut blocked = Vec::new();
    for b in strings(&doc, "blocked")? {
        let (source, name) = parse_skin_key(&b)?;
        blocked.push(skin_key(source, &name));
    }

    Ok(Config {
        source_order,
        cache_minutes: integer(&doc, "cache_minutes", 15, (1, 525_600))? as u64,
        admin_op_level: integer(&doc, "admin_op_level", 4, (0, 4))? as u8,
        admins,
        history_length: integer(&doc, "history_length", 36, (0, 1000))? as usize,
        blocked,
    })
}

/// Adds `entry` (already normalised `source:name`) to `blocked`, keeping comments and
/// layout. Returns the new file text, or `None` if it was already blocked.
pub fn add_blocked(text: &str, entry: &str) -> Result<Option<String>, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    if !doc.contains_key("blocked") {
        doc["blocked"] = Item::Value(Value::Array(Array::new()));
    }
    let array = doc["blocked"]
        .as_array_mut()
        .ok_or("`blocked` must be a list of strings")?;
    if array
        .iter()
        .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(entry)))
    {
        return Ok(None);
    }
    array.push(entry);
    Ok(Some(doc.to_string()))
}

/// Removes `entry` from `blocked`. Returns the new text, or `None` if it was not there.
pub fn remove_blocked(text: &str, entry: &str) -> Result<Option<String>, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    let Some(array) = doc.get_mut("blocked").and_then(Item::as_array_mut) else {
        return Ok(None);
    };
    let before = array.len();
    array.retain(|v| !v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(entry)));
    if array.len() == before {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let c = Config::default();
        assert_eq!(c.source_order, vec![Source::Ely, Source::Mojang]);
        assert_eq!(c.cache_minutes, 15);
        assert_eq!(c.admin_op_level, 4);
        assert_eq!(c.history_length, 36);
        assert!(c.admins.is_empty() && c.blocked.is_empty());
    }

    #[test]
    fn missing_keys_use_defaults() {
        let c = parse("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn values_are_normalised() {
        let c = parse(
            r#"
            source_order = ["mojang"]
            admins = ["069A79F4-44E9-4726-A5BE-FCA90E38AAF5"]
            blocked = ["Mojang:Notch"]
            "#,
        )
        .unwrap();
        assert_eq!(c.source_order, vec![Source::Mojang]);
        assert!(c.is_admin_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5"));
        assert!(c.is_blocked(Source::Mojang, "NOTCH"));
        assert!(!c.is_blocked(Source::Ely, "notch"));
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(parse(r#"source_order = ["minecraft"]"#).is_err());
        assert!(parse(r#"source_order = ["ely", "ely"]"#).is_err());
        assert!(parse("source_order = []").is_err());
        assert!(parse("admin_op_level = 5").is_err());
        assert!(parse("cache_minutes = 0").is_err());
        assert!(parse(r#"admins = ["not-a-uuid"]"#).is_err());
        assert!(parse(r#"blocked = ["Notch"]"#).is_err());
        assert!(parse("cache_minutes = \"x\"").is_err());
        assert!(parse("this is not toml").is_err());
    }

    #[test]
    fn block_edits_keep_comments() {
        let text = "# owner note\nblocked = [] # inline\n";
        let added = add_blocked(text, "mojang:notch").unwrap().unwrap();
        assert!(added.contains("# owner note"));
        assert!(parse(&added).unwrap().is_blocked(Source::Mojang, "notch"));
        assert_eq!(add_blocked(&added, "MOJANG:NOTCH").unwrap(), None);

        let removed = remove_blocked(&added, "mojang:notch").unwrap().unwrap();
        assert!(removed.contains("# owner note"));
        assert!(!parse(&removed).unwrap().is_blocked(Source::Mojang, "notch"));
        assert_eq!(remove_blocked(&removed, "mojang:notch").unwrap(), None);
    }

    #[test]
    fn block_adds_the_key_when_missing() {
        let added = add_blocked("cache_minutes = 5\n", "ely:steve")
            .unwrap()
            .unwrap();
        assert!(parse(&added).unwrap().is_blocked(Source::Ely, "steve"));
    }

    #[test]
    fn skin_keys_parse() {
        assert_eq!(
            parse_skin_key("ELY:Notch").unwrap(),
            (Source::Ely, "notch".to_string())
        );
        assert!(parse_skin_key("notch").is_err());
        assert!(parse_skin_key("ely:../x").is_err());
    }
}
