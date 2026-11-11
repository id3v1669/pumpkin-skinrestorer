//! Pumpkin plugin ABI v0.2 bindings around [`crate::app`].
//!
//! - `player-login-event` (blocking): apply a stored skin with `set-skin`
//!   ([`State::login`]).
//! - [`TICK_TASK`], every tick: start the requests the lookup engine wants, poll running
//!   ones, act on finished lookups, write changed state files.
//! - `/skin`, `/skull`: one greedy string argument, parsed by [`crate::command`], with
//!   tab completion from the same module.
//!
//! All state lives in thread-locals. They are only borrowed inside synchronous
//! sections; host calls that must be awaited happen after the borrow ends.

// The generated bindings contain host calls with many parameters.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use exports::pumpkin::plugin::{gametest_callbacks, metadata};
use pumpkin::plugin::command::{
    Arg, ArgumentType, Command, CommandNode, CommandSuggestion, PermissionLevel as SenderLevel,
    StringType,
};
use pumpkin::plugin::data_components::DataComponent;
use pumpkin::plugin::event::{EventPriority, EventType};
use pumpkin::plugin::item_stack::ItemStack;
use pumpkin::plugin::logging::{Level, log};
use pumpkin::plugin::permission::{Permission, PermissionDefault, PermissionLevel};
use pumpkin::plugin::player::{Player, PlayerSkin};
use pumpkin::plugin::scheduler::schedule_repeating_task;
use pumpkin::plugin::text::TextComponent;
use pumpkin::plugin::uuid as wit_uuid;

use crate::app::{Effect, Env, Files, Host, Issuer, LogLevel, PlayerInfo, Sender, State};
use crate::command::{suggest_skin, suggest_skull};
use crate::http;
use crate::lookup::ReqId;
use crate::resolve::Apply;
use crate::skin::{profile_component, steve};
use crate::source::SkinData;

const LOGIN_HANDLER: u32 = 1;
const TICK_TASK: u32 = 1;
/// Also the ids of the commands' suggestion handlers.
const SKIN_COMMAND: u32 = 1;
const SKULL_COMMAND: u32 = 2;
/// Granted to everyone; admin rights are checked by the plugin itself.
const COMMAND_PERMISSION: &str = "skinrestorer:command";

const NOT_LOADED: &str = "skinrestorer is not loaded.";

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Requests in flight, by the engine's id.
    static HTTP: RefCell<HashMap<ReqId, http::Request>> = RefCell::new(HashMap::new());
    /// Senders that are neither a player nor the console and wait for the result of a
    /// lookup, by their [`Sender::Other`] reply id. The host keeps a sender handle
    /// usable until the plugin drops it.
    static SENDERS: RefCell<HashMap<u64, CommandSender>> = RefCell::new(HashMap::new());
    static NEXT_REPLY: Cell<u64> = const { Cell::new(1) };
    /// The start of the monotonic clock.
    static START: Instant = Instant::now();
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

/// Monotonic milliseconds since the plugin first asked.
fn now_ms() -> u64 {
    START.with(|start| start.elapsed().as_millis() as u64)
}

/// Unix seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn text(s: &str) -> TextComponent {
    TextComponent::text(s)
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

fn info(player: &Player) -> PlayerInfo {
    PlayerInfo {
        name: player.get_name(),
        uuid: wit_uuid::to_string(player.get_id()),
        op_level: player_level(player.get_permission_level()),
    }
}

fn find_player(server: &ServerInstance, uuid: &str) -> Option<Player> {
    server.get_player_by_uuid(wit_uuid::parse(uuid)?)
}

struct PumpkinHost<'a>(&'a ServerInstance);

impl Host for PumpkinHost<'_> {
    fn player_by_name(&self, name: &str) -> Option<PlayerInfo> {
        // The server matches the exact name only.
        let player = self.0.get_player_by_name(name).or_else(|| {
            self.0
                .get_all_players()
                .into_iter()
                .find(|p| p.get_name().eq_ignore_ascii_case(name))
        })?;
        Some(info(&player))
    }

    fn player_by_uuid(&self, uuid: &str) -> Option<PlayerInfo> {
        find_player(self.0, uuid).map(|p| info(&p))
    }

    fn log(&self, level: LogLevel, msg: &str) {
        let level = match level {
            LogLevel::Info => Level::Info,
            LogLevel::Warn => Level::Warn,
            LogLevel::Error => Level::Error,
        };
        log(level, msg);
    }
}

