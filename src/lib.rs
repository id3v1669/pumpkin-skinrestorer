//! Ely.by and Mojang skins for the Pumpkin server. Everything but `plugin` and `http`,
//! the wasm-only bindings, is tested natively; `app` holds the plugin's behaviour.

pub mod app;
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
