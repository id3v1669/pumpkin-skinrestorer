//! Parsing of `/skin` and `/skull` arguments (everything after the command name).

use crate::config::parse_skin_key;
use crate::source::{Source, valid_name};

/// A skin to look up: an explicit source, or the configured order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkinSpec {
    pub source: Option<Source>,
    /// As typed; validated.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkinCmd {
    Help,
    Set {
        spec: SkinSpec,
        target: Option<String>,
    },
    Clear {
        target: Option<String>,
    },
    Update {
        target: Option<String>,
    },
    Status {
        target: Option<String>,
    },
    History,
    Undo,
    Lock(String),
    Unlock(String),
    Block(Source, String),
    Unblock(Source, String),
    Reload,
}

impl SkinCmd {
    /// Commands only admins may run.
    pub fn needs_admin(&self) -> bool {
        match self {
            SkinCmd::Set { target, .. }
            | SkinCmd::Clear { target }
            | SkinCmd::Update { target }
            | SkinCmd::Status { target } => target.is_some(),
            SkinCmd::Lock(_)
            | SkinCmd::Unlock(_)
            | SkinCmd::Block(..)
            | SkinCmd::Unblock(..)
            | SkinCmd::Reload => true,
            SkinCmd::Help | SkinCmd::History | SkinCmd::Undo => false,
        }
    }
}

pub const SKIN_USAGE: &str = "/skin set [ely:|mojang:]<name> [player] | clear [player] | \
update [player] | status [player] | history | undo | lock <player> | unlock <player> | \
block <source:name> | unblock <source:name> | reload";

pub const SKULL_USAGE: &str = "/skull [ely:|mojang:]<name> [player]";

/// `ely:Notch`, `mojang:Notch` or `Notch`.
pub fn parse_spec(arg: &str) -> Result<SkinSpec, String> {
    let (source, name) = match arg.split_once(':') {
        Some(("", _)) => return Err("missing source before `:`".to_string()),
        Some((s, n)) => {
            let source = Source::parse(s).ok_or_else(|| format!("unknown source `{s}`"))?;
            if n.is_empty() {
                return Err(format!("missing account name after `{s}:`"));
            }
            (Some(source), n)
        }
        None => (None, arg),
    };
    if !valid_name(name) {
        return Err(format!("`{name}` is not a valid account name"));
    }
    Ok(SkinSpec {
        source,
        name: name.to_string(),
    })
}

fn player(arg: &str) -> Result<String, String> {
    if valid_name(arg) {
        Ok(arg.to_string())
    } else {
        Err(format!("`{arg}` is not a valid player name"))
    }
}

fn optional_player(arg: Option<&str>) -> Result<Option<String>, String> {
    arg.map(player).transpose()
}

/// `/skin <subcommand> ...`; the subcommand is matched case-insensitively.
pub fn parse_skin(args: &str) -> Result<SkinCmd, String> {
    let subcommand = args
        .split_whitespace()
        .next()
        .map(|word| word.to_ascii_lowercase());
    let mut words: Vec<&str> = args.split_whitespace().collect();
    if let Some(subcommand) = &subcommand {
        words[0] = subcommand;
    }
    let usage = || format!("usage: {SKIN_USAGE}");
    let cmd = match words.as_slice() {
        [] | ["help"] => SkinCmd::Help,
        ["set", spec] => SkinCmd::Set {
            spec: parse_spec(spec)?,
            target: None,
        },
        ["set", spec, target] => SkinCmd::Set {
            spec: parse_spec(spec)?,
            target: Some(player(target)?),
        },
        ["clear", rest @ ..] if rest.len() <= 1 => SkinCmd::Clear {
            target: optional_player(rest.first().copied())?,
        },
        ["update", rest @ ..] if rest.len() <= 1 => SkinCmd::Update {
            target: optional_player(rest.first().copied())?,
        },
        ["status", rest @ ..] if rest.len() <= 1 => SkinCmd::Status {
            target: optional_player(rest.first().copied())?,
        },
        ["history"] => SkinCmd::History,
        ["undo"] => SkinCmd::Undo,
        ["lock", target] => SkinCmd::Lock(player(target)?),
        ["unlock", target] => SkinCmd::Unlock(player(target)?),
        ["block", key] => {
            let (s, n) = parse_skin_key(key)?;
            SkinCmd::Block(s, n)
        }
        ["unblock", key] => {
            let (s, n) = parse_skin_key(key)?;
            SkinCmd::Unblock(s, n)
        }
        ["reload"] => SkinCmd::Reload,
        _ => return Err(usage()),
    };
    Ok(cmd)
}