/// The preopened `data` folder.
struct DataFolder;

impl Files for DataFolder {
    fn read(&self, path: &str) -> Result<Option<String>, String> {
        match fs::read_to_string(path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{path}: {e}")),
        }
    }

    /// Writes to a temporary file, then renames it over `path`.
    fn write(&self, path: &str, contents: &str) -> Result<(), String> {
        let tmp = format!("{path}.tmp");
        fs::write(&tmp, contents).map_err(|e| format!("{tmp}: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
    }
}

fn env<'a>(host: &'a PumpkinHost<'a>) -> Env<'a> {
    Env {
        host,
        files: &DataFolder,
        now_ms: now_ms(),
        now_secs: now_secs(),
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
    let uuid = wit_uuid::to_string(player.get_id());
    let has_own_skin = player.get_skin().is_some_and(|s| !s.value.is_empty());
    let apply = with_state(|s| s.login(&name, &uuid, has_own_skin, now_ms(), now_secs()));
    match apply {
        Some(Apply::Steve) => set_skin(player, &steve()),
        Some(Apply::Skin(skin)) => set_skin(player, &skin),
        Some(Apply::Keep) | None => {}
    }
}

// --- tick ------------------------------------------------------------------------------

async fn give_skull(player: &Player, name: &str, skin: &SkinData) {
    let head = ItemStack::new("minecraft:player_head", 1);
    head.set_component(DataComponent::Profile, profile_component(name, skin))
        .await;
    player.get_inventory().give_item(head, true).await;
}

fn tell(server: &ServerInstance, issuer: &Issuer, msg: &str) {
    match issuer {
        Issuer::Console => log(Level::Info, msg),
        Issuer::Player { uuid, .. } => {
            if let Some(p) = find_player(server, uuid) {
                p.send_system_message(text(msg), false);
            }
        }
        // Each kept sender waits for exactly one reply.
        Issuer::Other { reply, .. } => {
            if let Some(sender) = SENDERS.with_borrow_mut(|s| s.remove(reply)) {
                sender.send_message(text(msg));
            }
        }
    }
}

async fn run_effects(server: &ServerInstance, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Tell(issuer, msg) => tell(server, &issuer, &msg),
            Effect::TellPlayer(uuid, msg) => {
                if let Some(p) = find_player(server, &uuid) {
                    p.send_system_message(text(&msg), false);
                }
            }
            Effect::GiveSkull {
                issuer,
                recipient,
                recipient_name,
                name,
                skin,
            } => match find_player(server, &recipient) {
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
    let host = PumpkinHost(server);
    let effects = with_state(|s| {
        let env = env(&host);
        let now = env.now_ms;
        HTTP.with_borrow_mut(|running| {
            for (id, req) in s.engine.poll_requests(now) {
                match http::Request::start(&req) {
                    Ok(r) => {
                        running.insert(id, r);
                    }
                    Err(e) => s.engine.on_response(id, Err(e), now),
                }
            }
            let done: Vec<_> = running
                .iter_mut()
                .filter_map(|(id, r)| r.poll().map(|res| (*id, res)))
                .collect();
            for (id, result) in done {
                running.remove(&id);
                s.engine.on_response(id, result, now);
            }
        });
        let effects = s.finish_all(&env);
        s.flush(&env, false);
        effects
    })
    .unwrap_or_default();
    run_effects(server, effects).await;
}

// --- commands --------------------------------------------------------------------------

/// Who a command sender is. `reply` is used if it is neither a player nor the console.
fn sender_kind(sender: &CommandSender, reply: u64) -> Sender {
    match sender.as_player() {
        Some(p) => Sender::Player(info(&p)),
        None if sender.is_console() => Sender::Console,
        None => Sender::Other {
            op_level: sender_level(sender.permission_level()),
            reply,
        },
    }
}

fn command_text(args: &ConsumedArgs) -> String {
    match args.get_value("args") {
        Arg::Simple(s) | Arg::Msg(s) => s,
        _ => String::new(),
    }
}

async fn run_command(command_id: u32, sender: CommandSender, server: &ServerInstance, raw: &str) {
    let reply_id = NEXT_REPLY.with(|next| next.replace(next.get() + 1));
    let kind = sender_kind(&sender, reply_id);
    let host = PumpkinHost(server);
    let reply = with_state(|s| {
        let env = env(&host);
        if command_id == SKULL_COMMAND {
            s.skull_command(&env, &kind, raw)
        } else {
            s.skin_command(&env, &kind, raw)
        }
    });
    let Some(reply) = reply else {
        sender.send_error(text(NOT_LOADED));
        return;
    };
    if let Some(head) = reply.give {
        match find_player(server, &head.recipient) {
            Some(p) => give_skull(&p, &head.name, &head.skin).await,
            None => {
                sender.send_error(text("Head not given: the player left."));
                return;
            }
        }
    }
    match &reply.text {
        Ok(msg) => sender.send_message(text(msg)),
        Err(e) => sender.send_error(text(e)),
    }
    // The result of the lookup is sent through the same handle later.
    if reply.deferred && matches!(kind, Sender::Other { .. }) {
        SENDERS.with_borrow_mut(|s| s.insert(reply_id, sender));
    }
}

fn suggestions(
    handler_id: u32,
    sender: &CommandSender,
    server: &ServerInstance,
    request: &SuggestionRequest,
) -> Vec<String> {
    let kind = sender_kind(sender, 0);
    let admin = with_state(|s| s.is_admin_sender(&kind)).unwrap_or(false);
    // Only admins may name other players.
    let players: Vec<String> = if admin {
        server
            .get_all_players()
            .iter()
            .map(Player::get_name)
            .collect()
    } else {
        Vec::new()
    };
    // `remaining` is the greedy argument as typed so far; a suggestion replaces it.
    if handler_id == SKULL_COMMAND {
        suggest_skull(&request.remaining, &players, admin)
    } else {
        suggest_skin(&request.remaining, &players, admin)
    }
}

// --- exports ---------------------------------------------------------------------------

fn register_command(context: &Context, name: &str, description: &str, handler: u32) {
    let command = Command::new(&[name.to_string()], description);
    command.execute_with_handler_id(handler);
    let args = CommandNode::argument("args", &ArgumentType::String(StringType::Greedy));
    args.execute_with_handler_id(handler);
    args.suggest_with_handler_id(handler);
    command.then(args);
    context.register_command(command, COMMAND_PERMISSION);
}

struct SkinRestorer;

impl Guest for SkinRestorer {
    fn init_plugin() {}

    async fn on_load(context: Context) -> Result<(), String> {
        let server = context.get_server();
        let host = PumpkinHost(&server);
        let _ = fs::create_dir_all("data");
        let state = State::load(&env(&host), server.is_online_mode()).inspect_err(|e| {
            log(Level::Error, &format!("skinrestorer not loaded: {e}"));
        })?;

        let permission = Permission {
            node: COMMAND_PERMISSION.into(),
            description: "Use /skin and /skull".into(),
            default: PermissionDefault::Allow,
            children: Vec::new(),
        };
        // Without the node nobody may run the commands. A node left registered by an
        // earlier load of this plugin is fine.
        if let Err(e) = context.register_permission(&permission)
            && !e.contains("already registered")
        {
            let e = format!("could not register {COMMAND_PERMISSION}: {e}");
            log(Level::Error, &format!("skinrestorer not loaded: {e}"));
            return Err(e);
        }
        STATE.with_borrow_mut(|s| *s = Some(state));
        context.register_event(
            LOGIN_HANDLER,
            EventType::PlayerLoginEvent,
            EventPriority::Normal,
            true,
        );
        register_command(&context, "skin", "Ely.by and Mojang skins", SKIN_COMMAND);
        register_command(&context, "skull", "Player head with a skin", SKULL_COMMAND);
        // The server drops the task when the plugin is unloaded.
        schedule_repeating_task(TICK_TASK, 1, 1);
        log(Level::Info, "skinrestorer loaded");
        Ok(())
    }

    async fn on_unload(context: Context) -> Result<(), String> {
        let server = context.get_server();
        let host = PumpkinHost(&server);
        with_state(|s| s.flush(&env(&host), true));
        HTTP.with_borrow_mut(HashMap::clear);
        SENDERS.with_borrow_mut(HashMap::clear);
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
        run_command(command_id, sender, &server, &raw).await;
        Ok(1)
    }

    async fn handle_command_suggestion(
        handler_id: u32,
        sender: CommandSender,
        server: ServerInstance,
        request: SuggestionRequest,
    ) -> CommandSuggestions {
        let values = suggestions(handler_id, &sender, &server, &request)
            .into_iter()
            .map(|value| CommandSuggestion {
                value,
                tooltip: None,
            })
            .collect();
        CommandSuggestions {
            start: request.start,
            length: request.remaining.len() as u32,
            values,
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
