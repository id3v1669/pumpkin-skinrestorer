//! Decisions: which skin a joining player gets, who is admin, cooldowns, the
//! one-pending-operation guard and stale-result protection.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::source::{SkinData, Source, valid_name};
use crate::store::{Cache, Choice, PlayerRecord};

/// Cooldown after a successful / failed `/skin set`, `/skin update` or `/skull`.
pub const COOLDOWN_SUCCESS_MS: u64 = 30_000;
pub const COOLDOWN_ERROR_MS: u64 = 5_000;

/// What to put on a joining player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Apply {
    Steve,
    /// Leave the skin the profile already has.
    Keep,
    Skin(SkinData),
}

/// A lookup to run in the background: `name` in `sources`, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub name: String,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginPlan {
    pub apply: Apply,
    pub lookup: Option<Wanted>,
    /// The stored choice became blocked: reset it to automatic.
    pub drop_choice: bool,
}

/// The login steps from the design, using stored data only.
pub fn plan_login(
    record: Option<&PlayerRecord>,
    own_name: &str,
    has_own_skin: bool,
    cache: &Cache,
    cfg: &Config,
    now_secs: u64,
) -> LoginPlan {
    let mut drop_choice = false;
    if let Some(r) = record {
        if r.locked {
            return LoginPlan {
                apply: Apply::Steve,
                lookup: None,
                drop_choice: false,
            };
        }
        if let Choice::Skin { source, name } = &r.choice {
            if cfg.is_blocked(*source, name) {
                drop_choice = true;
            } else {
                let exact = Wanted {
                    name: name.clone(),
                    sources: vec![*source],
                };
                return match cache.get(*source, name) {
                    Some(c) => LoginPlan {
                        apply: Apply::Skin(c.skin()),
                        lookup: c.is_expired(now_secs, cfg.cache_minutes).then_some(exact),
                        drop_choice: false,
                    },
                    None => LoginPlan {
                        apply: Apply::Steve,
                        lookup: Some(exact),
                        drop_choice: false,
                    },
                };
            }
        }
    }

    if has_own_skin {
        return LoginPlan {
            apply: Apply::Keep,
            lookup: None,
            drop_choice,
        };
    }
    let sources: Vec<Source> = cfg
        .source_order
        .iter()
        .copied()
        .filter(|s| !cfg.is_blocked(*s, own_name))
        .collect();
    if !valid_name(own_name) || sources.is_empty() {
        return LoginPlan {
            apply: Apply::Steve,
            lookup: None,
            drop_choice,
        };
    }
    for &source in &sources {
        if let Some(c) = cache.get(source, own_name) {
            return LoginPlan {
                apply: Apply::Skin(c.skin()),
                lookup: c.is_expired(now_secs, cfg.cache_minutes).then(|| Wanted {
                    name: own_name.to_ascii_lowercase(),
                    sources: vec![source],
                }),
                drop_choice,
            };
        }
    }
    LoginPlan {
        apply: Apply::Steve,
        lookup: Some(Wanted {
            name: own_name.to_ascii_lowercase(),
            sources,
        }),
        drop_choice,
    }
}

/// Operator level at least `admin_op_level`, or a player UUID listed in `admins`.
pub fn is_admin(op_level: u8, player_uuid: Option<&str>, cfg: &Config) -> bool {
    op_level >= cfg.admin_op_level || player_uuid.is_some_and(|u| cfg.is_admin_uuid(u))
}

/// In-memory cooldowns keyed by the issuing player's key.
#[derive(Default)]
pub struct Cooldowns {
    until: HashMap<String, u64>,
}

impl Cooldowns {
    /// `Err(seconds left)` while cooling down.
    pub fn check(&self, key: &str, now_ms: u64) -> Result<(), u64> {
        match self.until.get(key) {
            Some(&t) if now_ms < t => Err((t - now_ms).div_ceil(1000)),
            _ => Ok(()),
        }
    }

    pub fn start(&mut self, key: &str, success: bool, now_ms: u64) {
        let length = if success {
            COOLDOWN_SUCCESS_MS
        } else {
            COOLDOWN_ERROR_MS
        };
        self.until.insert(key.to_string(), now_ms + length);
    }
}

/// Per-player revision counters (memory only) and the one-pending-operation guard.
#[derive(Default)]
pub struct Ops {
    revisions: HashMap<String, u64>,
    pending: HashSet<String>,
}

impl Ops {
    pub fn revision(&self, key: &str) -> u64 {
        self.revisions.get(key).copied().unwrap_or(0)
    }

    /// Call on every change to a player's choice or lock.
    pub fn bump(&mut self, key: &str) {
        *self.revisions.entry(key.to_string()).or_insert(0) += 1;
    }

    /// Marks a network operation as pending; false if one already is.
    pub fn begin(&mut self, key: &str) -> bool {
        self.pending.insert(key.to_string())
    }

    pub fn end(&mut self, key: &str) {
        self.pending.remove(key);
    }

    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.contains(key)
    }
}

/// Why a finished lookup may not become the player's choice.
#[derive(Debug, PartialEq, Eq)]
pub enum Superseded {
    Changed,
    Locked,
    Blocked,
    NotAdmin,
}

