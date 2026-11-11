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

/// Values of keys the file leaves out; [`DEFAULT_CONFIG`] spells out the same ones.
const DEFAULT_SOURCE_ORDER: [Source; 2] = [Source::Ely, Source::Mojang];
const DEFAULT_CACHE_MINUTES: i64 = 15;
const DEFAULT_ADMIN_OP_LEVEL: i64 = 4;
const DEFAULT_HISTORY_LENGTH: i64 = 36;

/// Every key `config.toml` may contain.
const KEYS: [&str; 6] = [
    "source_order",
    "cache_minutes",
    "admin_op_level",
    "admins",
    "history_length",
    "blocked",
];

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

    if let Some((key, _)) = doc.iter().find(|(key, _)| !KEYS.contains(key)) {
        return Err(format!(
            "unknown key `{key}` (known keys: {})",
            KEYS.join(", ")
        ));
    }

    let mut source_order = Vec::new();
    for s in strings(&doc, "source_order")? {
        let source =
            Source::parse(&s).ok_or_else(|| format!("unknown source `{s}` in `source_order`"))?;
        if source_order.contains(&source) {
            return Err(format!("source `{s}` is listed twice"));
        }
        source_order.push(source);
    }
    if doc.get("source_order").is_none() {
        source_order = DEFAULT_SOURCE_ORDER.to_vec();
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
        let (source, name) = parse_skin_key(&b).map_err(|e| format!("`blocked`: {e}"))?;
        blocked.push(skin_key(source, &name));
    }

    Ok(Config {
        source_order,
        cache_minutes: integer(&doc, "cache_minutes", DEFAULT_CACHE_MINUTES, (1, 525_600))? as u64,
        admin_op_level: integer(&doc, "admin_op_level", DEFAULT_ADMIN_OP_LEVEL, (0, 4))? as u8,
        admins,
        history_length: integer(&doc, "history_length", DEFAULT_HISTORY_LENGTH, (0, 1000))?
            as usize,
        blocked,
    })
}

const NOT_A_LIST: &str = "`blocked` must be a list of strings";

// toml_edit keeps the text between two array values (comments, newlines, indentation)
// as the prefix of the *following* value, and the text after the last comma as the
// array's `trailing()`. A plain `push`/`retain` therefore puts a new entry on the last
// entry's line and deletes the comment lines above a removed entry, so the edits below
// move that text around by hand.

fn prefix(value: &Value) -> String {
    let raw = value.decor().prefix().and_then(|p| p.as_str());
    raw.unwrap_or_default().to_string()
}

fn suffix(value: &Value) -> String {
    let raw = value.decor().suffix().and_then(|s| s.as_str());
    raw.unwrap_or_default().to_string()
}

fn trailing(array: &Array) -> String {
    array.trailing().as_str().unwrap_or_default().to_string()
}

/// Without a trailing comma the text up to `]` hangs off the last value as its suffix.
/// If it spans lines, moves it into `trailing()` (adding the comma) so that both layouts
/// are edited the same way.
fn normalise_tail(array: &mut Array) {
    let Some(last) = array.len().checked_sub(1).and_then(|i| array.get_mut(i)) else {
        return;
    };
    let tail = suffix(last);
    if !tail.contains('\n') {
        return;
    }
    last.decor_mut().set_suffix("");
    let tail = tail + &trailing(array);
    array.set_trailing(tail);
    array.set_trailing_comma(true);
}

fn matches(value: &Value, entry: &str) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(entry))
}

fn position(array: &Array, entry: &str) -> Option<usize> {
    array.iter().position(|v| matches(v, entry))
}

