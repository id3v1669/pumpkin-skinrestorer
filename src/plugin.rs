//! Pumpkin plugin ABI v0.2 glue around the pure modules.
//!
//! - `player-login-event` (blocking): apply a stored skin with `set-skin` and queue
//!   background lookups ([`resolve::plan_login`]).
//! - [`TICK_TASK`], every tick: start the requests the [`Engine`] wants, poll running
//!   ones, act on finished lookups.
//! - `/skin`, `/skull`: one greedy string argument, parsed by [`crate::command`].
//!
//! All state lives in [`STATE`]. It is only borrowed inside synchronous sections; host
//! calls that must be awaited happen after the borrow ends.

// The generated bindings contain host calls with many parameters.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::time::{SystemTime, UNIX_EPOCH};

use exports::pumpkin::plugin::{gametest_callbacks, metadata};
use pumpkin::plugin::command::{
    Arg, ArgumentType, Command, CommandNode, PermissionLevel as SenderLevel, StringType,
};
use pumpkin::plugin::data_components::DataComponent;
use pumpkin::plugin::event::{EventPriority, EventType};
use pumpkin::plugin::item_stack::ItemStack;
use pumpkin::plugin::logging::{Level, log};
use pumpkin::plugin::permission::{Permission, PermissionDefault, PermissionLevel};
use pumpkin::plugin::player::{Player, PlayerSkin};
use pumpkin::plugin::scheduler::schedule_repeating_task;
use pumpkin::plugin::text::TextComponent;
use pumpkin::plugin::uuid::{self as wit_uuid, Uuid};

use crate::command::{SKIN_USAGE, SkinCmd, SkinSpec, parse_skin, parse_skull};
use crate::config::{self, Config, DEFAULT_CONFIG, skin_key};
use crate::http;
use crate::lookup::{Engine, Outcome, QueryId, ReqId};
use crate::resolve::{
    Apply, Cooldowns, Ops, Superseded, is_admin, may_commit, notice_due, plan_login,
};
use crate::skin::{profile_component, skin_url, steve};
use crate::source::{SkinData, Source};
use crate::store::{
    self, Cache, CacheSeq, CachedSkin, Choice, PlayerRecord, Players, player_key, to_json,
};

const LOGIN_HANDLER: u32 = 1;
const TICK_TASK: u32 = 1;
const SKIN_COMMAND: u32 = 1;
const SKULL_COMMAND: u32 = 2;
/// Granted to everyone; admin rights are checked by the plugin itself.
const COMMAND_PERMISSION: &str = "skinrestorer:command";

/// Failed login lookups are logged at most this often.
const LOOKUP_WARNING_INTERVAL_MS: u64 = 60_000;

const NOT_LOADED: &str = "skinrestorer is not loaded.";

const CONFIG_PATH: &str = "data/config.toml";
const PLAYERS_PATH: &str = "data/players.json";
const SKINS_PATH: &str = "data/skins.json";

/// Who ran a command, for replies that arrive after the command returned.
#[derive(Clone)]
enum Issuer {
    Console,
    Player { id: Uuid, key: String },
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
        recipient: Uuid,
        recipient_name: String,
        recipient_key: String,
        /// Account name as typed; it names the head.
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
    player: Uuid,
    /// Key of the player's record.
    key: String,
    /// Skin URL applied at their login, if any: a player is only told about a skin
    /// they are not already wearing.
    applied: Option<String>,
    /// Whether their profile had a skin of its own at login.
    has_own_skin: bool,
}

/// Host calls to make once the state borrow has ended.
enum Effect {
    Tell(Issuer, String),
    TellPlayer(Uuid, String),
    GiveSkull {
        issuer: Issuer,
        recipient: Uuid,
        recipient_name: String,
        name: String,
        skin: SkinData,
    },
}

