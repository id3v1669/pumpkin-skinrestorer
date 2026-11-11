//! Decisions: which skin a joining player gets, who is admin, cooldowns, the
//! one-pending-operation guard and stale-result protection.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::skin::skin_url;
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
    /// The stored choice became blocked, or its source recently answered that it has
    /// no such skin: reset it to automatic; the caller must also bump the player's
    /// revision (`Ops::bump`).
    pub drop_choice: bool,
}

/// What a joining player gets, decided from stored data only:
///
/// 1. Locked: Steve.
/// 2. An explicit choice: its cached skin, refreshed from its own source alone once
///    expired. A choice that is blocked, or that its source answered "not found" for
///    less than `cache_minutes` ago, is dropped and steps 3-4 apply. Any other choice
///    without a cached skin leaves the player with the automatic result of steps 3-4
///    while the choice is looked up.
/// 3. The skin the profile already has is kept.
/// 4. Automatic: the configured source order wins. The first cached, unblocked source
///    of the player's own name is applied; if that entry is expired every unblocked
///    source is looked up again, and if it is fresh only the sources ranked above it
///    are. Without any cached skin: Steve, and every unblocked source is looked up.
///
/// A source that answered "not found" for a name less than `cache_minutes` ago is not
/// looked up again.
pub fn plan_login(
    record: Option<&PlayerRecord>,
    own_name: &str,
    has_own_skin: bool,
    cache: &Cache,
    cfg: &Config,
    now_secs: u64,
) -> LoginPlan {
    let mut drop_choice = false;
    // The lookup for an explicit choice that has no cached skin.
    let mut chosen: Option<Wanted> = None;
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
                match cache.get(*source, name) {
                    Some(c) => {
                        return LoginPlan {
                            apply: Apply::Skin(c.skin()),
                            lookup: c.is_expired(now_secs, cfg.cache_minutes).then_some(exact),
                            drop_choice: false,
                        };
                    }
                    // The skin is gone: the player is automatic again.
                    None if cache.is_missed(*source, name, now_secs, cfg.cache_minutes) => {
                        drop_choice = true;
                    }
                    None => chosen = Some(exact),
                }
            }
        }
    }

    let (apply, automatic_lookup) = automatic(own_name, has_own_skin, cache, cfg, now_secs);
    LoginPlan {
        apply,
        // Only the choice is looked up while it is pending.
        lookup: chosen.or(automatic_lookup),
        drop_choice,
    }
}

/// Steps 3-4 of [`plan_login`]: what a player without a usable explicit choice gets, and
/// the lookup for their own name.
fn automatic(
    own_name: &str,
    has_own_skin: bool,
    cache: &Cache,
    cfg: &Config,
    now_secs: u64,
) -> (Apply, Option<Wanted>) {
    if has_own_skin {
        return (Apply::Keep, None);
    }
    if !valid_name(own_name) {
        return (Apply::Steve, None);
    }
    let sources: Vec<Source> = cfg
        .source_order
        .iter()
        .copied()
        .filter(|s| !cfg.is_blocked(*s, own_name))
        .collect();
    // A lookup in those of `list` that did not answer "not found" recently.
    let wanted = |list: &[Source]| {
        let sources: Vec<Source> = list
            .iter()
            .copied()
            .filter(|s| !cache.is_missed(*s, own_name, now_secs, cfg.cache_minutes))
            .collect();
        (!sources.is_empty()).then(|| Wanted {
            name: own_name.to_ascii_lowercase(),
            sources,
        })
    };
    for (i, &source) in sources.iter().enumerate() {
        if let Some(c) = cache.get(source, own_name) {
            // The source order wins: an expired hit rechecks every source, a
            // fresh one only the sources ranked above it.
            let recheck = if c.is_expired(now_secs, cfg.cache_minutes) {
                &sources[..]
            } else {
                &sources[..i]
            };
            return (Apply::Skin(c.skin()), wanted(recheck));
        }
    }
    (Apply::Steve, wanted(&sources))
}

