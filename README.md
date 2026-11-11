# pumpkin-skinrestorer

Ely.by and Mojang skins for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) Minecraft
server, written in Rust as a WebAssembly plugin against plugin **ABI v0.2**. A simplified
take on SkinsRestorer.

## Behaviour

- At login a player gets, in this order:
  1. Steve, if an admin locked them;
  2. the skin they chose with `/skin set`, unless it was blocked since (then the choice
     is reset to automatic);
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
- A changed skin always shows after the player rejoins. The plugin never kicks.
- Only skins a vanilla client shows are used: signed textures hosted on
  `textures.minecraft.net`. Ely.by skins come from `skinsystem.ely.by/textures/signed`.

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

Subcommands are case-insensitive. A name without an `ely:` / `mojang:` prefix is tried on
the sources in the configured order.

Admins are players with operator level `admin_op_level` or higher, players whose UUID is
listed in `admins`, and the console. `set`, `update` and `/skull` share a cooldown of 30 s
after success and 5 s after an error (admins are exempt), and only one of these lookups
runs per target player at a time.

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

Player choices are kept in `players.json`, fetched skins in `skins.json` (same folder). A
corrupt state file also stops the plugin from loading, so it is never overwritten.

## Requirements

ABI v0.2 is still behind a feature flag in Pumpkin: the server must be built with

```sh
cargo build --release --features plugin-v0_2
```

The plugin declares the `http.outbound`, `fs.read.data` and `fs.write.data` permissions;
the server must not block them. It needs outbound HTTPS to `authserver.ely.by`,
`skinsystem.ely.by`, `api.minecraftservices.com` and `sessionserver.mojang.com`.

`wit/` is a copy of `crates/pumpkin-plugin-wit/v0.2` from Pumpkin commit
`10f1fde9367c827f168eaabdc8f343b1ce6f676c` (MIT / Apache-2.0, licences included). The Steve
skin in `resources/steve.json` comes from SkinsRestorer's `hardcoded_skins.json`
(GPL-3.0).

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

- Never run on a live server yet: loading, HTTP access, head rendering and the Ely.by
  answers are untested.
- Bedrock players: Pumpkin computes their skin before the login event, so a skin set by
  this plugin may not reach Bedrock viewers.
- Lookups, cooldowns and revision counters live in memory; a restart drops running
  lookups.
- Admin commands only target online players.
- Failed background lookups are logged as warnings (at most one per minute).