struct State {
    cfg: Config,
    online_mode: bool,
    players: Players,
    cache: Cache,
    /// Which answer last wrote each cache entry; see [`CacheSeq`].
    cache_seq: CacheSeq,
    engine: Engine,
    http: HashMap<ReqId, http::Request>,
    /// Running lookups: the name looked up and what it is for.
    purposes: HashMap<QueryId, (String, Purpose)>,
    ops: Ops,
    cooldowns: Cooldowns,
    /// Running login lookups by dedupe key.
    login_lookups: HashMap<String, QueryId>,
    /// `skins.json` needs writing at the end of the tick.
    cache_dirty: bool,
    /// When a failed login lookup was last logged; see [`LOOKUP_WARNING_INTERVAL_MS`].
    last_lookup_warning_ms: u64,
    /// Failed login lookups not logged since then.
    suppressed_lookup_warnings: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn text(s: &str) -> TextComponent {
    TextComponent::text(s)
}

fn read_file(path: &str) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// Writes to a temporary file, then renames it over `path`.
fn write_atomic(path: &str, contents: &str) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    fs::write(&tmp, contents).map_err(|e| format!("{tmp}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

fn save(path: &str, contents: String) {
    if let Err(e) = write_atomic(path, &contents) {
        log(Level::Error, &format!("could not save {e}"));
    }
}

impl State {
    fn save_players(&self) {
        save(PLAYERS_PATH, to_json(&self.players));
    }

    fn save_cache(&self) {
        save(SKINS_PATH, to_json(&self.cache));
    }

    /// The record for a player, created if missing, with name and UUID kept current.
    /// The flag tells whether it was created or modified.
    fn record(&mut self, name: &str, uuid: &str) -> (String, &mut PlayerRecord, bool) {
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
        (key, record, changed)
    }

    /// Sources to try for `spec`, without those where the name is blocked.
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
    fn fresh_cached(&self, sources: &[Source], name: &str, now: u64) -> Option<(Source, SkinData)> {
        let &first = sources.first()?;
        self.cache
            .get(first, name)
            .filter(|c| !c.is_expired(now / 1000, self.cfg.cache_minutes))
            .map(|c| (first, c.skin()))
    }
}

fn uuid_string(id: Uuid) -> String {
    wit_uuid::to_string(id)
}

fn player_level(level: PermissionLevel) -> u8 {
    match level {
        PermissionLevel::Zero => 0,
        PermissionLevel::One => 1,
        PermissionLevel::Two => 2,
        PermissionLevel::Three => 3,
        PermissionLevel::Four => 4,
    }
}

fn sender_level(level: SenderLevel) -> u8 {
    match level {
        SenderLevel::Zero => 0,
        SenderLevel::One => 1,
        SenderLevel::Two => 2,
        SenderLevel::Three => 3,
        SenderLevel::Four => 4,
    }
}

fn player_is_admin(player: &Player, cfg: &Config) -> bool {
    let uuid = uuid_string(player.get_id());
    is_admin(
        player_level(player.get_permission_level()),
        Some(&uuid),
        cfg,
    )
}

fn issuer_is_admin(server: &ServerInstance, issuer: &Issuer, cfg: &Config) -> bool {
    match issuer {
        Issuer::Console => true,
        Issuer::Player { id, .. } => server
            .get_player_by_uuid(*id)
            .is_some_and(|p| player_is_admin(&p, cfg)),
    }
}

fn set_skin(player: &Player, skin: &SkinData) {
    player.set_skin(&PlayerSkin {
        value: skin.value.clone(),
        signature: Some(skin.signature.clone()),
    });
}

// --- login ---------------------------------------------------------------------------

fn on_login(player: &Player) {
    let name = player.get_name();
    let id = player.get_id();
    let uuid = uuid_string(id);
    let has_own_skin = player.get_skin().is_some_and(|s| !s.value.is_empty());
    let now = now_ms();
    let apply = with_state(|s| {
        let (key, record, mut changed) = s.record(&name, &uuid);
        let record = record.clone();
        let plan = plan_login(
            Some(&record),
            &name,
            has_own_skin,
            &s.cache,
            &s.cfg,
            now / 1000,
        );
        if plan.drop_choice {
            if let Some(r) = s.players.players.get_mut(&key) {
                r.choice = Choice::Auto;
            }
            s.ops.bump(&key);
            changed = true;
        }
        if changed {
            s.save_players();
        }
        if let Some(wanted) = plan.lookup {
            let dedupe = format!("{}|{:?}", wanted.name, wanted.sources);
            let applied = match &plan.apply {
                Apply::Skin(skin) => skin_url(&skin.value),
                Apply::Steve | Apply::Keep => None,
            };
            let running = s.login_lookups.get(&dedupe).copied();
            let waiter = LoginWaiter {
                player: id,
                key,
                applied,
                has_own_skin,
            };
            if let Some((_, Purpose::Login { waiters, .. })) =
                running.and_then(|q| s.purposes.get_mut(&q))
            {
                // Same lookup already running: wait for it too. A rejoining player
                // replaces their earlier entry.
                waiters.retain(|w| wit_uuid::to_string(w.player) != uuid);
                waiters.push(waiter);
            } else if let Ok(q) = s.engine.submit(&wanted.name, &wanted.sources, now) {
                s.login_lookups.insert(dedupe.clone(), q);
                s.purposes.insert(
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
    });
    match apply {
        Some(Apply::Steve) => set_skin(player, &steve()),
        Some(Apply::Skin(skin)) => set_skin(player, &skin),
        Some(Apply::Keep) | None => {}
    }
}

// --- tick ------------------------------------------------------------------------------

fn superseded_reason(s: &Superseded) -> &'static str {
    match s {
        Superseded::Changed => "the skin was changed meanwhile",
        Superseded::Locked => "the player is locked",
        Superseded::Blocked => "that skin is blocked",
        Superseded::NotAdmin => "you are no longer an admin",
    }
}

fn start_cooldown(s: &mut State, issuer: &Issuer, success: bool, now: u64) {
    if let Issuer::Player { key, .. } = issuer {
        s.cooldowns.start(key, success, now);
    }
}

/// Commits a found skin as the target's choice, after rechecking the rules.
fn commit_choice(
    s: &mut State,
    server: &ServerInstance,
    issuer: &Issuer,
    own: bool,
    target_key: &str,
    revision: u64,
    choice: Choice,
) -> Result<(), Superseded> {
    let Choice::Skin { source, name } = &choice else {
        unreachable!("lookups always find a concrete skin");
    };
    let blocked = s.cfg.is_blocked(*source, name);
    let still_admin = (!own).then(|| issuer_is_admin(server, issuer, &s.cfg));
    let current = s.ops.revision(target_key);
    let cap = s.cfg.history_length;
    let record = s
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
    s.ops.bump(target_key);
    s.save_players();
    Ok(())
}

/// Logs a failed login lookup, at most once per [`LOOKUP_WARNING_INTERVAL_MS`]; the
/// next warning counts the ones left out.
fn warn_failed_lookup(s: &mut State, name: &str, error: &str, now: u64) {
    if now.saturating_sub(s.last_lookup_warning_ms) < LOOKUP_WARNING_INTERVAL_MS {
        s.suppressed_lookup_warnings += 1;
        return;
    }
    let mut message = format!("skin lookup for {name} failed: {error}");
    let suppressed = std::mem::take(&mut s.suppressed_lookup_warnings);
    if suppressed > 0 {
        message.push_str(&format!(" ({suppressed} similar suppressed)"));
    }
    s.last_lookup_warning_ms = now;
    log(Level::Warn, &message);
}

/// Acts on a finished lookup of `lookup_name`.
fn finish(
    s: &mut State,
    server: &ServerInstance,
    lookup_name: &str,
    purpose: Purpose,
    outcome: Outcome,
    now: u64,
) -> Vec<Effect> {
    // A source that no longer has a skin for the name must not keep winning from the
    // cache. The answer may be old by now (the lookup went on to other sources): an
    // entry written from a later answer stays. Removals come before this outcome's own
    // write.
    for &(source, not_found_seq) in outcome.not_found_in() {
        if s.cache_seq
            .should_remove(source, lookup_name, not_found_seq)
        {
            s.cache_seq.forget(source, lookup_name);
            if s.cache.remove(source, lookup_name) {
                s.cache_dirty = true;
            }
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
            s.cache_seq.record_write(*source, name, *answered_seq);
            s.cache.put(
                *source,
                name,
                CachedSkin {
                    value: skin.value.clone(),
                    signature: skin.signature.clone(),
                    uuid: uuid.clone(),
                    fetched_at: now / 1000,
                },
            );
            s.cache_dirty = true;
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
            s.ops.end(&target_key);
            let message = match found {
                Some((source, name, _)) => {
                    let choice = Choice::skin(source, &name);
                    let label = choice.to_string();
                    match commit_choice(s, server, &issuer, own, &target_key, revision, choice) {
                        Ok(()) => {
                            start_cooldown(s, &issuer, true, now);
                            format!("Skin set to {label}. It applies on the next join.")
                        }
                        Err(why) => {
                            start_cooldown(s, &issuer, false, now);
                            format!("Skin change superseded: {}.", superseded_reason(&why))
                        }
                    }
                }
                None => {
                    start_cooldown(s, &issuer, false, now);
                    format!("Skin not changed: {failure}.")
                }
            };
            effects.push(Effect::Tell(issuer, message));
        }
        Purpose::Update { issuer, target_key } => {
            s.ops.end(&target_key);
            start_cooldown(s, &issuer, found.is_some(), now);
            let message = match found {
                Some((source, name, _)) => format!(
                    "Updated {}. It applies on the next join.",
                    skin_key(source, &name)
                ),
                None => format!("Skin not updated: {failure}."),
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
            s.ops.end(&recipient_key);
            // The rules may have changed while the lookup ran.
            let refused = match &found {
                Some((source, found_name, _)) if s.cfg.is_blocked(*source, found_name) => {
                    Some(Superseded::Blocked)
                }
                Some(_) if admin_required && !issuer_is_admin(server, &issuer, &s.cfg) => {
                    Some(Superseded::NotAdmin)
                }
                _ => None,
            };
            start_cooldown(s, &issuer, found.is_some() && refused.is_none(), now);
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
                (None, _) => effects.push(Effect::Tell(issuer, format!("No head: {failure}."))),
            }
        }
        Purpose::Login { dedupe, waiters } => {
            s.login_lookups.remove(&dedupe);
            if let Outcome::Failed { error, .. } = &outcome {
                warn_failed_lookup(s, lookup_name, error, now);
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
                    let due = s.players.players.get(&waiter.key).is_some_and(|record| {
                        let plan = plan_login(
                            Some(record),
                            &record.name,
                            waiter.has_own_skin,
                            &s.cache,
                            &s.cfg,
                            now / 1000,
                        );
                        notice_due(&plan, &url, waiter.applied.as_deref())
                    });
                    if due {
                        effects.push(Effect::TellPlayer(
                            waiter.player,
                            "Your skin is ready, rejoin to see it.".into(),
                        ));
                    }
                }
            }
        }
    }
    effects
}

async fn give_skull(player: &Player, name: &str, skin: &SkinData) {
    let head = ItemStack::new("minecraft:player_head", 1);
    head.set_component(DataComponent::Profile, profile_component(name, skin))
        .await;
    player.get_inventory().give_item(head, true).await;
}

fn tell_player(server: &ServerInstance, id: Uuid, msg: &str) {
    if let Some(p) = server.get_player_by_uuid(id) {
        p.send_system_message(text(msg), false);
    }
}

fn tell(server: &ServerInstance, issuer: &Issuer, msg: &str) {
    match issuer {
        Issuer::Console => log(Level::Info, msg),
        Issuer::Player { id, .. } => tell_player(server, *id, msg),
    }
}

async fn run_effects(server: &ServerInstance, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Tell(issuer, msg) => tell(server, &issuer, &msg),
            Effect::TellPlayer(id, msg) => tell_player(server, id, &msg),
            Effect::GiveSkull {
                issuer,
                recipient,
                recipient_name,
                name,
                skin,
            } => match server.get_player_by_uuid(recipient) {
                Some(p) => {
                    give_skull(&p, &name, &skin).await;
                    tell(server, &issuer, "Head given.");
                }
                None => tell(
                    server,
                    &issuer,
                    &format!("Head not given: {recipient_name} left."),
                ),
            },
        }
    }
}

async fn tick(server: &ServerInstance) {
    let now = now_ms();
    let effects = with_state(|s| {
        for (id, req) in s.engine.poll_requests(now) {
            match http::Request::start(&req) {
                Ok(r) => {
                    s.http.insert(id, r);
                }
                Err(e) => s.engine.on_response(id, Err(e), now),
            }
        }
        let done: Vec<_> = s
            .http
            .iter_mut()
            .filter_map(|(id, r)| r.poll().map(|res| (*id, res)))
            .collect();
        for (id, result) in done {
            s.http.remove(&id);
            s.engine.on_response(id, result, now);
        }
        let mut effects = Vec::new();
        for (q, outcome) in s.engine.take_finished() {
            if let Some((name, purpose)) = s.purposes.remove(&q) {
                effects.extend(finish(s, server, &name, purpose, outcome, now));
            }
        }
        if std::mem::take(&mut s.cache_dirty) {
            s.save_cache();
        }
        effects
    })
    .unwrap_or_default();
    run_effects(server, effects).await;
}

// --- commands --------------------------------------------------------------------------

/// The command's player: the target if given (must be online), else the sender.
fn resolve_target(
    server: &ServerInstance,
    sender: Option<&Player>,
    target: Option<&str>,
) -> Result<Player, String> {
    match (target, sender) {
        (Some(name), _) => server
            .get_player_by_name(name)
            .ok_or_else(|| format!("{name} is not online.")),
        (None, Some(p)) => Ok(server
            .get_player_by_uuid(p.get_id())
            .ok_or("You are not online.")?),
        (None, None) => Err("Give a player name when running this from the console.".into()),
    }
}

struct Sender {
    issuer: Issuer,
    player: Option<Player>,
    admin: bool,
}

fn sender_of(s: &mut State, sender: &CommandSender) -> Sender {
    match sender.as_player() {
        Some(p) => {
            let uuid = uuid_string(p.get_id());
            let (key, _, _) = s.record(&p.get_name(), &uuid);
            Sender {
                issuer: Issuer::Player {
                    id: p.get_id(),
                    key,
                },
                admin: player_is_admin(&p, &s.cfg),
                player: Some(p),
            }
        }
        None => Sender {
            issuer: Issuer::Console,
            player: None,
            admin: sender.is_console()
                || is_admin(sender_level(sender.permission_level()), None, &s.cfg),
        },
    }
}

/// Cooldown of the issuing player (admins exempt) and the target's pending lookup.
fn guard(s: &State, who: &Sender, pending_key: &str, now: u64) -> Result<(), String> {
    if !who.admin
        && let Issuer::Player { key, .. } = &who.issuer
        && let Err(secs) = s.cooldowns.check(key, now)
    {
        return Err(format!("Wait {secs} s before the next skin command."));
    }
    if s.ops.is_pending(pending_key) {
        return Err("A skin lookup is already running for that player.".into());
    }
    Ok(())
}

/// Cooldown and pending guard, then a lookup that ends in `purpose`.
fn start_lookup(
    s: &mut State,
    who: &Sender,
    pending_key: &str,
    name: &str,
    sources: &[Source],
    purpose: Purpose,
    now: u64,
) -> Result<QueryId, String> {
    if sources.is_empty() {
        return Err(format!("{name} is blocked."));
    }
    guard(s, who, pending_key, now)?;
    s.ops.begin(pending_key);
    match s.engine.submit(name, sources, now) {
        Ok(q) => {
            s.purposes.insert(q, (name.to_string(), purpose));
            Ok(q)
        }
        Err(_) => {
            s.ops.end(pending_key);
            Err("Skin service busy, try again later.".into())
        }
    }
}

fn describe(s: &State, record: &PlayerRecord, now: u64) -> String {
    let (cached_for, label) = match &record.choice {
        Choice::Skin { source, name } => (
            s.cache.get(*source, name),
            format!("{} (chosen)", record.choice),
        ),
        Choice::Auto => (
            s.cfg
                .source_order
                .iter()
                .find_map(|src| s.cache.get(*src, &record.name)),
            "automatic (own name)".to_string(),
        ),
    };
    let age = cached_for.map_or("not cached".to_string(), |c| {
        format!(
            "cached {} min ago",
            (now / 1000).saturating_sub(c.fetched_at) / 60
        )
    });
    let lock = if record.locked {
        ", locked to Steve"
    } else {
        ""
    };
    format!("{}: {label}, {age}{lock}.", record.name)
}

/// The text of `config.toml`. A missing file is recreated from the defaults, which the
/// flag reports.
fn config_text() -> Result<(String, bool), String> {
    match read_file(CONFIG_PATH)? {
        Some(t) => Ok((t, false)),
        None => {
            write_atomic(CONFIG_PATH, DEFAULT_CONFIG)?;
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

fn edit_block_list(s: &mut State, entry: &str, add: bool) -> Result<String, String> {
    let (current, recreated) = config_text()?;
    // The file is re-read as on `/skin reload`, also when the list stays as it is. An
    // invalid file is refused and the previous config stays.
    let current_cfg = config::parse(&current)?;
    if recreated {
        s.cfg = current_cfg.clone();
    }
    let edited = if add {
        config::add_blocked(&current, entry)?
    } else {
        config::remove_blocked(&current, entry)?
    };
    let reply = match edited {
        Some(edited) => {
            let cfg = config::parse(&edited)?;
            write_atomic(CONFIG_PATH, &edited)?;
            s.cfg = cfg;
            if add {
                format!("Blocked {entry}.")
            } else {
                format!("Unblocked {entry}.")
            }
        }
        None => {
            s.cfg = current_cfg;
            if add {
                format!("{entry} was already blocked.")
            } else {
                format!("{entry} was not blocked.")
            }
        }
    };
    Ok(recreated_note(recreated, reply))
}

/// Runs `/skin`; returns the reply.
fn run_skin(
    s: &mut State,
    server: &ServerInstance,
    sender: &CommandSender,
    cmd: SkinCmd,
    now: u64,
) -> Result<String, String> {
    let who = sender_of(s, sender);
    if cmd.needs_admin() && !who.admin {
        return Err("You are not allowed to do that.".into());
    }
    let target_of = |s: &mut State, target: Option<&str>| -> Result<(String, bool), String> {
        let p = resolve_target(server, who.player.as_ref(), target)?;
        let (key, _, _) = s.record(&p.get_name(), &uuid_string(p.get_id()));
        Ok((key, target.is_none()))
    };
    if matches!(cmd, SkinCmd::History | SkinCmd::Undo) && who.player.is_none() {
        return Err("This command is for players.".into());
    }
    let reply = match cmd {
        SkinCmd::Help => format!("Usage: {SKIN_USAGE}"),
        SkinCmd::Set { spec, target } => {
            let (key, own) = target_of(s, target.as_deref())?;
            set_skin_command(s, &who, &key, own, &spec, now)?
        }
        SkinCmd::Clear { target } => {
            let (key, own) = target_of(s, target.as_deref())?;
            let cap = s.cfg.history_length;
            let record = s.players.players.get_mut(&key).expect("record exists");
            if record.locked {
                return Err(format!("{} is locked.", record.name));
            }
            if own {
                record.set_own(Choice::Auto, cap);
            } else {
                record.choice = Choice::Auto;
            }
            s.ops.bump(&key);
            s.save_players();
            "Skin cleared: back to automatic. It applies on the next join.".into()
        }
        SkinCmd::Update { target } => {
            let (key, _) = target_of(s, target.as_deref())?;
            let record = s.players.players[&key].clone();
            let (name, sources) = match &record.choice {
                Choice::Skin { source, name } => (name.clone(), s.sources_for(Some(*source), name)),
                Choice::Auto => (record.name.clone(), s.sources_for(None, &record.name)),
            };
            let purpose = Purpose::Update {
                issuer: who.issuer.clone(),
                target_key: key.clone(),
            };
            start_lookup(s, &who, &key, &name, &sources, purpose, now)?;
            format!("Looking up {name}...")
        }
        SkinCmd::Status { target } => {
            let (key, _) = target_of(s, target.as_deref())?;
            describe(s, &s.players.players[&key], now)
        }
        SkinCmd::History => {
            let (key, _) = target_of(s, None)?;
            let record = &s.players.players[&key];
            let mut lines = vec![format!("Current: {}", record.choice)];
            for (i, c) in record.history.iter().rev().enumerate() {
                lines.push(format!("{}. {c}", i + 1));
            }
            lines.join("\n")
        }
        SkinCmd::Undo => {
            let (key, _) = target_of(s, None)?;
            let cfg = s.cfg.clone();
            let record = s.players.players.get_mut(&key).expect("record exists");
            if record.locked {
                return Err("Your skin is locked.".into());
            }
            let history_len = record.history.len();
            let restored = record.undo(|c| match c {
                Choice::Skin { source, name } => cfg.is_blocked(*source, name),
                Choice::Auto => false,
            });
            let Some(restored) = restored else {
                // Entries popped on the way (blocked or equal to the current choice)
                // stay gone after a restart.
                if record.history.len() != history_len {
                    s.ops.bump(&key);
                    s.save_players();
                }
                return Err("Nothing to undo.".into());
            };
            s.ops.bump(&key);
            s.save_players();
            format!("Skin back to {restored}. It applies on the next join.")
        }
        SkinCmd::Lock(name) => lock_command(s, server, &name, true)?,
        SkinCmd::Unlock(name) => lock_command(s, server, &name, false)?,
        SkinCmd::Block(source, name) => edit_block_list(s, &skin_key(source, &name), true)?,
        SkinCmd::Unblock(source, name) => edit_block_list(s, &skin_key(source, &name), false)?,
        SkinCmd::Reload => {
            let (text, recreated) = config_text()?;
            s.cfg = config::parse(&text)?;
            recreated_note(recreated, "Config reloaded.".into())
        }
    };
    Ok(reply)
}

fn set_skin_command(
    s: &mut State,
    who: &Sender,
    key: &str,
    own: bool,
    spec: &SkinSpec,
    now: u64,
) -> Result<String, String> {
    let record = &s.players.players[key];
    if record.locked {
        return Err(format!("{} is locked.", record.name));
    }
    let sources = s.sources_for(spec.source, &spec.name);
    if sources.is_empty() {
        return Err(format!("{} is blocked.", spec.name));
    }
    if let Some((source, _)) = s.fresh_cached(&sources, &spec.name, now) {
        guard(s, who, key, now)?;
        let choice = Choice::skin(source, &spec.name);
        let label = choice.to_string();
        let cap = s.cfg.history_length;
        let record = s.players.players.get_mut(key).expect("record exists");
        if own {
            record.set_own(choice, cap);
        } else {
            record.choice = choice;
        }
        s.ops.bump(key);
        s.save_players();
        start_cooldown(s, &who.issuer, true, now);
        return Ok(format!("Skin set to {label}. It applies on the next join."));
    }
    let purpose = Purpose::Set {
        issuer: who.issuer.clone(),
        own,
        target_key: key.to_string(),
        revision: s.ops.revision(key),
    };
    start_lookup(s, who, key, &spec.name, &sources, purpose, now)?;
    Ok(format!("Looking up {}...", spec.name))
}

fn command_text(args: &ConsumedArgs) -> String {
    match args.get_value("args") {
        Arg::Simple(s) | Arg::Msg(s) => s,
        _ => String::new(),
    }
}

/// A head to give right away: recipient, account name, skin.
type Head = (Player, String, SkinData);

async fn handle_skull(sender: &CommandSender, server: &ServerInstance, raw: &str) {
    let now = now_ms();
    let result = with_state(|s| -> Result<(String, Option<Head>), String> {
        let (spec, target) = parse_skull(raw)?;
        let who = sender_of(s, sender);
        if target.is_some() && !who.admin {
            return Err("You are not allowed to give heads to others.".into());
        }
        let recipient = resolve_target(server, who.player.as_ref(), target.as_deref())?;
        let recipient_name = recipient.get_name();
        let (recipient_key, _, _) = s.record(&recipient_name, &uuid_string(recipient.get_id()));
        let sources = s.sources_for(spec.source, &spec.name);
        if let Some((_, skin)) = s.fresh_cached(&sources, &spec.name, now) {
            guard(s, &who, &recipient_key, now)?;
            start_cooldown(s, &who.issuer, true, now);
            return Ok((
                "Head given.".into(),
                Some((recipient, spec.name.clone(), skin)),
            ));
        }
        let purpose = Purpose::Skull {
            issuer: who.issuer.clone(),
            recipient: recipient.get_id(),
            recipient_name,
            recipient_key: recipient_key.clone(),
            name: spec.name.clone(),
            admin_required: target.is_some(),
        };
        start_lookup(s, &who, &recipient_key, &spec.name, &sources, purpose, now)?;
        Ok((format!("Looking up {}...", spec.name), None))
    });
    match result {
        Some(Ok((reply, give))) => {
            if let Some((player, name, skin)) = give {
                give_skull(&player, &name, &skin).await;
            }
            sender.send_message(text(&reply));
        }
        Some(Err(e)) => sender.send_error(text(&e)),
        None => sender.send_error(text(NOT_LOADED)),
    }
}

fn lock_command(
    s: &mut State,
    server: &ServerInstance,
    name: &str,
    lock: bool,
) -> Result<String, String> {
    let p = server
        .get_player_by_name(name)
        .ok_or_else(|| format!("{name} is not online."))?;
    let (key, record, _) = s.record(&p.get_name(), &uuid_string(p.get_id()));
    if record.locked == lock {
        return Ok(if lock {
            format!("{name} is already locked.")
        } else {
            format!("{name} is already unlocked.")
        });
    }
    record.locked = lock;
    s.ops.bump(&key);
    s.save_players();
    Ok(if lock {
        format!("{name} is locked to Steve from the next join.")
    } else {
        format!("{name} is unlocked.")
    })
}

// --- exports ---------------------------------------------------------------------------

fn load_state(online_mode: bool) -> Result<State, String> {
    let _ = fs::create_dir_all("data");
    let (config_text, _) = config_text()?;
    let cfg = config::parse(&config_text)?;
    let players: Players = store::load(read_file(PLAYERS_PATH)?.as_deref())
        .map_err(|e| format!("{PLAYERS_PATH}: {e}"))?;
    let cache: Cache =
        store::load(read_file(SKINS_PATH)?.as_deref()).map_err(|e| format!("{SKINS_PATH}: {e}"))?;
    Ok(State {
        cfg,
        online_mode,
        players,
        cache,
        cache_seq: CacheSeq::default(),
        engine: Engine::new(),
        http: HashMap::new(),
        purposes: HashMap::new(),
        ops: Ops::default(),
        cooldowns: Cooldowns::default(),
        login_lookups: HashMap::new(),
        cache_dirty: false,
        last_lookup_warning_ms: 0,
        suppressed_lookup_warnings: 0,
    })
}

fn register_command(context: &Context, name: &str, description: &str, handler: u32) {
    let command = Command::new(&[name.to_string()], description);
    command.execute_with_handler_id(handler);
    let args = CommandNode::argument("args", &ArgumentType::String(StringType::Greedy));
    args.execute_with_handler_id(handler);
    command.then(args);
    context.register_command(command, COMMAND_PERMISSION);
}

struct SkinRestorer;

impl Guest for SkinRestorer {
    fn init_plugin() {}

    async fn on_load(context: Context) -> Result<(), String> {
        let state = load_state(context.get_server().is_online_mode()).inspect_err(|e| {
            log(Level::Error, &format!("skinrestorer not loaded: {e}"));
        })?;
        STATE.with_borrow_mut(|s| *s = Some(state));

        let permission = Permission {
            node: COMMAND_PERMISSION.into(),
            description: "Use /skin and /skull".into(),
            default: PermissionDefault::Allow,
            children: Vec::new(),
        };
        if let Err(e) = context.register_permission(&permission) {
            log(
                Level::Warn,
                &format!("could not register {COMMAND_PERMISSION}: {e}"),
            );
        }
        context.register_event(
            LOGIN_HANDLER,
            EventType::PlayerLoginEvent,
            EventPriority::Normal,
            true,
        );
        register_command(&context, "skin", "Ely.by and Mojang skins", SKIN_COMMAND);
        register_command(&context, "skull", "Player head with a skin", SKULL_COMMAND);
        schedule_repeating_task(TICK_TASK, 1, 1);
        log(Level::Info, "skinrestorer loaded");
        Ok(())
    }

    async fn on_unload(_context: Context) -> Result<(), String> {
        STATE.with_borrow_mut(|s| *s = None);
        Ok(())
    }

    async fn handle_event(event_id: u32, _server: ServerInstance, event: Event) -> Event {
        if event_id == LOGIN_HANDLER
            && let Event::PlayerLoginEvent(data) = &event
            && !data.cancelled
        {
            on_login(&data.player);
        }
        event
    }

    async fn handle_task(handler_id: u32, server: ServerInstance) {
        if handler_id == TICK_TASK {
            tick(&server).await;
        }
    }

    async fn handle_command(
        command_id: u32,
        sender: CommandSender,
        server: ServerInstance,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let raw = command_text(&args);
        if command_id == SKULL_COMMAND {
            handle_skull(&sender, &server, &raw).await;
            return Ok(1);
        }
        let now = now_ms();
        let result = with_state(|s| run_skin(s, &server, &sender, parse_skin(&raw)?, now));
        match result {
            Some(Ok(reply)) => sender.send_message(text(&reply)),
            Some(Err(e)) => sender.send_error(text(&e)),
            None => sender.send_error(text(NOT_LOADED)),
        }
        Ok(1)
    }

    async fn handle_command_suggestion(
        _handler_id: u32,
        _sender: CommandSender,
        _server: ServerInstance,
        _request: SuggestionRequest,
    ) -> CommandSuggestions {
        CommandSuggestions {
            start: 0,
            length: 0,
            values: Vec::new(),
        }
    }

    async fn handle_ipc_message(
        _sender: PluginId,
        _message: IpcMessage,
    ) -> Result<IpcMessage, String> {
        Err("skinrestorer does not handle IPC messages".into())
    }

    async fn handle_ai_goal_can_start(
        _goal_id: u32,
        _server: ServerInstance,
        _entity: Entity,
    ) -> bool {
        false
    }

    async fn handle_ai_goal_should_continue(
        _goal_id: u32,
        _server: ServerInstance,
        _entity: Entity,
    ) -> bool {
        false
    }

    async fn handle_ai_goal_start(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_ai_goal_tick(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_ai_goal_stop(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_generate_phase(
        _generator_id: u32,
        _phase: GenerationPhase,
        _chunk: ChunkBuffer,
    ) {
    }
}

impl metadata::Guest for SkinRestorer {
    fn get_metadata() -> metadata::PluginMetadata {
        metadata::PluginMetadata {
            name: "skinrestorer".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: env!("CARGO_PKG_AUTHORS")
                .split(':')
                .map(Into::into)
                .collect(),
            description: env!("CARGO_PKG_DESCRIPTION").into(),
            dependencies: Vec::new(),
            permissions: vec![
                "http.outbound".into(),
                "fs.read.data".into(),
                "fs.write.data".into(),
            ],
        }
    }
}

impl gametest_callbacks::Guest for SkinRestorer {
    fn invoke_void(_callback: gametest_callbacks::VoidCallbackId) {}

    fn invoke_test(_callback: gametest_callbacks::TestCallbackId, _test: gametest_callbacks::Test) {
    }

    async fn invoke_async_test(
        _callback: gametest_callbacks::AsyncTestCallbackId,
        _test: gametest_callbacks::Test,
    ) {
    }

    fn invoke_block_predicate(
        _callback: gametest_callbacks::BlockPredicateCallbackId,
        _permutation: gametest_callbacks::BlockPermutation,
    ) -> bool {
        false
    }

    fn invoke_entity_predicate(
        _callback: gametest_callbacks::EntityPredicateCallbackId,
        _entity: &gametest_callbacks::Entity,
    ) -> bool {
        false
    }
}

export!(SkinRestorer);