/// Whether a player is told that a skin found by a login lookup is ready. `plan` is
/// their login plan as it stands now, after the lookup updated the cache: the skin it
/// applies must be the found one (`found_url`), and not the one already applied at
/// their login (`applied_url`). Skins are compared by URL: the textures value carries a
/// fresh timestamp.
pub fn notice_due(plan: &LoginPlan, found_url: &str, applied_url: Option<&str>) -> bool {
    match &plan.apply {
        Apply::Skin(s) => {
            skin_url(&s.value).as_deref() == Some(found_url) && applied_url != Some(found_url)
        }
        Apply::Steve | Apply::Keep => false,
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

    fn cached(value: &str, at: u64) -> CachedSkin {
        CachedSkin {
            value: value.into(),
            signature: "s".into(),
            uuid: "u".into(),
            fetched_at: at,
        }
    }

    fn skin(value: &str) -> Apply {
        Apply::Skin(SkinData {
            value: value.into(),
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
        let cfg = parse(r#"blocked = ["mojang:notch"]"#).unwrap();
        let p = plan_login(Some(&r), "Alice", true, &Cache::default(), &cfg, 0);
        assert_eq!((p.apply, p.lookup), (Apply::Steve, None));
        assert!(!p.drop_choice);
    }

    #[test]
    fn cached_choice_is_applied_and_refreshed_when_expired() {
        let r = record(Choice::skin(Source::Mojang, "notch"));
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "notch", cached("v", 0));
        let cfg = Config::default();
        let fresh = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert_eq!((fresh.apply, fresh.lookup), (skin("v"), None));
        let stale = plan_login(Some(&r), "Alice", false, &cache, &cfg, 15 * 60);
        assert_eq!(stale.apply, skin("v"));
        assert_eq!(
            stale.lookup,
            Some(Wanted {
                name: "notch".into(),
                sources: vec![Source::Mojang]
            })
        );
    }

    #[test]
    fn uncached_choice_gets_the_automatic_skin_and_its_own_lookup() {
        let r = record(Choice::skin(Source::Ely, "notch"));
        let cfg = Config::default();
        let exact = Some(Wanted {
            name: "notch".into(),
            sources: vec![Source::Ely],
        });
        // Nothing stored at all: Steve.
        let p = plan_login(Some(&r), "Alice", false, &Cache::default(), &cfg, 0);
        assert_eq!(
            (p.apply, p.lookup, p.drop_choice),
            (Apply::Steve, exact.clone(), false)
        );
        // The profile's own skin is kept meanwhile.
        let p = plan_login(Some(&r), "Alice", true, &Cache::default(), &cfg, 0);
        assert_eq!((p.apply, p.lookup), (Apply::Keep, exact.clone()));
        // Or the cached skin of the player's own name; only the choice is looked up.
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "alice", cached("own", 0));
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert_eq!((p.apply, p.lookup), (skin("own"), exact));
    }

    #[test]
    fn a_recently_missed_choice_is_dropped_and_the_own_name_looked_up() {
        // Someone else's lookup found out that the chosen skin is gone.
        let r = record(Choice::skin(Source::Ely, "notch"));
        let cfg = Config::default();
        let mut cache = Cache::default();
        cache.put_miss(Source::Ely, "notch", 0);
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert!(p.drop_choice);
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Ely, Source::Mojang]
            })
        );
        // With a profile skin there is nothing to look up.
        let p = plan_login(Some(&r), "Alice", true, &cache, &cfg, 60);
        assert_eq!(
            (p.apply, p.lookup, p.drop_choice),
            (Apply::Keep, None, true)
        );
        // An expired miss says nothing: the choice is kept and looked up.
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 15 * 60);
        assert!(!p.drop_choice);
        assert_eq!(p.lookup.unwrap().name, "notch");
    }

    #[test]
    fn auto_skips_sources_that_recently_had_nothing() {
        let cfg = Config::default();
        let both = Some(Wanted {
            name: "alice".into(),
            sources: vec![Source::Ely, Source::Mojang],
        });
        let mut cache = Cache::default();
        cache.put_miss(Source::Ely, "alice", 0);
        let p = plan_login(None, "Alice", false, &cache, &cfg, 60);
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Mojang]
            })
        );
        // Nothing anywhere: no lookup until a miss expires.
        cache.put_miss(Source::Mojang, "alice", 0);
        let p = plan_login(None, "Alice", false, &cache, &cfg, 60);
        assert_eq!((p.apply, p.lookup), (Apply::Steve, None));
        let p = plan_login(None, "Alice", false, &cache, &cfg, 15 * 60);
        assert_eq!((p.apply, p.lookup), (Apply::Steve, both));
    }

    #[test]
    fn a_lower_priority_hit_does_not_recheck_a_missed_source() {
        let cfg = Config::default();
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "alice", cached("mojang", 0));
        cache.put_miss(Source::Ely, "alice", 0);
        let p = plan_login(None, "Alice", false, &cache, &cfg, 60);
        assert_eq!((p.apply, p.lookup), (skin("mojang"), None));
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
    fn blocked_choice_with_own_skin_keeps_it() {
        let r = record(Choice::skin(Source::Mojang, "notch"));
        let cfg = parse(r#"blocked = ["mojang:notch"]"#).unwrap();
        let p = plan_login(Some(&r), "Alice", true, &Cache::default(), &cfg, 0);
        assert!(p.drop_choice);
        assert_eq!((p.apply, p.lookup), (Apply::Keep, None));
    }

    #[test]
    fn blocked_choice_falls_back_to_the_cached_own_name() {
        let r = record(Choice::skin(Source::Mojang, "notch"));
        let cfg = parse(r#"blocked = ["mojang:notch"]"#).unwrap();
        let mut cache = Cache::default();
        cache.put(Source::Ely, "alice", cached("own", 0));
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert!(p.drop_choice);
        assert_eq!((p.apply, p.lookup), (skin("own"), None));
    }

    #[test]
    fn record_with_automatic_choice_is_planned_like_no_record() {
        let r = record(Choice::Auto);
        let cfg = Config::default();
        let kept = plan_login(Some(&r), "Alice", true, &Cache::default(), &cfg, 0);
        assert_eq!((kept.apply, kept.lookup), (Apply::Keep, None));
        assert!(!kept.drop_choice);
        let p = plan_login(Some(&r), "Alice", false, &Cache::default(), &cfg, 0);
        assert_eq!(
            p,
            plan_login(None, "Alice", false, &Cache::default(), &cfg, 0)
        );
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Ely, Source::Mojang]
            })
        );
        assert!(!p.drop_choice);
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
    fn auto_prefers_the_first_source_when_both_are_cached() {
        let mut cache = Cache::default();
        cache.put(Source::Ely, "alice", cached("ely", 0));
        cache.put(Source::Mojang, "alice", cached("mojang", 0));
        let p = plan_login(None, "Alice", false, &cache, &Config::default(), 60);
        assert_eq!((p.apply, p.lookup), (skin("ely"), None));
    }

    #[test]
    fn auto_with_a_lower_priority_hit_looks_up_the_sources_before_it() {
        let mut cache = Cache::default();
        cache.put(Source::Mojang, "alice", cached("mojang", 0));
        let p = plan_login(None, "Alice", false, &cache, &Config::default(), 60);
        assert_eq!(p.apply, skin("mojang"));
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Ely]
            })
        );
    }

    #[test]
    fn auto_with_an_expired_hit_looks_up_all_sources() {
        let mut cache = Cache::default();
        cache.put(Source::Ely, "alice", cached("ely", 0));
        let p = plan_login(None, "Alice", false, &cache, &Config::default(), 15 * 60);
        assert_eq!(p.apply, skin("ely"));
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Ely, Source::Mojang]
            })
        );
    }

    #[test]
    fn auto_ignores_a_cached_skin_from_a_blocked_source() {
        let cfg = parse(r#"blocked = ["ely:alice"]"#).unwrap();
        let mut cache = Cache::default();
        cache.put(Source::Ely, "alice", cached("ely", 0));
        let p = plan_login(None, "Alice", false, &cache, &cfg, 60);
        assert_eq!(p.apply, Apply::Steve);
        assert_eq!(
            p.lookup,
            Some(Wanted {
                name: "alice".into(),
                sources: vec![Source::Mojang]
            })
        );
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
        c.start("a", true, 0);
        assert_eq!(c.check("a", 29_999), Err(1));
        c.start("b", false, 0);
        assert_eq!(c.check("b", 0), Err(5));
        assert_eq!(c.check("b", 4_999), Err(1));
        assert_eq!(c.check("b", 5_000), Ok(()));
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
        assert_eq!(may_commit(1, 1, false, false, Some(true)), Ok(()));
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

    /// A base64 `textures` value whose skin URL is `url`.
    fn textures(url: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(format!(r#"{{"textures":{{"SKIN":{{"url":"{url}"}}}}}}"#))
    }

    fn plan(apply: Apply) -> LoginPlan {
        LoginPlan {
            apply,
            lookup: None,
            drop_choice: false,
        }
    }

    #[test]
    fn no_notice_when_the_plan_applies_a_different_skin() {
        let p = plan(skin(&textures("http://t/other")));
        assert!(!notice_due(&p, "http://t/found", None));
        assert!(!notice_due(&p, "http://t/found", Some("http://t/old")));
    }

    #[test]
    fn no_notice_when_the_plan_is_steve_or_keep() {
        for apply in [Apply::Steve, Apply::Keep] {
            assert!(!notice_due(&plan(apply), "http://t/found", None));
        }
    }

    #[test]
    fn notice_when_the_plan_applies_the_found_skin_and_it_is_new() {
        let p = plan(skin(&textures("http://t/found")));
        assert!(notice_due(&p, "http://t/found", None));
        assert!(notice_due(&p, "http://t/found", Some("http://t/old")));
    }

    #[test]
    fn no_notice_when_the_found_skin_is_already_worn() {
        let p = plan(skin(&textures("http://t/found")));
        assert!(!notice_due(&p, "http://t/found", Some("http://t/found")));
    }

    #[test]
    fn no_notice_when_the_planned_skin_has_no_url() {
        assert!(!notice_due(
            &plan(skin("not base64")),
            "http://t/found",
            None
        ));
    }

    #[test]
    fn ely_result_is_not_what_a_mojang_only_order_applies() {
        // A lookup found the Ely skin and cached it, then a reload dropped Ely.
        let cfg = parse(r#"source_order = ["mojang"]"#).unwrap();
        let mut cache = Cache::default();
        cache.put(Source::Ely, "alice", cached(&textures("http://t/ely"), 0));
        let r = record(Choice::Auto);
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert_eq!(p.apply, Apply::Steve);
        assert!(!notice_due(&p, "http://t/ely", None));

        // With Ely in the order the same result is the planned skin.
        let p = plan_login(Some(&r), "Alice", false, &cache, &Config::default(), 60);
        assert!(notice_due(&p, "http://t/ely", None));
        // A profile skin outranks it.
        let p = plan_login(Some(&r), "Alice", true, &cache, &Config::default(), 60);
        assert!(!notice_due(&p, "http://t/ely", None));
        // So does a cached skin from a higher-ranked source.
        let cfg = parse(r#"source_order = ["mojang", "ely"]"#).unwrap();
        cache.put(
            Source::Mojang,
            "alice",
            cached(&textures("http://t/mojang"), 0),
        );
        let p = plan_login(Some(&r), "Alice", false, &cache, &cfg, 60);
        assert!(!notice_due(&p, "http://t/ely", None));
    }
}
