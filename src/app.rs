//! The plugin's behaviour, independent of the Pumpkin bindings: the state, the login
//! decision, `/skin` and `/skull`, what happens when a lookup finishes, and when the
//! state files are written.
//!
//! The server is reached only through [`Host`] and [`Files`], so everything here is
//! tested natively. `plugin.rs` implements both, sends the HTTP requests the
//! [`Engine`] asks for and carries out the returned [`Effect`]s.
//!
//! Two clocks are passed in: `now_ms` is monotonic and drives the engine, cooldowns
//! and write intervals; `now_secs` is Unix time and only dates cache entries.

use std::collections::HashMap;

use crate::command::{SKIN_USAGE, SkinCmd, SkinSpec, parse_skin, parse_skull};
use crate::config::{self, Config, DEFAULT_CONFIG, skin_key};
use crate::lookup::{Engine, Outcome, QueryId};
use crate::resolve::{
    Apply, Cooldowns, LoginPlan, Ops, Superseded, is_admin, may_commit, notice_due, plan_login,
};
use crate::skin::skin_url;
use crate::source::{SkinData, Source, valid_name};
use crate::store::{
    self, Cache, CacheSeq, CachedSkin, Choice, PlayerRecord, Players, keep_secs, player_key,
    to_json, to_json_compact,
};

pub const CONFIG_PATH: &str = "data/config.toml";
pub const PLAYERS_PATH: &str = "data/players.json";
pub const SKINS_PATH: &str = "data/skins.json";

/// `skins.json` is written at most this often.
pub const CACHE_SAVE_INTERVAL_MS: u64 = 5_000;
/// Old cache entries are dropped this often (and at load).
pub const EVICT_INTERVAL_MS: u64 = 60 * 60 * 1000;
/// Failed login lookups are logged at most this often.
pub const LOOKUP_WARNING_INTERVAL_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// An online player as the server reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    pub name: String,
    /// Hyphenated UUID.
    pub uuid: String,
    /// Operator level, 0-4.
    pub op_level: u8,
}

/// What the plugin asks of the server.
pub trait Host {
    /// An online player by name, ignoring ASCII case.
    fn player_by_name(&self, name: &str) -> Option<PlayerInfo>;
    fn player_by_uuid(&self, uuid: &str) -> Option<PlayerInfo>;
    fn log(&self, level: LogLevel, msg: &str);
}

/// The plugin's data folder.
pub trait Files {
    /// `Ok(None)` if the file does not exist.
    fn read(&self, path: &str) -> Result<Option<String>, String>;
    /// Replaces the file as a whole.
    fn write(&self, path: &str, contents: &str) -> Result<(), String>;
}

/// The server, the data folder and the two clocks for one call.
pub struct Env<'a> {
    pub host: &'a dyn Host,
    pub files: &'a dyn Files,
    /// Monotonic milliseconds.
    pub now_ms: u64,
    /// Unix seconds.
    pub now_secs: u64,
}

/// Who runs a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sender {
    Player(PlayerInfo),
    Console,
    /// Anything else (RCON, a command block). `reply` identifies the sender for a reply
    /// that comes after the command returned; see [`Issuer::Other`].
    Other {
        op_level: u8,
        reply: u64,
    },
}

/// Who ran a command, for replies that arrive after the command returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issuer {
    Console,
    Player {
        uuid: String,
        key: String,
    },
    /// A [`Sender::Other`]; `admin` is what they were when the command ran, since they
    /// cannot be asked again.
    Other {
        reply: u64,
        admin: bool,
    },
}

/// Things for the caller to do once a call into [`State`] has returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Tell(Issuer, String),
    /// A message for the player with this UUID, if still online.
    TellPlayer(String, String),
    GiveSkull {
        issuer: Issuer,
        /// UUID of the player who gets the head.
        recipient: String,
        recipient_name: String,
        /// Account name as typed; it names the head.
        name: String,
        skin: SkinData,
    },
}

/// A head to give right away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// UUID of the player who gets it.
    pub recipient: String,
    pub name: String,
    pub skin: SkinData,
}

/// The outcome of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The answer, or the error to show.
    pub text: Result<String, String>,
    /// `/skull` from the cache: give this head before sending `text`.
    pub give: Option<Head>,
    /// A lookup was started; its result reaches the issuer later as an [`Effect`].
    pub deferred: bool,
}

impl Reply {
    fn now(text: Result<String, String>) -> Reply {
        Reply {
            text,
            give: None,
            deferred: false,
        }
    }
}

/// What a running lookup is for.
enum Purpose {
    Set {
        issuer: Issuer,
        own: bool,
        target_key: String,
        revision: u64,
    },
    Update {
        issuer: Issuer,
        target_key: String,
    },
    Skull {
        issuer: Issuer,
        recipient: String,
        recipient_name: String,
        recipient_key: String,
        name: String,
        /// A target player was given: the issuer must still be an admin at delivery.
        admin_required: bool,
    },
    /// Queued at login; `dedupe` is its entry in `State::login_lookups`.
    Login {
        dedupe: String,
        /// Every player waiting for this lookup.
        waiters: Vec<LoginWaiter>,
    },
}

struct LoginWaiter {
    uuid: String,
    /// Key of the player's record.
    key: String,
    /// Skin URL applied at their login, if any: a player is only told about a skin
    /// they are not already wearing.
    applied: Option<String>,
}

/// The sender of a command, resolved.
struct Who {
    issuer: Issuer,
    player: Option<PlayerInfo>,
    admin: bool,
}

pub struct State {
    cfg: Config,
    online_mode: bool,
    players: Players,
    cache: Cache,
    /// Which answer last wrote each cache key; see [`CacheSeq`].
    cache_seq: CacheSeq,
    /// The caller sends the requests it asks for and feeds the responses back.
    pub engine: Engine,
    /// Running lookups: the name looked up and what it is for.
    purposes: HashMap<QueryId, (String, Purpose)>,
    ops: Ops,
    cooldowns: Cooldowns,
    /// Running login lookups by dedupe key.
    login_lookups: HashMap<String, QueryId>,
    /// Whether each player's profile had a skin of its own at their last login.
    own_skin: HashMap<String, bool>,
    /// `players.json` needs writing at the next [`State::flush`].
    players_dirty: bool,
    /// `skins.json` needs writing; see [`CACHE_SAVE_INTERVAL_MS`].
    cache_dirty: bool,
    last_cache_save_ms: Option<u64>,
    last_evict_ms: u64,
    /// When a failed login lookup was last logged; see [`LOOKUP_WARNING_INTERVAL_MS`].
    last_lookup_warning_ms: Option<u64>,
    /// Failed login lookups not logged since then.
    suppressed_lookup_warnings: u32,
}

/// The text of `config.toml`. A missing file is recreated from the defaults, which the
/// flag reports.
fn config_text(files: &dyn Files) -> Result<(String, bool), String> {
    match files.read(CONFIG_PATH)? {
        Some(t) => Ok((t, false)),
        None => {
            files.write(CONFIG_PATH, DEFAULT_CONFIG)?;
            Ok((DEFAULT_CONFIG.to_string(), true))
        }
    }
}

fn recreated_note(recreated: bool, reply: String) -> String {
    if recreated {
        format!("config.toml was missing; recreated with defaults. {reply}")
    } else {
        reply
    }
}

fn superseded_reason(s: &Superseded) -> &'static str {
    match s {
        Superseded::Changed => "the skin was changed meanwhile",
        Superseded::Locked => "the player is locked",
        Superseded::Blocked => "that skin is blocked",
        Superseded::NotAdmin => "you are no longer an admin",
    }
}

impl State {
    /// Reads the config and the state files. An invalid config or `players.json` is an
    /// error, so that neither is overwritten; an unreadable `skins.json` is only a
    /// cache and is replaced by an empty one.
    pub fn load(env: &Env, online_mode: bool) -> Result<State, String> {
        let (config_text, _) = config_text(env.files)?;
        let cfg = config::parse(&config_text)?;
        let players: Players = store::load(env.files.read(PLAYERS_PATH)?.as_deref())
            .map_err(|e| format!("{PLAYERS_PATH}: {e}"))?;
        let read_cache = env
            .files
            .read(SKINS_PATH)
            .and_then(|text| store::load(text.as_deref()));
        let mut cache: Cache = match read_cache {
            Ok(cache) => cache,
            Err(e) => {
                env.host.log(
                    LogLevel::Error,
                    &format!("{SKINS_PATH} is unreadable ({e}); starting with an empty skin cache"),
                );
                Cache::default()
            }
        };
        let cache_dirty = cache.evict(env.now_secs, keep_secs(cfg.cache_minutes));
        Ok(State {
            cfg,
            online_mode,
            players,
            cache,
            cache_seq: CacheSeq::default(),
            engine: Engine::new(),
            purposes: HashMap::new(),
            ops: Ops::default(),
            cooldowns: Cooldowns::default(),
            login_lookups: HashMap::new(),
            own_skin: HashMap::new(),
            players_dirty: false,
            cache_dirty,
            last_cache_save_ms: None,
            last_evict_ms: env.now_ms,
            last_lookup_warning_ms: None,
            suppressed_lookup_warnings: 0,
        })
    }