/// Adds `entry` (already normalised `source:name`) to `blocked`, keeping comments and
/// layout. Returns the new file text, or `None` if it was already blocked.
pub fn add_blocked(text: &str, entry: &str) -> Result<Option<String>, String> {
    debug_assert!(parse_skin_key(entry).is_ok());
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    if !doc.contains_key("blocked") {
        doc["blocked"] = Item::Value(Value::Array(Array::new()));
    }
    let array = doc["blocked"].as_array_mut().ok_or(NOT_A_LIST)?;
    if position(array, entry).is_some() {
        return Ok(None);
    }

    normalise_tail(array);
    let tail = trailing(array);
    match tail.rfind('\n') {
        // `]` is on the last entry's line: a single-line list.
        None => array.push(entry),
        // `]` is on its own line: the entry gets its own line too, indented like the
        // nearest entry that starts a line.
        Some(newline) => {
            let (before, closing) = tail.split_at(newline + 1);
            let indent = array
                .iter()
                .map(prefix)
                .filter_map(|p| p.rfind('\n').map(|i| p[i + 1..].to_string()))
                .last()
                .unwrap_or_else(|| "    ".to_string());
            let mut value = Value::from(entry);
            value.decor_mut().set_prefix(format!("{before}{indent}"));
            value.decor_mut().set_suffix("");
            array.push_formatted(value);
            array.set_trailing(format!("\n{closing}"));
            array.set_trailing_comma(true);
        }
    }
    Ok(Some(doc.to_string()))
}

/// Removes the value at `index`, keeping the comment lines around it. Only the rest of
/// the removed entry's own line (its trailing comment) goes with it.
fn remove_at(array: &mut Array, index: usize) {
    let last = index + 1 == array.len();
    let before = prefix(&array.remove(index));
    // The text after the removed entry: the next value's prefix, or the array's tail.
    let after = match array.get(index) {
        Some(next) => prefix(next),
        None => trailing(array),
    };
    let joined = match (before.rfind('\n'), after.find('\n')) {
        // The entry had its own line: drop that line, keep the lines above and below.
        (Some(b), Some(a)) => format!("{}{}", &before[..=b], &after[a + 1..]),
        // The next entry was on the same line and takes this one's place.
        (Some(_), None) => before,
        // The entry ended a line shared with earlier entries, which keep its rest.
        (None, Some(_)) => after,
        (None, None) if last => after,
        (None, None) => before,
    };
    match array.get_mut(index) {
        Some(next) => next.decor_mut().set_prefix(joined),
        None => array.set_trailing(joined),
    }
}

