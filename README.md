# pumpkin-skinrestorer

Ely.by and Mojang skins for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) Minecraft
server, written in Rust as a WebAssembly plugin against plugin **ABI v0.2**. A simplified
take on SkinsRestorer.

## Behaviour

- At login a player gets, in this order:
  1. Steve, if an admin locked them;
  2. the skin they chose with `/skin set`, unless it was blocked since or its account
     no longer has a skin (then the choice is reset to automatic). While a chosen skin
     is not in the cache yet, the player gets what steps 3-5 give;
  3. the skin their profile already has (online mode, proxy);
  4. the skin of an account with their own name, Ely.by first, then Mojang (order set in
     the config);
  5. otherwise Steve.
- Logins never wait for the network. Skins come from the plugin's cache. A skin that is
  missing or older than `cache_minutes` is fetched in the background and shows at the
  next join. If the fetched skin differs from the one the player got at login and is
  still the one they get at the next join, they are told "Your skin is ready, rejoin to
  see it."
- Automatic skins follow the source order: a cached skin from a lower-ranked source is
  used, but the sources ranked above it are still looked up, and win once they answer.
- A source that answered "no such skin" for a name is not asked again for
  `cache_minutes`, so a player without any skin does not cause requests at every join.
  `/skin set`, `/skin update` and `/skull` always ask.
- A changed skin always shows after the player rejoins. The plugin never kicks.
- Only skins a vanilla client shows are used: signed textures hosted on
  `textures.minecraft.net`. Ely.by skins come from `skinsystem.ely.by/textures/signed`,
  asked by name; Mojang names are resolved in batches of up to ten.

## Commands

| Command | Who |
|---|---|
| `/skin set [ely:\|mojang:]<name>` | everyone |
| `/skin clear`, `update`, `status`, `history`, `undo` | everyone |
| `/skin set\|clear\|update\|status <player>` | admin |
| `/skin lock\|unlock <player>` | admin |
| `/skin block\|unblock <source:name>` | admin |
| `/skin reload` | admin |
| `/skull [ely:\|mojang:]<name>` | everyone |
| `/skull <name> <player>` | admin |

Subcommands and player names are case-insensitive. A name without an `ely:` / `mojang:`
prefix is tried on the sources in the configured order. Tab completion offers the
subcommands, the source prefixes and, to admins, the online players.

Admins are players with operator level `admin_op_level` or higher, players whose UUID is
listed in `admins`, and the console. Other senders (RCON, command blocks) are admins by
their operator level. `set`, `update` and `/skull` share a cooldown of 30 s after success
and 5 s after an error, and only one of these lookups runs per target player at a time.
`clear` and `undo` start the 5 s cooldown when they change something. Admins are exempt
from cooldowns.

`/skin status` shows the choice and what the next join will apply. `/skin update` only
says "It applies on the next join" when that is so: a profile skin or a higher-ranked
source can still win.

`/skin undo` steps back through the player's own earlier choices, skipping blocked ones;
admin changes don't add to the history; the choice they set is recorded when the player
next changes their skin.

## Configuration

`plugins/data/skinrestorer/config.toml` is created on first start with these defaults:

```toml
source_order = ["ely", "mojang"]
cache_minutes = 15
admin_op_level = 4
admins = []          # player UUIDs
history_length = 36
blocked = []         # "source:name", edited by /skin block
```

Unknown keys are rejected. An invalid file stops the plugin from loading; on
`/skin reload` it is rejected and the previous config stays. If the file is missing at
`/skin reload` or `/skin block`/`unblock`, it is recreated with the defaults and the reply
says so. `/skin block` and `unblock` rewrite only the `blocked` list and keep the comments
in it; they also re-read the rest of the file, like `/skin reload`.

Player choices are kept in `players.json`, fetched skins in `skins.json` (same folder).
A corrupt `players.json` also stops the plugin from loading, so it is never overwritten.
`skins.json` is only a cache: if it cannot be read, an error is logged and the plugin
starts with an empty one. Cache entries not refreshed for 30 days (or `cache_minutes`, if
that is longer) are dropped. Changes to `players.json` are written within a tick,
`skins.json` at most every 5 s and when the plugin unloads.

## Requirements

ABI v0.2 is still behind a feature flag in Pumpkin: the server must be built with

```sh
cargo build --release --features plugin-v0_2
```

The plugin declares the `http.outbound`, `fs.read.data` and `fs.write.data` permissions;
the server must not block them. It needs outbound HTTPS to `skinsystem.ely.by`,
`api.minecraftservices.com` and `sessionserver.mojang.com`.

`wit/` is a copy of `crates/pumpkin-plugin-wit/v0.2` from the Pumpkin commit named in
`wit/PUMPKIN_COMMIT` (MIT / Apache-2.0, licences included); `just wit-check` compares the
two. The Steve
skin in `resources/steve.json` comes from SkinsRestorer's `hardcoded_skins.json`
(GPL-3.0).

## Privacy and offline mode

- The name of every joining player without a profile skin is sent to the sources in
  `source_order` (by default Ely.by, then Mojang), and so is every name given to
  `/skin set` and `/skull`. Remove a source from `source_order` to stop using it.
- In offline mode a player is whoever uses the name: records, locks and choices are kept
  by name, and UUIDs are derived from names, so an entry in `admins` gives admin rights
  to anyone joining with that name. Use `admins` only in online mode or behind a proxy
  that forwards real UUIDs.

## Build

```sh
nix build            # -> result/lib/skinrestorer.wasm
nix flake check      # unit tests, rustfmt, clippy
```

or in the dev shell (`nix develop`):

```sh
just build
just test
just lint
```

Copy the `.wasm` into the server's `plugins/` directory.

## Known limits

- Never run on a live server yet: loading, HTTP access, head rendering, tab completion,
  delayed replies to RCON and command blocks, and the Ely.by answers are untested.
- It is not documented what Mojang's bulk name lookup does with a name it rejects. If it
  answers HTTP 400, the names of that batch are asked again one by one; a name refused on
  its own counts as a failed lookup, not as "no such skin".
- Bedrock players: Pumpkin computes their skin before the login event, so a skin set by
  this plugin may not reach Bedrock viewers.
- Lookups, cooldowns and revision counters live in memory; a restart drops running
  lookups.
- Admin commands only target online players.
- Failed background lookups are logged as warnings (at most one per minute).