/// `/skull <spec> [player]`.
///
/// The caller must require admin when a target player is given.
pub fn parse_skull(args: &str) -> Result<(SkinSpec, Option<String>), String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [spec] => Ok((parse_spec(spec)?, None)),
        [spec, target] => Ok((parse_spec(spec)?, Some(player(target)?))),
        _ => Err(format!("usage: {SKULL_USAGE}")),
    }
}

const OPEN_SUBCOMMANDS: [&str; 7] = [
    "set", "clear", "update", "status", "history", "undo", "help",
];
const ADMIN_SUBCOMMANDS: [&str; 5] = ["lock", "unlock", "block", "unblock", "reload"];
const SOURCE_PREFIXES: [&str; 2] = ["ely:", "mojang:"];

/// Splits the typed arguments into the finished words (with their whitespace) and the
/// word being typed, which is empty after a space.
fn split_last_word(input: &str) -> (&str, &str) {
    let cut = input.trim_end_matches(|c: char| !c.is_whitespace()).len();
    input.split_at(cut)
}

/// The candidates that continue the word being typed, each as the whole argument text:
/// the server replaces everything typed after the command name with a suggestion.
fn completions<S: AsRef<str>>(input: &str, candidates: impl IntoIterator<Item = S>) -> Vec<String> {
    let (head, typed) = split_last_word(input);
    let typed = typed.to_ascii_lowercase();
    candidates
        .into_iter()
        .filter(|c| {
            let c = c.as_ref().to_ascii_lowercase();
            c.starts_with(&typed) && c != typed
        })
        .map(|c| format!("{head}{}", c.as_ref()))
        .collect()
}

/// Tab completions for the arguments of `/skin` typed so far: subcommands, source
/// prefixes and, for admins, the names of online players.
pub fn suggest_skin(input: &str, players: &[String], admin: bool) -> Vec<String> {
    let (head, _) = split_last_word(input);
    let words: Vec<String> = head
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => {
            let admin_only = if admin { &ADMIN_SUBCOMMANDS[..] } else { &[] };
            completions(input, OPEN_SUBCOMMANDS.iter().chain(admin_only))
        }
        ["set"] => completions(input, SOURCE_PREFIXES),
        ["block" | "unblock"] if admin => completions(input, SOURCE_PREFIXES),
        ["set", _] | ["clear" | "update" | "status" | "lock" | "unlock"] if admin => {
            completions(input, players)
        }
        _ => Vec::new(),
    }
}