    /// Writes what changed: `players.json` at once, `skins.json` at most every
    /// [`CACHE_SAVE_INTERVAL_MS`] unless `force`d (at unload). Also drops old cache
    /// entries once per [`EVICT_INTERVAL_MS`]. A failed write is logged and not retried
    /// until the state changes again.
    pub fn flush(&mut self, env: &Env, force: bool) {
        if env.now_ms.saturating_sub(self.last_evict_ms) >= EVICT_INTERVAL_MS {
            self.last_evict_ms = env.now_ms;
            if self
                .cache
                .evict(env.now_secs, keep_secs(self.cfg.cache_minutes))
            {
                self.cache_dirty = true;
            }
        }
        let save = |path: &str, contents: String| {
            if let Err(e) = env.files.write(path, &contents) {
                env.host
                    .log(LogLevel::Error, &format!("could not save {e}"));
            }
        };
        if std::mem::take(&mut self.players_dirty) {
            save(PLAYERS_PATH, to_json(&self.players));
        }
        let due = self
            .last_cache_save_ms
            .is_none_or(|t| env.now_ms.saturating_sub(t) >= CACHE_SAVE_INTERVAL_MS);
        if self.cache_dirty && (force || due) {
            self.cache_dirty = false;
            self.last_cache_save_ms = Some(env.now_ms);
            save(SKINS_PATH, to_json_compact(&self.cache));
        }
    }

    /// The key of a player's record, which is created if missing and has its name and
    /// UUID kept current.
    fn record(&mut self, name: &str, uuid: &str) -> String {
        let key = player_key(self.online_mode, name, uuid);
        let mut changed = false;
        let record = self.players.players.entry(key.clone()).or_insert_with(|| {
            changed = true;
            PlayerRecord::new(name, uuid)
        });
        if record.name != name || record.uuid != uuid {
            record.name = name.to_string();
            record.uuid = uuid.to_string();
            changed = true;
        }
        if changed {
            self.players_dirty = true;
        }
        key
    }

    /// The login decision for a stored player as it stands now.
    fn plan_for(&self, key: &str, now_secs: u64) -> Option<LoginPlan> {
        let record = self.players.players.get(key)?;
        let has_own_skin = self.own_skin.get(key).copied().unwrap_or(false);
        Some(plan_login(
            Some(record),
            &record.name,
            has_own_skin,
            &self.cache,
            &self.cfg,
            now_secs,
        ))
    }

    /// Sources to try for a name, without those where it is blocked.
    fn sources_for(&self, spec_source: Option<Source>, name: &str) -> Vec<Source> {
        let order = match spec_source {
            Some(s) => vec![s],
            None => self.cfg.source_order.clone(),
        };
        order
            .into_iter()
            .filter(|s| !self.cfg.is_blocked(*s, name))
            .collect()
    }

    /// Unexpired cached skin of the first of `sources`. Lower-ranked sources are not
    /// consulted: the source order wins, so they need a lookup.
    fn fresh_cached(
        &self,
        sources: &[Source],
        name: &str,
        now_secs: u64,
    ) -> Option<(Source, SkinData)> {
        let &first = sources.first()?;
        self.cache
            .get(first, name)
            .filter(|c| !c.is_expired(now_secs, self.cfg.cache_minutes))
            .map(|c| (first, c.skin()))
    }

    fn issuer_is_admin(&self, host: &dyn Host, issuer: &Issuer) -> bool {
        match issuer {
            Issuer::Console => true,
            Issuer::Player { uuid, .. } => host
                .player_by_uuid(uuid)
                .is_some_and(|p| is_admin(p.op_level, Some(&p.uuid), &self.cfg)),
            Issuer::Other { admin, .. } => *admin,
        }
    }

    /// Whether the sender may use the admin commands (for tab completion).
    pub fn is_admin_sender(&self, sender: &Sender) -> bool {
        match sender {
            Sender::Player(p) => is_admin(p.op_level, Some(&p.uuid), &self.cfg),
            Sender::Console => true,
            Sender::Other { op_level, .. } => is_admin(*op_level, None, &self.cfg),
        }
    }

    // --- login -----------------------------------------------------------------------

    /// What to put on a joining player. Uses stored data only; a lookup for what is
    /// missing or expired is queued.
    pub fn login(
        &mut self,
        name: &str,
        uuid: &str,
        has_own_skin: bool,
        now_ms: u64,
        now_secs: u64,
    ) -> Apply {
        let key = self.record(name, uuid);
        self.own_skin.insert(key.clone(), has_own_skin);
        let plan = plan_login(
            self.players.players.get(&key),
            name,
            has_own_skin,
            &self.cache,
            &self.cfg,
            now_secs,
        );
        if plan.drop_choice {
            if let Some(r) = self.players.players.get_mut(&key) {
                r.choice = Choice::Auto;
            }
            self.ops.bump(&key);
            self.players_dirty = true;
        }
        if let Some(wanted) = plan.lookup {
            let dedupe = format!("{}|{:?}", wanted.name, wanted.sources);
            let applied = match &plan.apply {
                Apply::Skin(skin) => skin_url(&skin.value),
                Apply::Steve | Apply::Keep => None,
            };
            let waiter = LoginWaiter {
                uuid: uuid.to_string(),
                key: key.clone(),
                applied,
            };
            let running = self.login_lookups.get(&dedupe).copied();
            if let Some((_, Purpose::Login { waiters, .. })) =
                running.and_then(|q| self.purposes.get_mut(&q))
            {
                // Same lookup already running: wait for it too. A rejoining player
                // replaces their earlier entry.
                waiters.retain(|w| w.key != key);
                waiters.push(waiter);
            } else if let Ok(q) = self.engine.submit(&wanted.name, &wanted.sources, now_ms) {
                self.login_lookups.insert(dedupe.clone(), q);
                self.purposes.insert(
                    q,
                    (
                        wanted.name.clone(),
                        Purpose::Login {
                            dedupe,
                            waiters: vec![waiter],
                        },
                    ),
                );
            }
            // Busy: the stored skin stays in use; the next login tries again.
        }
        plan.apply
    }

    // --- finished lookups ------------------------------------------------------------

