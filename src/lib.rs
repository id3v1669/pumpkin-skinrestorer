//! Ely.by and Mojang skins for the Pumpkin server. The pure modules are tested natively;
//! `plugin` and `http` are the wasm-only glue.

pub mod command;
pub mod config;
pub mod lookup;
pub mod resolve;
pub mod skin;
pub mod source;
pub mod store;

#[cfg(target_family = "wasm")]
mod http;
#[cfg(target_family = "wasm")]
mod plugin;