/// Tab completions for the arguments of `/skull` typed so far.
pub fn suggest_skull(input: &str, players: &[String], admin: bool) -> Vec<String> {
    let (head, _) = split_last_word(input);
    match head.split_whitespace().count() {
        0 => completions(input, SOURCE_PREFIXES),
        1 if admin => completions(input, players),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online() -> Vec<String> {
        vec!["Alice".to_string(), "alex".to_string(), "Bob".to_string()]
    }

    #[test]
    fn subcommands_are_suggested_by_prefix() {
        let all = suggest_skin("", &online(), false);
        assert_eq!(all, OPEN_SUBCOMMANDS.map(String::from).to_vec());
        assert_eq!(suggest_skin("s", &online(), false), ["set", "status"]);
        assert_eq!(suggest_skin("U", &online(), false), ["update", "undo"]);
        // Admin subcommands only for admins.
        assert!(suggest_skin("lo", &online(), false).is_empty());
        assert_eq!(suggest_skin("lo", &online(), true), ["lock"]);
        assert_eq!(
            suggest_skin("un", &online(), true),
            ["undo", "unlock", "unblock"]
        );
        // A finished word is not suggested again.
        assert!(suggest_skin("history", &online(), false).is_empty());
        assert!(suggest_skin("dance", &online(), true).is_empty());
    }

    #[test]
    fn suggestions_are_the_whole_argument_text() {
        assert_eq!(
            suggest_skin("set ", &online(), false),
            ["set ely:", "set mojang:"]
        );
        assert_eq!(suggest_skin("SET  m", &online(), false), ["SET  mojang:"]);
        assert_eq!(
            suggest_skin("set Notch a", &online(), true),
            ["set Notch Alice", "set Notch alex"]
        );
        assert_eq!(suggest_skin("lock b", &online(), true), ["lock Bob"]);
        assert_eq!(
            suggest_skin("block ", &online(), true),
            ["block ely:", "block mojang:"]
        );
    }

    #[test]
    fn players_are_suggested_to_admins_only() {
        for input in [
            "set Notch ",
            "clear ",
            "update ",
            "status ",
            "lock ",
            "unlock ",
        ] {
            assert!(suggest_skin(input, &online(), false).is_empty(), "{input}");
            assert_eq!(suggest_skin(input, &online(), true).len(), 3, "{input}");
        }
        assert!(suggest_skin("block ", &online(), false).is_empty());
        // Nothing after the last argument.
        assert!(suggest_skin("set Notch Bob ", &online(), true).is_empty());
        assert!(suggest_skin("history ", &online(), true).is_empty());
        assert!(suggest_skin("reload ", &online(), true).is_empty());
    }

    #[test]
    fn skull_suggestions() {
        assert_eq!(suggest_skull("", &online(), false), ["ely:", "mojang:"]);
        assert_eq!(suggest_skull("e", &online(), false), ["ely:"]);
        assert!(suggest_skull("ely:No", &online(), false).is_empty());
        assert!(suggest_skull("Notch ", &online(), false).is_empty());
        assert_eq!(suggest_skull("Notch b", &online(), true), ["Notch Bob"]);
        assert!(suggest_skull("Notch Bob ", &online(), true).is_empty());
    }

    #[test]
    fn specs_parse_with_and_without_prefix() {
        assert_eq!(
            parse_spec("ely:Notch").unwrap(),
            SkinSpec {
                source: Some(Source::Ely),
                name: "Notch".into()
            }
        );
        assert_eq!(parse_spec("Notch").unwrap().source, None);
        assert!(parse_spec("cdn:Notch").is_err());
        assert!(parse_spec("ely:").is_err());
        assert!(parse_spec("bad/name").is_err());
    }

    #[test]
    fn skin_subcommands_parse() {
        assert_eq!(parse_skin("").unwrap(), SkinCmd::Help);
        assert_eq!(
            parse_skin("set mojang:Notch Bob").unwrap(),
            SkinCmd::Set {
                spec: SkinSpec {
                    source: Some(Source::Mojang),
                    name: "Notch".into()
                },
                target: Some("Bob".into())
            }
        );
        assert_eq!(
            parse_skin("clear").unwrap(),
            SkinCmd::Clear { target: None }
        );
        assert_eq!(
            parse_skin("status Bob").unwrap(),
            SkinCmd::Status {
                target: Some("Bob".into())
            }
        );
        assert_eq!(parse_skin("undo").unwrap(), SkinCmd::Undo);
        assert_eq!(
            parse_skin("block ely:Notch").unwrap(),
            SkinCmd::Block(Source::Ely, "notch".into())
        );
        assert_eq!(parse_skin("reload").unwrap(), SkinCmd::Reload);
        assert!(parse_skin("set").is_err());
        assert!(parse_skin("clear a b").is_err());
        assert!(parse_skin("block Notch").is_err());
        assert!(parse_skin("dance").is_err());
    }

    #[test]
    fn admin_commands_are_marked() {
        assert!(!parse_skin("set Notch").unwrap().needs_admin());
        assert!(parse_skin("set Notch Bob").unwrap().needs_admin());
        assert!(parse_skin("lock Bob").unwrap().needs_admin());
        assert!(parse_skin("reload").unwrap().needs_admin());
        assert!(!parse_skin("history").unwrap().needs_admin());
    }

    #[test]
    fn skull_arguments_parse() {
        let (spec, target) = parse_skull("ely:Notch").unwrap();
        assert_eq!((spec.source, target), (Some(Source::Ely), None));
        assert_eq!(parse_skull("Notch Bob").unwrap().1, Some("Bob".into()));
        assert!(parse_skull("").is_err());
    }

    #[test]
    fn needs_admin_covers_every_form() {
        let admin = [
            "set Notch Bob",
            "clear Bob",
            "update Bob",
            "status Bob",
            "lock Bob",
            "unlock Bob",
            "block ely:Notch",
            "unblock mojang:Notch",
            "reload",
        ];
        let open = [
            "set Notch",
            "clear",
            "update",
            "status",
            "history",
            "undo",
            "help",
            "",
        ];
        for args in admin {
            assert!(parse_skin(args).unwrap().needs_admin(), "{args}");
        }
        for args in open {
            assert!(!parse_skin(args).unwrap().needs_admin(), "{args}");
        }
    }

    #[test]
    fn subcommands_ignore_case() {
        assert_eq!(
            parse_skin("Set Notch").unwrap(),
            SkinCmd::Set {
                spec: SkinSpec {
                    source: None,
                    name: "Notch".into()
                },
                target: None
            }
        );
        assert_eq!(parse_skin("RELOAD").unwrap(), SkinCmd::Reload);
        assert_eq!(parse_skin("Help").unwrap(), SkinCmd::Help);
    }

    #[test]
    fn empty_spec_parts_are_named() {
        assert_eq!(
            parse_spec("ely:").unwrap_err(),
            "missing account name after `ely:`"
        );
        assert_eq!(
            parse_spec(":Notch").unwrap_err(),
            "missing source before `:`"
        );
    }

    #[test]
    fn remaining_subcommands_parse() {
        assert_eq!(
            parse_skin("update").unwrap(),
            SkinCmd::Update { target: None }
        );
        assert_eq!(
            parse_skin("update Bob").unwrap(),
            SkinCmd::Update {
                target: Some("Bob".into())
            }
        );
        assert_eq!(parse_skin("history").unwrap(), SkinCmd::History);
        assert_eq!(
            parse_skin("unlock Bob").unwrap(),
            SkinCmd::Unlock("Bob".into())
        );
        assert_eq!(
            parse_skin("unblock mojang:Notch").unwrap(),
            SkinCmd::Unblock(Source::Mojang, "notch".into())
        );
        assert_eq!(parse_skin("help").unwrap(), SkinCmd::Help);
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(
            parse_skin("  set   Notch  ").unwrap(),
            SkinCmd::Set {
                spec: SkinSpec {
                    source: None,
                    name: "Notch".into()
                },
                target: None
            }
        );
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(parse_skin("set Notch bad/name").is_err());
        assert!(parse_skin("lock bad/name").is_err());
        let long = "a".repeat(17);
        assert!(parse_spec(&long).is_err());
        assert!(parse_skin(&format!("set {long}")).is_err());
        assert!(parse_skin(&format!("set Notch {long}")).is_err());
        assert!(parse_skin(&format!("lock {long}")).is_err());
        assert!(parse_spec(&"a".repeat(16)).is_ok());
    }

    #[test]
    fn skull_rejects_extra_arguments_and_bad_specs() {
        assert!(parse_skull("Notch Bob extra").is_err());
        assert!(parse_skull("cdn:Notch").is_err());
        assert!(parse_skull("bad/name").is_err());
    }

    #[test]
    fn spec_keeps_case_and_block_lowercases() {
        assert_eq!(parse_spec("Notch").unwrap().name, "Notch");
        assert_eq!(parse_spec("ely:Notch").unwrap().name, "Notch");
        assert_eq!(
            parse_skin("block ely:Notch").unwrap(),
            SkinCmd::Block(Source::Ely, "notch".into())
        );
    }
}