    /// Acts on every lookup the engine finished since the last call.
    pub fn finish_all(&mut self, env: &Env) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (q, outcome) in self.engine.take_finished() {
            if let Some((name, purpose)) = self.purposes.remove(&q) {
                effects.extend(self.finish(env, &name, purpose, outcome));
            }
        }
        effects
    }

    fn start_cooldown(&mut self, issuer: &Issuer, success: bool, now_ms: u64) {
        if let Issuer::Player { key, .. } = issuer {
            self.cooldowns.start(key, success, now_ms);
        }
    }

    /// Commits a found skin as the target's choice, after rechecking the rules.
    fn commit_choice(
        &mut self,
        host: &dyn Host,
        issuer: &Issuer,
        own: bool,
        target_key: &str,
        revision: u64,
        choice: Choice,
    ) -> Result<(), Superseded> {
        let blocked = match &choice {
            Choice::Skin { source, name } => self.cfg.is_blocked(*source, name),
            Choice::Auto => false,
        };
        let still_admin = (!own).then(|| self.issuer_is_admin(host, issuer));
        let current = self.ops.revision(target_key);
        let cap = self.cfg.history_length;
        let record = self
            .players
            .players
            .get_mut(target_key)
            .ok_or(Superseded::Changed)?;
        may_commit(revision, current, record.locked, blocked, still_admin)?;
        if own {
            record.set_own(choice, cap);
        } else {
            record.choice = choice;
        }
        self.ops.bump(target_key);
        self.players_dirty = true;
        Ok(())
    }

    /// If the player's explicit choice is the looked-up name and its source just
    /// answered that it has no such skin, the choice goes back to automatic (history
    /// untouched, as for a blocked choice). Returns the dropped choice.
    ///
    /// The revision is not bumped: a `/skin set` the player has running should still
    /// win over this.
    fn reset_vanished(
        &mut self,
        key: &str,
        lookup_name: &str,
        outcome: &Outcome,
    ) -> Option<Choice> {
        let record = self.players.players.get_mut(key)?;
        let Choice::Skin { source, name } = &record.choice else {
            return None;
        };
        let vanished = name.eq_ignore_ascii_case(lookup_name)
            && outcome.not_found_in().iter().any(|(s, _)| s == source)
            // A newer answer may have found it again.
            && self.cache.get(*source, name).is_none();
        if !vanished {
            return None;
        }
        self.players_dirty = true;
        Some(std::mem::replace(&mut record.choice, Choice::Auto))
    }

    /// Logs a failed login lookup, at most once per [`LOOKUP_WARNING_INTERVAL_MS`]; the
    /// next warning counts the ones left out.
    fn warn_failed_lookup(&mut self, host: &dyn Host, name: &str, error: &str, now_ms: u64) {
        let recent = self
            .last_lookup_warning_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < LOOKUP_WARNING_INTERVAL_MS);
        if recent {
            self.suppressed_lookup_warnings += 1;
            return;
        }
        let mut message = format!("skin lookup for {name} failed: {error}");
        let suppressed = std::mem::take(&mut self.suppressed_lookup_warnings);
        if suppressed > 0 {
            message.push_str(&format!(" ({suppressed} similar suppressed)"));
        }
        self.last_lookup_warning_ms = Some(now_ms);
        host.log(LogLevel::Warn, &message);
    }

    /// Acts on a finished lookup of `lookup_name`.
    fn finish(
        &mut self,
        env: &Env,
        lookup_name: &str,
        purpose: Purpose,
        outcome: Outcome,
    ) -> Vec<Effect> {
        let now = env.now_ms;
        // Lookups finish out of order (one may wait on a second source while another
        // for the same name completes), so an answer is only written to the cache if it
        // is newer than what the key holds.
        for &(source, seq) in outcome.not_found_in() {
            if self.cache_seq.is_newer(source, lookup_name, seq) {
                self.cache_seq.record(source, lookup_name, seq);
                self.cache.put_miss(source, lookup_name, env.now_secs);
                self.cache_dirty = true;
            }
        }
        let found = match &outcome {
            Outcome::Found {
                source,
                name,
                uuid,
                skin,
                answered_seq,
                ..
            } => {
                if self.cache_seq.is_newer(*source, name, *answered_seq) {
                    self.cache_seq.record(*source, name, *answered_seq);
                    self.cache.put(
                        *source,
                        name,
                        CachedSkin {
                            value: skin.value.clone(),
                            signature: skin.signature.clone(),
                            uuid: uuid.clone(),
                            fetched_at: env.now_secs,
                        },
                    );
                    self.cache_dirty = true;
                }
                Some((*source, name.clone(), skin.clone()))
            }
            _ => None,
        };
        let failure = match &outcome {
            Outcome::Found { .. } => String::new(),
            Outcome::NotFound { .. } => "no skin found".to_string(),
            Outcome::Failed { error, .. } => format!("skin lookup failed: {error}"),
        };
        let mut effects = Vec::new();
        match purpose {
            Purpose::Set {
                issuer,
                own,
                target_key,
                revision,
            } => {
                self.ops.end(&target_key);
                let message = match found {
                    Some((source, name, _)) => {
                        let choice = Choice::skin(source, &name);
                        let label = choice.to_string();
                        match self.commit_choice(
                            env.host,
                            &issuer,
                            own,
                            &target_key,
                            revision,
                            choice,
                        ) {
                            Ok(()) => {
                                self.start_cooldown(&issuer, true, now);
                                format!("Skin set to {label}. It applies on the next join.")
                            }
                            Err(why) => {
                                self.start_cooldown(&issuer, false, now);
                                format!("Skin change superseded: {}.", superseded_reason(&why))
                            }
                        }
                    }
                    None => {
                        self.start_cooldown(&issuer, false, now);
                        format!("Skin not changed: {failure}.")
                    }
                };
                effects.push(Effect::Tell(issuer, message));
            }
            Purpose::Update { issuer, target_key } => {
                self.ops.end(&target_key);
                self.start_cooldown(&issuer, found.is_some(), now);
                let message = match found {
                    Some((source, name, skin)) => {
                        // Only promised if it is what the next login applies: a
                        // profile skin or a higher-ranked source may win.
                        let url = skin_url(&skin.value);
                        let applies = self.plan_for(&target_key, env.now_secs).is_some_and(
                            |plan| matches!(&plan.apply, Apply::Skin(s) if skin_url(&s.value) == url),
                        );
                        let key = skin_key(source, &name);
                        if applies {
                            format!("Updated {key}. It applies on the next join.")
                        } else {
                            format!("Updated {key}.")
                        }
                    }
                    None => match self.reset_vanished(&target_key, lookup_name, &outcome) {
                        Some(gone) => format!(
                            "Skin not updated: {failure}. {gone} no longer exists; back to automatic."
                        ),
                        None => format!("Skin not updated: {failure}."),
                    },
                };
                effects.push(Effect::Tell(issuer, message));
            }
            Purpose::Skull {
                issuer,
                recipient,
                recipient_name,
                recipient_key,
                name,
                admin_required,
            } => {
                self.ops.end(&recipient_key);
                // The rules may have changed while the lookup ran.
                let refused = match &found {
                    Some((source, found_name, _)) if self.cfg.is_blocked(*source, found_name) => {
                        Some(Superseded::Blocked)
                    }
                    Some(_) if admin_required && !self.issuer_is_admin(env.host, &issuer) => {
                        Some(Superseded::NotAdmin)
                    }
                    _ => None,
                };
                self.start_cooldown(&issuer, found.is_some() && refused.is_none(), now);
                match (found, refused) {
                    (Some(_), Some(why)) => effects.push(Effect::Tell(
                        issuer,
                        format!("Head not given: {}.", superseded_reason(&why)),
                    )),
                    (Some((_, _, skin)), None) => effects.push(Effect::GiveSkull {
                        issuer,
                        recipient,
                        recipient_name,
                        name,
                        skin,
                    }),
                    (None, _) => {
                        effects.push(Effect::Tell(issuer, format!("No head: {failure}.")));
                    }
                }
            }
            Purpose::Login { dedupe, waiters } => {
                self.login_lookups.remove(&dedupe);
                if let Outcome::Failed { error, .. } = &outcome {
                    self.warn_failed_lookup(env.host, lookup_name, error, now);
                }
                for waiter in &waiters {
                    self.reset_vanished(&waiter.key, lookup_name, &outcome);
                }
                // Compared by URL: the textures value carries a fresh timestamp.
                if let Some(url) = found
                    .as_ref()
                    .and_then(|(_, _, skin)| skin_url(&skin.value))
                {
                    for waiter in waiters {
                        // Only players who get this skin at their next join are told: the
                        // login decision is made again with the rules, record and cache as
                        // they are now.
                        let due = self
                            .plan_for(&waiter.key, env.now_secs)
                            .is_some_and(|plan| notice_due(&plan, &url, waiter.applied.as_deref()));
                        if due {
                            effects.push(Effect::TellPlayer(
                                waiter.uuid,
                                "Your skin is ready, rejoin to see it.".into(),
                            ));
                        }
                    }
                }
            }
        }
        effects
    }

    // --- commands --------------------------------------------------------------------

    fn sender_of(&mut self, sender: &Sender) -> Who {
        let admin = self.is_admin_sender(sender);
        match sender {
            Sender::Player(p) => {
                let key = self.record(&p.name, &p.uuid);
                Who {
                    issuer: Issuer::Player {
                        uuid: p.uuid.clone(),
                        key,
                    },
                    player: Some(p.clone()),
                    admin,
                }
            }
            Sender::Console => Who {
                issuer: Issuer::Console,
                player: None,
                admin,
            },
            Sender::Other { reply, .. } => Who {
                issuer: Issuer::Other {
                    reply: *reply,
                    admin,
                },
                player: None,
                admin,
            },
        }
    }

    /// The command's player: the target if given (must be online), else the sender.
    fn resolve_target(
        host: &dyn Host,
        who: &Who,
        target: Option<&str>,
    ) -> Result<PlayerInfo, String> {
        match (target, &who.player) {
            (Some(name), _) => host
                .player_by_name(name)
                .ok_or_else(|| format!("{name} is not online.")),
            (None, Some(p)) => Ok(p.clone()),
            (None, None) => {
                Err("Give a player name when running this without being a player.".into())
            }
        }
    }

    /// The record key of the command's player, and whether that is the sender.
    fn target_of(
        &mut self,
        host: &dyn Host,
        who: &Who,
        target: Option<&str>,
    ) -> Result<(String, bool), String> {
        let p = Self::resolve_target(host, who, target)?;
        Ok((self.record(&p.name, &p.uuid), target.is_none()))
    }

    /// The issuing player's cooldown; admins are exempt.
    fn check_cooldown(&self, who: &Who, now_ms: u64) -> Result<(), String> {
        if !who.admin
            && let Issuer::Player { key, .. } = &who.issuer
            && let Err(secs) = self.cooldowns.check(key, now_ms)
        {
            return Err(format!("Wait {secs} s before the next skin command."));
        }
        Ok(())
    }

    /// Cooldown of the issuing player and the target's pending lookup.
    fn guard(&self, who: &Who, pending_key: &str, now_ms: u64) -> Result<(), String> {
        self.check_cooldown(who, now_ms)?;
        if self.ops.is_pending(pending_key) {
            return Err("A skin lookup is already running for that player.".into());
        }
        Ok(())
    }

    /// Cooldown and pending guard, then a lookup that ends in `purpose`.
    fn start_lookup(
        &mut self,
        who: &Who,
        pending_key: &str,
        name: &str,
        sources: &[Source],
        purpose: Purpose,
        now_ms: u64,
    ) -> Result<(), String> {
        if sources.is_empty() {
            return Err(format!("{name} is blocked."));
        }
        self.guard(who, pending_key, now_ms)?;
        match self.engine.submit(name, sources, now_ms) {
            Ok(q) => {
                let started = self.ops.begin(pending_key);
                debug_assert!(started, "the guard refuses a second lookup");
                self.purposes.insert(q, (name.to_string(), purpose));
                Ok(())
            }
            Err(_) => Err("Skin service busy, try again later.".into()),
        }
    }

    /// What `/skin status` says about a player.
    fn describe(&self, key: &str, now_secs: u64) -> String {
        let record = &self.players.players[key];
        let label = match &record.choice {
            Choice::Skin { .. } => format!("{} (chosen)", record.choice),
            Choice::Auto => "automatic".to_string(),
        };
        let Some(plan) = self.plan_for(key, now_secs) else {
            return format!("{}: {label}.", record.name);
        };
        let next = match &plan.apply {
            Apply::Steve => "Steve".to_string(),
            Apply::Keep => "the profile's own skin".to_string(),
            Apply::Skin(skin) => {
                // The cache entry the plan took the skin from.
                let chosen = match &record.choice {
                    Choice::Skin { source, name } => Some((*source, name.as_str())),
                    Choice::Auto => None,
                };
                let own = self
                    .cfg
                    .source_order
                    .iter()
                    .map(|s| (*s, record.name.as_str()));
                chosen
                    .into_iter()
                    .chain(own)
                    .find_map(|(source, name)| {
                        let c = self.cache.get(source, name)?;
                        (c.value == skin.value).then(|| {
                            format!(
                                "{}, fetched {} min ago",
                                skin_key(source, name),
                                now_secs.saturating_sub(c.fetched_at) / 60
                            )
                        })
                    })
                    .unwrap_or_else(|| "a cached skin".to_string())
            }
        };
        let mut notes = String::new();
        if record.locked {
            notes.push_str(" (locked)");
        }
        if plan.drop_choice {
            notes.push_str(" (the chosen skin is blocked or gone)");
        }
        format!("{}: {label}; next join: {next}{notes}.", record.name)
    }

    fn edit_block_list(
        &mut self,
        files: &dyn Files,
        entry: &str,
        add: bool,
    ) -> Result<String, String> {
        let (current, recreated) = config_text(files)?;
        // The file is re-read as on `/skin reload`, also when the list stays as it is. An
        // invalid file is refused and the previous config stays.
        let current_cfg = config::parse(&current)?;
        let edited = if add {
            config::add_blocked(&current, entry)?
        } else {
            config::remove_blocked(&current, entry)?
        };
        let reply = match edited {
            Some(edited) => {
                let cfg = config::parse(&edited)?;
                files.write(CONFIG_PATH, &edited)?;
                self.cfg = cfg;
                if add {
                    format!("Blocked {entry}.")
                } else {
                    format!("Unblocked {entry}.")
                }
            }
            None => {
                self.cfg = current_cfg;
                if add {
                    format!("{entry} was already blocked.")
                } else {
                    format!("{entry} was not blocked.")
                }
            }
        };
        Ok(recreated_note(recreated, reply))
    }

    fn lock_command(&mut self, host: &dyn Host, name: &str, lock: bool) -> Result<String, String> {
        let p = host
            .player_by_name(name)
            .ok_or_else(|| format!("{name} is not online."))?;
        let name = p.name.as_str();
        let key = self.record(name, &p.uuid);
        let record = self.players.players.get_mut(&key).expect("record exists");
        if record.locked == lock {
            return Ok(if lock {
                format!("{name} is already locked.")
            } else {
                format!("{name} is already unlocked.")
            });
        }
        record.locked = lock;
        self.ops.bump(&key);
        self.players_dirty = true;
        Ok(if lock {
            format!("{name} is locked to Steve from the next join.")
        } else {
            format!("{name} is unlocked.")
        })
    }

    fn set_command(
        &mut self,
        env: &Env,
        who: &Who,
        key: &str,
        own: bool,
        spec: &SkinSpec,
    ) -> Result<(String, bool), String> {
        let now = env.now_ms;
        let record = &self.players.players[key];
        if record.locked {
            return Err(format!("{} is locked.", record.name));
        }
        let sources = self.sources_for(spec.source, &spec.name);
        if sources.is_empty() {
            return Err(format!("{} is blocked.", spec.name));
        }
        if let Some((source, _)) = self.fresh_cached(&sources, &spec.name, env.now_secs) {
            self.guard(who, key, now)?;
            let choice = Choice::skin(source, &spec.name);
            let label = choice.to_string();
            let cap = self.cfg.history_length;
            let record = self.players.players.get_mut(key).expect("record exists");
            if own {
                record.set_own(choice, cap);
            } else {
                record.choice = choice;
            }
            self.ops.bump(key);
            self.players_dirty = true;
            self.start_cooldown(&who.issuer, true, now);
            return Ok((
                format!("Skin set to {label}. It applies on the next join."),
                false,
            ));
        }
        let purpose = Purpose::Set {
            issuer: who.issuer.clone(),
            own,
            target_key: key.to_string(),
            revision: self.ops.revision(key),
        };
        self.start_lookup(who, key, &spec.name, &sources, purpose, now)?;
        Ok((format!("Looking up {}...", spec.name), true))
    }

    /// Runs `/skin`; returns the reply and whether a lookup was started.
    fn run_skin(
        &mut self,
        env: &Env,
        sender: &Sender,
        cmd: SkinCmd,
    ) -> Result<(String, bool), String> {
        let now = env.now_ms;
        let who = self.sender_of(sender);
        if cmd.needs_admin() && !who.admin {
            return Err("You are not allowed to do that.".into());
        }
        if matches!(cmd, SkinCmd::History | SkinCmd::Undo) && who.player.is_none() {
            return Err("This command is for players.".into());
        }
        let reply = match cmd {
            SkinCmd::Help => format!("Usage: {SKIN_USAGE}"),
            SkinCmd::Set { spec, target } => {
                let (key, own) = self.target_of(env.host, &who, target.as_deref())?;
                return self.set_command(env, &who, &key, own, &spec);
            }
            SkinCmd::Clear { target } => {
                let (key, own) = self.target_of(env.host, &who, target.as_deref())?;
                self.check_cooldown(&who, now)?;
                let cap = self.cfg.history_length;
                let record = self.players.players.get_mut(&key).expect("record exists");
                if record.locked {
                    return Err(format!("{} is locked.", record.name));
                }
                if record.choice == Choice::Auto {
                    // Nothing to write, but a change that is still being looked up
                    // must not replace automatic mode afterwards.
                    self.ops.bump(&key);
                    return Ok(("Skin is already automatic.".into(), false));
                }
                if own {
                    record.set_own(Choice::Auto, cap);
                } else {
                    record.choice = Choice::Auto;
                }
                self.ops.bump(&key);
                self.players_dirty = true;
                self.start_cooldown(&who.issuer, false, now);
                "Skin cleared: back to automatic. It applies on the next join.".into()
            }
            SkinCmd::Update { target } => {
                let (key, _) = self.target_of(env.host, &who, target.as_deref())?;
                let record = &self.players.players[&key];
                let (name, sources) = match &record.choice {
                    Choice::Skin { source, name } => {
                        (name.clone(), self.sources_for(Some(*source), name))
                    }
                    Choice::Auto if !valid_name(&record.name) => {
                        return Err(format!(
                            "There is no skin lookup for the name {}.",
                            record.name
                        ));
                    }
                    Choice::Auto => (record.name.clone(), self.sources_for(None, &record.name)),
                };
                let purpose = Purpose::Update {
                    issuer: who.issuer.clone(),
                    target_key: key.clone(),
                };
                self.start_lookup(&who, &key, &name, &sources, purpose, now)?;
                return Ok((format!("Looking up {name}..."), true));
            }
            SkinCmd::Status { target } => {
                let (key, _) = self.target_of(env.host, &who, target.as_deref())?;
                self.describe(&key, env.now_secs)
            }
            SkinCmd::History => {
                let (key, _) = self.target_of(env.host, &who, None)?;
                let record = &self.players.players[&key];
                let mut lines = vec![format!("Current: {}", record.choice)];
                for (i, c) in record.history.iter().rev().enumerate() {
                    lines.push(format!("{}. {c}", i + 1));
                }
                lines.join("\n")
            }
            SkinCmd::Undo => {
                let (key, _) = self.target_of(env.host, &who, None)?;
                self.check_cooldown(&who, now)?;
                let cfg = &self.cfg;
                let record = self.players.players.get_mut(&key).expect("record exists");
                if record.locked {
                    return Err("Your skin is locked.".into());
                }
                let history_len = record.history.len();
                let restored = record.undo(|c| match c {
                    Choice::Skin { source, name } => cfg.is_blocked(*source, name),
                    Choice::Auto => false,
                });
                // Entries popped on the way (blocked or equal to the current choice)
                // stay gone, also when nothing was restored.
                if record.history.len() != history_len {
                    self.ops.bump(&key);
                    self.players_dirty = true;
                    self.start_cooldown(&who.issuer, false, now);
                }
                match restored {
                    Some(restored) => {
                        format!("Skin back to {restored}. It applies on the next join.")
                    }
                    None => return Err("Nothing to undo.".into()),
                }
            }
            SkinCmd::Lock(name) => self.lock_command(env.host, &name, true)?,
            SkinCmd::Unlock(name) => self.lock_command(env.host, &name, false)?,
            SkinCmd::Block(source, name) => {
                self.edit_block_list(env.files, &skin_key(source, &name), true)?
            }
            SkinCmd::Unblock(source, name) => {
                self.edit_block_list(env.files, &skin_key(source, &name), false)?
            }
            SkinCmd::Reload => {
                let (text, recreated) = config_text(env.files)?;
                self.cfg = config::parse(&text)?;
                recreated_note(recreated, "Config reloaded.".into())
            }
        };
        Ok((reply, false))
    }

    /// `/skin <raw>`.
    pub fn skin_command(&mut self, env: &Env, sender: &Sender, raw: &str) -> Reply {
        match parse_skin(raw).and_then(|cmd| self.run_skin(env, sender, cmd)) {
            Ok((text, deferred)) => Reply {
                text: Ok(text),
                give: None,
                deferred,
            },
            Err(e) => Reply::now(Err(e)),
        }
    }

    fn run_skull(&mut self, env: &Env, sender: &Sender, raw: &str) -> Result<Reply, String> {
        let now = env.now_ms;
        let (spec, target) = parse_skull(raw)?;
        let who = self.sender_of(sender);
        if target.is_some() && !who.admin {
            return Err("You are not allowed to give heads to others.".into());
        }
        let recipient = Self::resolve_target(env.host, &who, target.as_deref())?;
        let recipient_key = self.record(&recipient.name, &recipient.uuid);
        let sources = self.sources_for(spec.source, &spec.name);
        if let Some((_, skin)) = self.fresh_cached(&sources, &spec.name, env.now_secs) {
            self.guard(&who, &recipient_key, now)?;
            self.start_cooldown(&who.issuer, true, now);
            return Ok(Reply {
                text: Ok("Head given.".into()),
                give: Some(Head {
                    recipient: recipient.uuid,
                    name: spec.name,
                    skin,
                }),
                deferred: false,
            });
        }
        let purpose = Purpose::Skull {
            issuer: who.issuer.clone(),
            recipient: recipient.uuid,
            recipient_name: recipient.name,
            recipient_key: recipient_key.clone(),
            name: spec.name.clone(),
            admin_required: target.is_some(),
        };
        self.start_lookup(&who, &recipient_key, &spec.name, &sources, purpose, now)?;
        Ok(Reply {
            text: Ok(format!("Looking up {}...", spec.name)),
            give: None,
            deferred: true,
        })
    }

    /// `/skull <raw>`.
    pub fn skull_command(&mut self, env: &Env, sender: &Sender, raw: &str) -> Reply {
        self.run_skull(env, sender, raw)
            .unwrap_or_else(|e| Reply::now(Err(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{HttpRequest, HttpResponse};
    use base64::Engine as _;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeHost {
        players: RefCell<Vec<PlayerInfo>>,
        logs: RefCell<Vec<(LogLevel, String)>>,
    }

    impl Host for FakeHost {
        fn player_by_name(&self, name: &str) -> Option<PlayerInfo> {
            let players = self.players.borrow();
            players
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .cloned()
        }
        fn player_by_uuid(&self, uuid: &str) -> Option<PlayerInfo> {
            self.players
                .borrow()
                .iter()
                .find(|p| p.uuid == uuid)
                .cloned()
        }
        fn log(&self, level: LogLevel, msg: &str) {
            self.logs.borrow_mut().push((level, msg.to_string()));
        }
    }

    #[derive(Default)]
    struct FakeFiles {
        files: RefCell<HashMap<String, String>>,
        /// Paths that cannot be read.
        unreadable: RefCell<Vec<String>>,
        /// Paths written, in order.
        writes: RefCell<Vec<String>>,
    }

    impl Files for FakeFiles {
        fn read(&self, path: &str) -> Result<Option<String>, String> {
            if self.unreadable.borrow().iter().any(|p| p == path) {
                return Err(format!("{path}: stream did not contain valid UTF-8"));
            }
            Ok(self.files.borrow().get(path).cloned())
        }
        fn write(&self, path: &str, contents: &str) -> Result<(), String> {
            self.files
                .borrow_mut()
                .insert(path.to_string(), contents.to_string());
            self.writes.borrow_mut().push(path.to_string());
            Ok(())
        }
    }

    /// A server with a data folder and both clocks.
    #[derive(Default)]
    struct World {
        host: FakeHost,
        files: FakeFiles,
        now_ms: u64,
        now_secs: u64,
    }

    impl World {
        fn new() -> World {
            World {
                now_secs: 10_000_000,
                ..World::default()
            }
        }

        fn env(&self) -> Env<'_> {
            Env {
                host: &self.host,
                files: &self.files,
                now_ms: self.now_ms,
                now_secs: self.now_secs,
            }
        }

        fn load(&self) -> State {
            State::load(&self.env(), false).unwrap()
        }

        fn advance(&mut self, secs: u64) {
            self.now_ms += secs * 1000;
            self.now_secs += secs;
        }

        fn join(&self, name: &str, op_level: u8) -> Sender {
            let info = PlayerInfo {
                name: name.into(),
                uuid: uuid_of(name),
                op_level,
            };
            self.host.players.borrow_mut().push(info.clone());
            Sender::Player(info)
        }

        fn writes(&self, path: &str) -> usize {
            self.files
                .writes
                .borrow()
                .iter()
                .filter(|p| *p == path)
                .count()
        }

        fn file(&self, path: &str) -> String {
            self.files.files.borrow()[path].clone()
        }

        /// Answers every request the engine wants until no lookup is left, then
        /// finishes the lookups. Time moves on a second per round, past the batch
        /// window.
        fn answer(
            &mut self,
            state: &mut State,
            respond: impl Fn(&HttpRequest) -> Result<HttpResponse, String>,
        ) -> Vec<Effect> {
            for _ in 0..4 {
                self.advance(1);
                loop {
                    let reqs = state.engine.poll_requests(self.now_ms);
                    if reqs.is_empty() {
                        break;
                    }
                    for (id, req) in reqs {
                        state.engine.on_response(id, respond(&req), self.now_ms);
                    }
                }
                if state.engine.queued() == 0 {
                    break;
                }
            }
            state.finish_all(&self.env())
        }
    }

    fn uuid_of(name: &str) -> String {
        format!("00000000-0000-0000-0000-{:0>12}", name.to_ascii_lowercase())
    }

    fn textures(url: &str) -> String {
        base64::engine::general_purpose::STANDARD
            .encode(format!(r#"{{"textures":{{"SKIN":{{"url":"{url}"}}}}}}"#))
    }

    fn status(code: u16) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: code,
            retry_after: None,
            body: Vec::new(),
        })
    }

    fn ok(body: String) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: body.into_bytes(),
        })
    }

    /// A signed profile whose skin is `http://textures.minecraft.net/texture/<id>`.
    fn profile(id: &str) -> Result<HttpResponse, String> {
        let value = textures(&format!("http://textures.minecraft.net/texture/{id}"));
        ok(format!(
            r#"{{"id":"u","name":"n","properties":[{{"name":"textures","value":"{value}","signature":"sig"}}]}}"#
        ))
    }

    /// Ely.by has a skin for every name (its texture id is the name); Mojang has none.
    fn ely_has_all(req: &HttpRequest) -> Result<HttpResponse, String> {
        match req.host {
            "skinsystem.ely.by" => profile(req.path.rsplit('/').next().unwrap()),
            _ => status(204),
        }
    }

    fn nobody_has_any(_: &HttpRequest) -> Result<HttpResponse, String> {
        status(204)
    }

    fn fails(_: &HttpRequest) -> Result<HttpResponse, String> {
        Err("timed out".into())
    }

    fn cached(id: &str, at: u64) -> CachedSkin {
        CachedSkin {
            value: textures(&format!("http://textures.minecraft.net/texture/{id}")),
            signature: "sig".into(),
            uuid: String::new(),
            fetched_at: at,
        }
    }

    fn text(reply: &Reply) -> &str {
        match &reply.text {
            Ok(t) | Err(t) => t,
        }
    }

    fn choice(state: &State, key: &str) -> Choice {
        state.players.players[key].choice.clone()
    }

    #[test]
    fn first_start_writes_the_default_config_only() {
        let w = World::new();
        let state = w.load();
        assert_eq!(w.file(CONFIG_PATH), DEFAULT_CONFIG);
        assert_eq!(state.cfg, Config::default());
        assert_eq!(w.files.writes.borrow().len(), 1);
    }

    #[test]
    fn corrupt_cache_is_replaced_and_corrupt_players_stop_the_load() {
        let w = World::new();
        w.files.write(SKINS_PATH, "{broken").unwrap();
        let state = w.load();
        assert_eq!(state.cache, Cache::default());
        let logs = w.host.logs.borrow();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, LogLevel::Error);
        assert!(logs[0].1.contains(SKINS_PATH), "{}", logs[0].1);
        // The broken file is left alone until there is something to save.
        assert_eq!(w.file(SKINS_PATH), "{broken");

        w.files.write(PLAYERS_PATH, "{broken").unwrap();
        let e = State::load(&w.env(), false).err().unwrap();
        assert!(e.starts_with(PLAYERS_PATH), "{e}");
        assert_eq!(w.file(PLAYERS_PATH), "{broken");
    }

    #[test]
    fn an_unreadable_cache_is_replaced_and_unreadable_players_stop_the_load() {
        let w = World::new();
        w.files.unreadable.borrow_mut().push(SKINS_PATH.into());
        let state = w.load();
        assert_eq!(state.cache, Cache::default());
        assert!(w.host.logs.borrow()[0].1.contains("valid UTF-8"));
        w.files.unreadable.borrow_mut().push(PLAYERS_PATH.into());
        assert!(State::load(&w.env(), false).is_err());
        *w.files.unreadable.borrow_mut() = vec![CONFIG_PATH.into()];
        assert!(State::load(&w.env(), false).is_err());
    }

    #[test]
    fn clear_cancels_a_pending_set_even_when_already_automatic() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set ely:Notch");
        let reply = state.skin_command(&w.env(), &alice, "clear");
        assert_eq!(reply.text, Ok("Skin is already automatic.".into()));
        let effects = w.answer(&mut state, ely_has_all);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "Skin change superseded: the skin was changed meanwhile."),
            "{effects:?}"
        );
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        // The same for an admin clearing someone else.
        w.advance(30);
        state.skin_command(&w.env(), &alice, "set ely:Notch");
        state.skin_command(&w.env(), &Sender::Console, "clear Alice");
        w.answer(&mut state, ely_has_all);
        assert_eq!(choice(&state, "alice"), Choice::Auto);
    }

    #[test]
    fn a_choice_found_gone_by_someone_else_is_dropped_at_the_next_login() {
        let mut w = World::new();
        let mut state = w.load();
        state.record("Alice", &uuid_of("alice"));
        state.players.players.get_mut("alice").unwrap().choice = Choice::skin(Source::Ely, "gone");
        // Bob asks for the same skin while Alice is offline.
        let bob = w.join("Bob", 0);
        state.skin_command(&w.env(), &bob, "set ely:gone");
        w.answer(&mut state, nobody_has_any);
        assert_eq!(choice(&state, "alice"), Choice::skin(Source::Ely, "gone"));
        // Alice is automatic again and her own name is looked up.
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        let effects = w.answer(&mut state, ely_has_all);
        assert_eq!(effects.len(), 1, "{effects:?}");
        assert!(state.cache.get(Source::Ely, "alice").is_some());
    }

    #[test]
    fn old_cache_entries_are_dropped_at_load_and_hourly() {
        let mut w = World::new();
        let mut cache = Cache::default();
        cache.put(Source::Ely, "old", cached("old", 0));
        cache.put(Source::Ely, "new", cached("new", w.now_secs));
        w.files.write(SKINS_PATH, &to_json(&cache)).unwrap();
        let mut state = w.load();
        assert_eq!(state.cache.skins.keys().collect::<Vec<_>>(), ["ely:new"]);
        state.flush(&w.env(), false);
        assert!(!w.file(SKINS_PATH).contains("ely:old"));

        // A month later the other one goes too, at the hourly check.
        w.advance(store::CACHE_KEEP_SECS);
        state.flush(&w.env(), false);
        assert!(state.cache.skins.is_empty());
        assert_eq!(w.file(SKINS_PATH), r#"{"skins":{},"misses":{}}"#);
    }

    #[test]
    fn login_writes_nothing_until_the_flush() {
        let mut w = World::new();
        let mut state = w.load();
        let apply = state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(apply, Apply::Steve);
        assert_eq!(w.writes(PLAYERS_PATH), 0);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(PLAYERS_PATH), 1);
        assert!(w.file(PLAYERS_PATH).contains("Alice"));
        // Nothing changed: nothing written.
        w.advance(10);
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(PLAYERS_PATH), 1);
    }

    #[test]
    fn login_lookup_caches_the_skin_and_tells_the_player() {
        let mut w = World::new();
        let mut state = w.load();
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        let effects = w.answer(&mut state, ely_has_all);
        assert_eq!(
            effects,
            vec![Effect::TellPlayer(
                uuid_of("alice"),
                "Your skin is ready, rejoin to see it.".into()
            )]
        );
        // The next login applies it without a lookup, and nobody is told again.
        let apply = state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert!(matches!(apply, Apply::Skin(_)));
        assert!(w.answer(&mut state, fails).is_empty());
        assert!(w.host.logs.borrow().is_empty());
    }

    #[test]
    fn the_cache_file_is_compact_and_written_at_most_every_five_seconds() {
        let mut w = World::new();
        let mut state = w.load();
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, ely_has_all);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(SKINS_PATH), 1);
        assert!(!w.file(SKINS_PATH).contains('\n'));

        state.login("Bob", &uuid_of("bob"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, ely_has_all);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(SKINS_PATH), 1);
        assert!(!w.file(SKINS_PATH).contains("ely:bob"));
        w.advance(4);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(SKINS_PATH), 2);
        assert!(w.file(SKINS_PATH).contains("ely:bob"));
    }

    #[test]
    fn a_forced_flush_writes_the_cache_at_once() {
        let mut w = World::new();
        let mut state = w.load();
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, ely_has_all);
        state.flush(&w.env(), false);
        state.login("Bob", &uuid_of("bob"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, ely_has_all);
        state.flush(&w.env(), true);
        assert!(w.file(SKINS_PATH).contains("ely:bob"));
    }

    #[test]
    fn a_name_without_any_skin_is_not_looked_up_again_until_the_miss_expires() {
        let mut w = World::new();
        let mut state = w.load();
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert!(w.answer(&mut state, nobody_has_any).is_empty());
        assert_eq!(
            state.cache.misses.keys().collect::<Vec<_>>(),
            ["ely:alice", "mojang:alice"]
        );
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(state.engine.queued(), 0);
        // A failed lookup is no miss.
        state.login("Bob", &uuid_of("bob"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, fails);
        assert!(!state.cache.misses.contains_key("ely:bob"));
        assert_eq!(w.host.logs.borrow()[0].0, LogLevel::Warn);

        w.advance(15 * 60);
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(state.engine.queued(), 1);
    }

    #[test]
    fn failed_login_lookups_are_logged_from_the_start_and_throttled() {
        let mut w = World::new();
        let mut state = w.load();
        for name in ["a", "b", "c"] {
            state.login(name, &uuid_of(name), false, w.now_ms, w.now_secs);
            w.answer(&mut state, fails);
        }
        assert_eq!(w.host.logs.borrow().len(), 1);
        w.advance(60);
        state.login("d", &uuid_of("d"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, fails);
        let logs = w.host.logs.borrow();
        assert_eq!(logs.len(), 2);
        assert!(logs[1].1.contains("2 similar suppressed"), "{}", logs[1].1);
    }

    #[test]
    fn set_looks_up_commits_and_starts_the_cooldown() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        let reply = state.skin_command(&w.env(), &alice, "set ely:Notch");
        assert_eq!(reply.text, Ok("Looking up Notch...".into()));
        assert!(reply.deferred);
        // One lookup per player at a time.
        let again = state.skin_command(&w.env(), &alice, "update");
        assert_eq!(
            text(&again),
            "A skin lookup is already running for that player."
        );
        let effects = w.answer(&mut state, ely_has_all);
        let issuer = Issuer::Player {
            uuid: uuid_of("alice"),
            key: "alice".into(),
        };
        assert_eq!(
            effects,
            vec![Effect::Tell(
                issuer,
                "Skin set to ely:notch. It applies on the next join.".into()
            )]
        );
        assert_eq!(choice(&state, "alice"), Choice::skin(Source::Ely, "notch"));
        assert_eq!(state.players.players["alice"].history, vec![Choice::Auto]);
        let soon = state.skin_command(&w.env(), &alice, "set ely:jeb_");
        assert_eq!(
            soon.text,
            Err("Wait 30 s before the next skin command.".into())
        );
        state.flush(&w.env(), false);
        assert!(w.file(PLAYERS_PATH).contains("ely:notch"));
    }

    #[test]
    fn set_from_a_fresh_cache_entry_needs_no_lookup() {
        let w = World::new();
        let mut state = w.load();
        state
            .cache
            .put(Source::Ely, "notch", cached("notch", w.now_secs));
        let alice = w.join("Alice", 0);
        let reply = state.skin_command(&w.env(), &alice, "set Notch");
        assert_eq!(
            reply.text,
            Ok("Skin set to ely:notch. It applies on the next join.".into())
        );
        assert!(!reply.deferred);
        assert_eq!(state.engine.queued(), 0);
    }

    #[test]
    fn a_failed_set_changes_nothing() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set mojang:Notch");
        let effects = w.answer(&mut state, nobody_has_any);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "Skin not changed: no skin found."),
            "{effects:?}"
        );
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        let soon = state.skin_command(&w.env(), &alice, "set mojang:Notch");
        assert_eq!(
            soon.text,
            Err("Wait 5 s before the next skin command.".into())
        );
    }

    #[test]
    fn a_lock_during_the_lookup_supersedes_the_set() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set ely:Notch");
        let locked = state.skin_command(&w.env(), &Sender::Console, "lock alice");
        assert_eq!(
            locked.text,
            Ok("Alice is locked to Steve from the next join.".into())
        );
        let effects = w.answer(&mut state, ely_has_all);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m.starts_with("Skin change superseded")),
            "{effects:?}"
        );
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        assert_eq!(
            state.login("Alice", &uuid_of("alice"), true, w.now_ms, w.now_secs),
            Apply::Steve
        );
    }

    #[test]
    fn clear_does_nothing_when_already_automatic_and_has_a_cooldown() {
        let w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        let reply = state.skin_command(&w.env(), &alice, "clear");
        assert_eq!(reply.text, Ok("Skin is already automatic.".into()));
        state.flush(&w.env(), false);
        let written = w.writes(PLAYERS_PATH);

        // No cooldown was started by the no-op.
        state
            .cache
            .put(Source::Ely, "notch", cached("notch", w.now_secs));
        state.players.players.get_mut("alice").unwrap().choice = Choice::skin(Source::Ely, "notch");
        let reply = state.skin_command(&w.env(), &alice, "clear");
        assert!(text(&reply).starts_with("Skin cleared"), "{reply:?}");
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        state.flush(&w.env(), false);
        assert_eq!(w.writes(PLAYERS_PATH), written + 1);
        for cmd in ["clear", "undo", "set Notch"] {
            let reply = state.skin_command(&w.env(), &alice, cmd);
            assert_eq!(
                reply.text,
                Err("Wait 5 s before the next skin command.".into()),
                "{cmd}"
            );
        }
        state.flush(&w.env(), false);
        assert_eq!(w.writes(PLAYERS_PATH), written + 1);
    }

    #[test]
    fn admins_have_no_cooldown() {
        let w = World::new();
        let mut state = w.load();
        state
            .cache
            .put(Source::Ely, "notch", cached("notch", w.now_secs));
        let admin = w.join("Admin", 4);
        for _ in 0..3 {
            assert!(
                state
                    .skin_command(&w.env(), &admin, "set Notch")
                    .text
                    .is_ok()
            );
            assert!(state.skin_command(&w.env(), &admin, "clear").text.is_ok());
        }
    }

    #[test]
    fn undo_restores_the_previous_choice() {
        let mut w = World::new();
        let mut state = w.load();
        state.cache.put(Source::Ely, "a", cached("a", w.now_secs));
        state.cache.put(Source::Ely, "b", cached("b", w.now_secs));
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set a");
        w.advance(30);
        state.skin_command(&w.env(), &alice, "set b");
        w.advance(30);
        let reply = state.skin_command(&w.env(), &alice, "undo");
        assert_eq!(
            reply.text,
            Ok("Skin back to ely:a. It applies on the next join.".into())
        );
        let history = state.skin_command(&w.env(), &alice, "history");
        assert_eq!(history.text, Ok("Current: ely:a\n1. auto".into()));
        assert_eq!(
            state.skin_command(&w.env(), &Sender::Console, "undo").text,
            Err("This command is for players.".into())
        );
    }

    #[test]
    fn a_choice_without_a_cached_skin_keeps_the_automatic_skin_meanwhile() {
        let mut w = World::new();
        let mut state = w.load();
        state
            .cache
            .put(Source::Mojang, "alice", cached("own", w.now_secs));
        state.cache.put_miss(Source::Ely, "alice", w.now_secs);
        state.record("Alice", &uuid_of("alice"));
        state.players.players.get_mut("alice").unwrap().choice = Choice::skin(Source::Ely, "notch");
        let apply = state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(apply, Apply::Skin(cached("own", 0).skin()));
        let effects = w.answer(&mut state, ely_has_all);
        assert_eq!(effects.len(), 1, "{effects:?}");
        let apply = state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(apply, Apply::Skin(cached("notch", 0).skin()));
    }

    #[test]
    fn a_choice_whose_skin_vanished_goes_back_to_automatic() {
        let mut w = World::new();
        let mut state = w.load();
        state.record("Alice", &uuid_of("alice"));
        state.players.players.get_mut("alice").unwrap().choice = Choice::skin(Source::Ely, "gone");
        state.flush(&w.env(), false);
        state.login("Alice", &uuid_of("alice"), true, w.now_ms, w.now_secs);
        assert!(w.answer(&mut state, nobody_has_any).is_empty());
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        state.flush(&w.env(), false);
        assert!(!w.file(PLAYERS_PATH).contains("ely:gone"));
        // No more lookups for it.
        assert_eq!(
            state.login("Alice", &uuid_of("alice"), true, w.now_ms, w.now_secs),
            Apply::Keep
        );
        assert_eq!(state.engine.queued(), 0);
    }

    #[test]
    fn a_failed_lookup_keeps_the_choice() {
        let mut w = World::new();
        let mut state = w.load();
        state.record("Alice", &uuid_of("alice"));
        let chosen = Choice::skin(Source::Ely, "notch");
        state.players.players.get_mut("alice").unwrap().choice = chosen.clone();
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, fails);
        assert_eq!(choice(&state, "alice"), chosen);
    }

    #[test]
    fn update_says_when_the_choice_is_gone() {
        let mut w = World::new();
        let mut state = w.load();
        state
            .cache
            .put(Source::Ely, "gone", cached("gone", w.now_secs));
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set gone");
        w.advance(30);
        let reply = state.skin_command(&w.env(), &alice, "update");
        assert_eq!(reply.text, Ok("Looking up gone...".into()));
        let effects = w.answer(&mut state, nobody_has_any);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m
                == "Skin not updated: no skin found. ely:gone no longer exists; back to automatic."),
            "{effects:?}"
        );
        assert_eq!(choice(&state, "alice"), Choice::Auto);
        assert_eq!(state.cache.get(Source::Ely, "gone"), None);
    }

    #[test]
    fn update_only_promises_a_skin_the_next_login_applies() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        w.answer(&mut state, nobody_has_any);
        state.skin_command(&w.env(), &alice, "update");
        let effects = w.answer(&mut state, ely_has_all);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "Updated ely:alice. It applies on the next join."),
            "{effects:?}"
        );
        // A player whose profile has its own skin keeps that one.
        let bob = w.join("Bob", 0);
        state.login("Bob", &uuid_of("bob"), true, w.now_ms, w.now_secs);
        state.skin_command(&w.env(), &bob, "update");
        let effects = w.answer(&mut state, ely_has_all);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "Updated ely:bob."),
            "{effects:?}"
        );
    }

    #[test]
    fn update_refuses_a_name_no_source_can_have() {
        let w = World::new();
        let mut state = w.load();
        let odd = w.join(".Bedrock Player", 0);
        let reply = state.skin_command(&w.env(), &odd, "update");
        assert_eq!(
            reply.text,
            Err("There is no skin lookup for the name .Bedrock Player.".into())
        );
        assert_eq!(state.engine.queued(), 0);
    }

    #[test]
    fn status_reports_what_the_next_login_applies() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        state.login("Alice", &uuid_of("alice"), true, w.now_ms, w.now_secs);
        let status = |state: &mut State, w: &World| {
            state.skin_command(&w.env(), &alice, "status").text.unwrap()
        };
        assert_eq!(
            status(&mut state, &w),
            "Alice: automatic; next join: the profile's own skin."
        );
        state.login("Alice", &uuid_of("alice"), false, w.now_ms, w.now_secs);
        assert_eq!(
            status(&mut state, &w),
            "Alice: automatic; next join: Steve."
        );
        state
            .cache
            .put(Source::Mojang, "alice", cached("own", w.now_secs));
        w.advance(120);
        assert_eq!(
            status(&mut state, &w),
            "Alice: automatic; next join: mojang:alice, fetched 2 min ago."
        );
        state
            .cache
            .put(Source::Ely, "notch", cached("notch", w.now_secs));
        state.players.players.get_mut("alice").unwrap().choice = Choice::skin(Source::Ely, "notch");
        assert_eq!(
            status(&mut state, &w),
            "Alice: ely:notch (chosen); next join: ely:notch, fetched 0 min ago."
        );
        state.skin_command(&w.env(), &Sender::Console, "block ely:notch");
        assert_eq!(
            status(&mut state, &w),
            "Alice: ely:notch (chosen); next join: mojang:alice, fetched 2 min ago (the chosen skin is blocked or gone)."
        );
        state.skin_command(&w.env(), &Sender::Console, "lock Alice");
        assert!(status(&mut state, &w).contains("next join: Steve (locked)"));
    }

    #[test]
    fn admin_commands_need_an_admin_and_find_players_ignoring_case() {
        let w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        let admin = w.join("Admin", 4);
        for cmd in [
            "lock Admin",
            "reload",
            "block ely:x",
            "clear Admin",
            "status Admin",
        ] {
            let reply = state.skin_command(&w.env(), &alice, cmd);
            assert_eq!(
                reply.text,
                Err("You are not allowed to do that.".into()),
                "{cmd}"
            );
        }
        let reply = state.skin_command(&w.env(), &admin, "lock ALICE");
        assert_eq!(
            reply.text,
            Ok("Alice is locked to Steve from the next join.".into())
        );
        assert!(state.players.players["alice"].locked);
        assert_eq!(
            state.skin_command(&w.env(), &admin, "lock Nobody").text,
            Err("Nobody is not online.".into())
        );
        assert_eq!(
            state.skin_command(&w.env(), &alice, "set Notch").text,
            Err("Alice is locked.".into())
        );
        assert_eq!(
            state
                .skin_command(&w.env(), &Sender::Console, "status")
                .text,
            Err("Give a player name when running this without being a player.".into())
        );
    }

    #[test]
    fn block_edits_the_config_file_and_applies_it() {
        let w = World::new();
        let mut state = w.load();
        let reply = state.skin_command(&w.env(), &Sender::Console, "block Mojang:Notch");
        assert_eq!(reply.text, Ok("Blocked mojang:notch.".into()));
        assert!(
            w.file(CONFIG_PATH)
                .contains(r#"blocked = ["mojang:notch"]"#)
        );
        let alice = w.join("Alice", 0);
        assert_eq!(
            state
                .skin_command(&w.env(), &alice, "set mojang:Notch")
                .text,
            Err("Notch is blocked.".into())
        );
        let reply = state.skin_command(&w.env(), &Sender::Console, "block mojang:notch");
        assert_eq!(reply.text, Ok("mojang:notch was already blocked.".into()));
        let reply = state.skin_command(&w.env(), &Sender::Console, "unblock mojang:notch");
        assert_eq!(reply.text, Ok("Unblocked mojang:notch.".into()));
        assert_eq!(w.file(CONFIG_PATH), DEFAULT_CONFIG);
    }

    #[test]
    fn reload_rejects_an_invalid_file_and_recreates_a_missing_one() {
        let w = World::new();
        let mut state = w.load();
        w.files.write(CONFIG_PATH, "cache_minutes = 5\n").unwrap();
        let reply = state.skin_command(&w.env(), &Sender::Console, "reload");
        assert_eq!(reply.text, Ok("Config reloaded.".into()));
        assert_eq!(state.cfg.cache_minutes, 5);
        w.files.write(CONFIG_PATH, "cache_minuts = 7\n").unwrap();
        assert!(
            state
                .skin_command(&w.env(), &Sender::Console, "reload")
                .text
                .is_err()
        );
        assert_eq!(state.cfg.cache_minutes, 5);
        w.files.files.borrow_mut().remove(CONFIG_PATH);
        let reply = state.skin_command(&w.env(), &Sender::Console, "reload");
        assert_eq!(
            reply.text,
            Ok("config.toml was missing; recreated with defaults. Config reloaded.".into())
        );
        assert_eq!(state.cfg, Config::default());
    }

    #[test]
    fn a_sender_that_is_neither_player_nor_console_gets_the_later_reply() {
        let mut w = World::new();
        let mut state = w.load();
        w.join("Alice", 0);
        let rcon = Sender::Other {
            op_level: 4,
            reply: 7,
        };
        let reply = state.skin_command(&w.env(), &rcon, "set ely:Notch Alice");
        assert!(reply.deferred);
        let effects = w.answer(&mut state, ely_has_all);
        assert_eq!(
            effects,
            vec![Effect::Tell(
                Issuer::Other {
                    reply: 7,
                    admin: true
                },
                "Skin set to ely:notch. It applies on the next join.".into()
            )]
        );
        // Admin changes do not add to the player's history.
        assert!(state.players.players["alice"].history.is_empty());
        // Without the operator level it is no admin.
        let weak = Sender::Other {
            op_level: 2,
            reply: 8,
        };
        let reply = state.skin_command(&w.env(), &weak, "set ely:Notch Alice");
        assert_eq!(reply.text, Err("You are not allowed to do that.".into()));
        assert!(!reply.deferred);
    }

    #[test]
    fn an_admin_who_left_loses_a_pending_change_for_someone_else() {
        let mut w = World::new();
        let mut state = w.load();
        w.join("Alice", 0);
        let admin = w.join("Admin", 4);
        state.skin_command(&w.env(), &admin, "set ely:Notch Alice");
        w.host.players.borrow_mut().retain(|p| p.name != "Admin");
        let effects = w.answer(&mut state, ely_has_all);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "Skin change superseded: you are no longer an admin."),
            "{effects:?}"
        );
        assert_eq!(choice(&state, "alice"), Choice::Auto);
    }

    #[test]
    fn skull_from_the_cache_is_given_at_once() {
        let w = World::new();
        let mut state = w.load();
        state
            .cache
            .put(Source::Ely, "notch", cached("notch", w.now_secs));
        let alice = w.join("Alice", 0);
        let reply = state.skull_command(&w.env(), &alice, "Notch");
        assert_eq!(reply.text, Ok("Head given.".into()));
        assert_eq!(
            reply.give,
            Some(Head {
                recipient: uuid_of("alice"),
                name: "Notch".into(),
                skin: cached("notch", 0).skin(),
            })
        );
        assert_eq!(
            state.skull_command(&w.env(), &alice, "Notch").text,
            Err("Wait 30 s before the next skin command.".into())
        );
    }

    #[test]
    fn skull_lookup_gives_the_head_later_and_others_need_an_admin() {
        let mut w = World::new();
        let mut state = w.load();
        let alice = w.join("Alice", 0);
        w.join("Bob", 0);
        assert_eq!(
            state.skull_command(&w.env(), &alice, "Notch Bob").text,
            Err("You are not allowed to give heads to others.".into())
        );
        let reply = state.skull_command(&w.env(), &Sender::Console, "ely:Notch bob");
        assert_eq!(reply.text, Ok("Looking up Notch...".into()));
        assert!(reply.deferred && reply.give.is_none());
        let effects = w.answer(&mut state, ely_has_all);
        assert_eq!(
            effects,
            vec![Effect::GiveSkull {
                issuer: Issuer::Console,
                recipient: uuid_of("bob"),
                recipient_name: "Bob".into(),
                name: "Notch".into(),
                skin: cached("notch", 0).skin(),
            }]
        );
        let reply = state.skull_command(&w.env(), &alice, "mojang:Notch");
        assert!(reply.deferred);
        let effects = w.answer(&mut state, nobody_has_any);
        assert!(
            matches!(&effects[..], [Effect::Tell(_, m)] if m == "No head: no skin found."),
            "{effects:?}"
        );
    }

    #[test]
    fn an_older_answer_does_not_overwrite_a_newer_one() {
        let mut w = World::new();
        let mut state = w.load();
        // A: Ely has nothing for "x", the lookup goes on to Mojang and waits there.
        state.login("x", &uuid_of("x"), false, w.now_ms, w.now_secs);
        let reqs = state.engine.poll_requests(w.now_ms);
        assert_eq!(reqs.len(), 1);
        state.engine.on_response(reqs[0].0, status(204), w.now_ms);
        // B: meanwhile Ely gets a skin for "x" and someone asks for it.
        let alice = w.join("Alice", 0);
        state.skin_command(&w.env(), &alice, "set ely:x");
        let reqs = state.engine.poll_requests(w.now_ms);
        assert_eq!(reqs.len(), 1);
        state.engine.on_response(reqs[0].0, profile("x"), w.now_ms);
        assert_eq!(state.finish_all(&w.env()).len(), 1);
        assert!(state.cache.get(Source::Ely, "x").is_some());
        // A finishes last, carrying the old Ely "not found": the skin stays.
        let effects = w.answer(&mut state, nobody_has_any);
        assert!(state.cache.get(Source::Ely, "x").is_some());
        assert!(!state.cache.misses.contains_key("ely:x"));
        assert!(state.cache.misses.contains_key("mojang:x"));
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(choice(&state, "alice"), Choice::skin(Source::Ely, "x"));
    }
}