/// Removes `entry` from `blocked`. Returns the new text, or `None` if it was not there.
pub fn remove_blocked(text: &str, entry: &str) -> Result<Option<String>, String> {
    debug_assert!(parse_skin_key(entry).is_ok());
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    let Some(item) = doc.get_mut("blocked") else {
        return Ok(None);
    };
    let array = item.as_array_mut().ok_or(NOT_A_LIST)?;
    if position(array, entry).is_none() {
        return Ok(None);
    }
    normalise_tail(array);
    while let Some(index) = position(array, entry) {
        remove_at(array, index);
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

    fn err(text: &str) -> String {
        parse(text).unwrap_err()
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(err(r#"source_order = ["minecraft"]"#).contains("in `source_order`"));
        assert!(err(r#"source_order = ["ely", "ely"]"#).contains("listed twice"));
        assert!(err("source_order = []").contains("at least one source"));
        assert!(err("admin_op_level = 5").contains("`admin_op_level` must be between 0 and 4"));
        assert!(err("cache_minutes = 0").contains("`cache_minutes` must be between 1 and 525600"));
        assert!(err(r#"admins = ["not-a-uuid"]"#).contains("`admins`"));
        assert!(err(r#"blocked = ["Notch"]"#).starts_with("`blocked`: "));
        assert!(err(r#"blocked = ["nope:Notch"]"#).starts_with("`blocked`: "));
        assert!(err("cache_minutes = \"x\"").contains("`cache_minutes` must be an integer"));
        assert!(err("admins = [1]").contains("`admins` must be a list of strings"));
        assert!(err("blocked = \"x\"").contains("`blocked` must be a list of strings"));
        assert!(parse("this is not toml").is_err());
    }

    #[test]
    fn range_edges() {
        assert!(parse("cache_minutes = 525600").is_ok());
        assert!(err("cache_minutes = 525601").contains("`cache_minutes` must be between"));
        assert!(err("history_length = 1001").contains("`history_length` must be between"));
        assert_eq!(parse("history_length = 0").unwrap().history_length, 0);
        assert!(err("admin_op_level = -1").contains("`admin_op_level` must be between"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = err("cache_minuts = 5");
        assert!(e.contains("`cache_minuts`"), "{e}");
        for key in [
            "source_order",
            "cache_minutes",
            "admin_op_level",
            "admins",
            "history_length",
            "blocked",
        ] {
            assert!(e.contains(key), "{e}");
        }
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

    const MULTI: &str = r#"blocked = [
  # griefers
  "mojang:a", # why a
  # b is next
  "ely:b", # why b
]
"#;

    /// The edit result, which must still be a valid config.
    fn edited(result: Result<Option<String>, String>) -> String {
        let text = result.unwrap().unwrap();
        parse(&text).unwrap();
        text
    }

    #[test]
    fn multi_line_add_goes_on_its_own_line() {
        assert_eq!(
            edited(add_blocked(MULTI, "ely:new")),
            r#"blocked = [
  # griefers
  "mojang:a", # why a
  # b is next
  "ely:b", # why b
  "ely:new",
]
"#
        );
    }

    #[test]
    fn multi_line_remove_keeps_other_comments() {
        assert_eq!(
            edited(remove_blocked(MULTI, "mojang:a")),
            r#"blocked = [
  # griefers
  # b is next
  "ely:b", # why b
]
"#
        );
        assert_eq!(
            edited(remove_blocked(MULTI, "ely:b")),
            r#"blocked = [
  # griefers
  "mojang:a", # why a
  # b is next
]
"#
        );
        let only = "blocked = [\n  # griefers\n  \"mojang:a\", # why a\n]\n";
        assert_eq!(
            edited(remove_blocked(only, "mojang:a")),
            "blocked = [\n  # griefers\n]\n"
        );
    }

    #[test]
    fn multi_line_without_trailing_comma() {
        let text = "blocked = [\n  \"mojang:a\",\n  \"ely:b\" # why b\n]\n";
        assert_eq!(
            edited(add_blocked(text, "ely:new")),
            "blocked = [\n  \"mojang:a\",\n  \"ely:b\", # why b\n  \"ely:new\",\n]\n"
        );
        assert_eq!(
            edited(remove_blocked(text, "ely:b")),
            "blocked = [\n  \"mojang:a\",\n]\n"
        );
        assert_eq!(
            edited(remove_blocked(text, "mojang:a")),
            "blocked = [\n  \"ely:b\", # why b\n]\n"
        );
    }

    #[test]
    fn multi_line_empty_array_add() {
        assert_eq!(
            edited(add_blocked("blocked = [\n  # none yet\n]\n", "ely:new")),
            "blocked = [\n  # none yet\n    \"ely:new\",\n]\n"
        );
    }

    #[test]
    fn single_line_edits() {
        let text = "blocked = [] # inline\n";
        let added = edited(add_blocked(text, "mojang:notch"));
        assert_eq!(added, "blocked = [\"mojang:notch\"] # inline\n");
        assert_eq!(edited(remove_blocked(&added, "mojang:notch")), text);

        let two = edited(add_blocked("blocked = [\"mojang:a\"]\n", "ely:b"));
        assert_eq!(two, "blocked = [\"mojang:a\", \"ely:b\"]\n");
        assert_eq!(
            edited(remove_blocked(&two, "mojang:a")),
            "blocked = [\"ely:b\"]\n"
        );
        assert_eq!(
            edited(remove_blocked(&two, "ely:b")),
            "blocked = [\"mojang:a\"]\n"
        );
    }

    #[test]
    fn remove_ignores_case_and_takes_duplicates() {
        let text = "blocked = [\"Mojang:A\", \"ely:b\", \"mojang:a\"]\n";
        assert_eq!(
            edited(remove_blocked(text, "MOJANG:a")),
            "blocked = [\"ely:b\"]\n"
        );
    }

    #[test]
    fn edits_reject_a_non_list() {
        let text = "blocked = \"x\"\n";
        let add = add_blocked(text, "ely:b").unwrap_err();
        assert_eq!(remove_blocked(text, "ely:b").unwrap_err(), add);
        assert_eq!(remove_blocked("cache_minutes = 5\n", "ely:b"), Ok(None));
    }
}