/// Rechecks the rules before committing a lookup result as a choice.
pub fn may_commit(
    started_revision: u64,
    current_revision: u64,
    locked: bool,
    blocked: bool,
    issuer_still_admin: Option<bool>,
) -> Result<(), Superseded> {
    if current_revision != started_revision {
        return Err(Superseded::Changed);
    }
    if locked {
        return Err(Superseded::Locked);
    }
    if blocked {
        return Err(Superseded::Blocked);
    }
    if issuer_still_admin == Some(false) {
        return Err(Superseded::NotAdmin);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use crate::store::CachedSkin;

    fn cached(at: u64) -> CachedSkin {
        CachedSkin {
            value: "v".into(),
            signature: "s".into(),
            uuid: "u".into(),
            fetched_at: at,
        }
    }

    fn skin() -> Apply {
        Apply::Skin(SkinData {
            value: "v".into(),
            signature: "s".into(),
        })
    }

    fn record(choice: Choice) -> PlayerRecord {
        let mut r = PlayerRecord::new("Alice", "u");
        r.choice = choice;
        r
    }

    #[test]
    fn locked_player_gets_steve() {
        let mut r = record(Choice::skin(Source::Mojang, "notch"));
        r.locked = true;
        let p = plan_login(
            Some(&r),
            "Alice",
            true,
            &Cache::default(),
            &Config::default(),
            0,
        );
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(p.lookup, None);
    }

    #[test]
    fn cached_choice_is_applied_and_refreshed_when_expired() {
        let r = record(Choice::skin(Source::Mojang, "notch"));
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "notch", cached(0));
        let cfg = Config::default();
        let fresh = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert_eq!((fresh.apply, fresh.lookup), (skin(), None));
        let stale = plan_login(Some(&r), "Alice", false, &cache, &cfg, 15 * 60);
        assert_eq!(stale.apply, skin());
        assert_eq!(
            stale.lookup,
            Some(Wanted {
                name: "notch".into(),
                sources: vec![Source::Mojang]
            })
        );
    }

    #[test]
    fn uncached_choice_gets_steve_and_a_lookup() {
        let r = record(Choice::skin(Source::Ely, "notch"));
        let p = plan_login(
            Some(&r),
            "Alice",
            true,
            &Cache::default(),
            &Config::default(),
            0,
        );
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(p.lookup.unwrap().sources, vec![Source::Ely]);
    }

    #[test]
    fn blocked_choice_is_dropped_and_falls_back_to_auto() {
        let r = record(Choice::skin(Source::Mojang, "notch"));
        let cfg = parse(r#"blocked = ["mojang:notch"]"#).unwrap();
        let p = plan_login(Some(&r), "Alice", false, &Cache::default(), &cfg, 0);
        assert!(p.drop_choice);
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(p.lookup.unwrap().name, "alice");
    }

    #[test]
    fn own_profile_skin_is_kept_in_auto() {
        let p = plan_login(
            None,
            "Alice",
            true,
            &Cache::default(),
            &Config::default(),
            0,
        );
        assert_eq!(p.apply, Apply::Keep);
        assert_eq!(p.lookup, None);
    }

    #[test]
    fn auto_uses_the_first_cached_source() {
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "alice", cached(0));
        let p = plan_login(None, "Alice", false, &cache, &Config::default(), 60);
        assert_eq!((p.apply, p.lookup), (skin(), None));
    }

    #[test]
    fn auto_without_cache_looks_up_all_unblocked_sources() {
        let cfg = parse(r#"blocked = ["ely:alice"]"#).unwrap();
        let p = plan_login(None, "Alice", false, &Cache::default(), &cfg, 0);
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(p.lookup.unwrap().sources, vec![Source::Mojang]);
    }

    #[test]
    fn invalid_or_fully_blocked_own_name_is_steve() {
        let p = plan_login(
            None,
            "bad name",
            false,
            &Cache::default(),
            &Config::default(),
            0,
        );
        assert_eq!((p.apply, p.lookup), (Apply::Steve, None));
        let cfg = parse(r#"blocked = ["ely:alice", "mojang:alice"]"#).unwrap();
        let p = plan_login(None, "Alice", false, &Cache::default(), &cfg, 0);
        assert_eq!((p.apply, p.lookup), (Apply::Steve, None));
    }

    #[test]
    fn admin_by_level_or_uuid() {
        let cfg = parse(r#"admins = ["069a79f4-44e9-4726-a5be-fca90e38aaf5"]"#).unwrap();
        assert!(is_admin(4, None, &cfg));
        assert!(!is_admin(3, Some("other"), &cfg));
        assert!(is_admin(
            0,
            Some("069A79F4-44E9-4726-A5BE-FCA90E38AAF5"),
            &cfg
        ));
    }

    #[test]
    fn cooldowns_depend_on_success() {
        let mut c = Cooldowns::default();
        assert_eq!(c.check("a", 0), Ok(()));
        c.start("a", true, 0);
        assert_eq!(c.check("a", 1), Err(30));
        assert_eq!(c.check("a", 30_000), Ok(()));
        c.start("a", false, 0);
        assert_eq!(c.check("a", 4_001), Err(1));
    }

    #[test]
    fn one_pending_operation_per_player() {
        let mut ops = Ops::default();
        assert!(ops.begin("a"));
        assert!(!ops.begin("a"));
        assert!(ops.is_pending("a"));
        ops.end("a");
        assert!(!ops.is_pending("a"));
        assert!(ops.begin("a"));
    }

    #[test]
    fn stale_results_are_refused() {
        assert_eq!(may_commit(1, 1, false, false, None), Ok(()));
        assert_eq!(
            may_commit(1, 2, false, false, None),
            Err(Superseded::Changed)
        );
        assert_eq!(may_commit(1, 1, true, false, None), Err(Superseded::Locked));
        assert_eq!(
            may_commit(1, 1, false, true, None),
            Err(Superseded::Blocked)
        );
        assert_eq!(
            may_commit(1, 1, false, false, Some(false)),
            Err(Superseded::NotAdmin)
        );
        let mut ops = Ops::default();
        let start = ops.revision("a");
        ops.bump("a");
        assert_ne!(ops.revision("a"), start);
    }
}
