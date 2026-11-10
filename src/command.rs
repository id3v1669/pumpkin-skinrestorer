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
        Some((s, n)) => (
            Some(Source::parse(s).ok_or_else(|| format!("unknown source `{s}`"))?),
            n,
        ),
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

pub fn parse_skin(args: &str) -> Result<SkinCmd, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
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
pub fn parse_skull(args: &str) -> Result<(SkinSpec, Option<String>), String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [spec] => Ok((parse_spec(spec)?, None)),
        [spec, target] => Ok((parse_spec(spec)?, Some(player(target)?))),
        _ => Err(format!("usage: {SKULL_USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
