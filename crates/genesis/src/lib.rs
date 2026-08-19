//! Genesis: the trusted kernel of the harness.
//!
//! It owns every capability the system has — the network, the filesystem, the
//! database, the build toolchain — and hands guests narrow, mediated slices of
//! them through the WIT contract in `wit/genesis.wit`. Guests (the agent, the
//! gateways, the tools) are hot-swappable WebAssembly components that can be
//! rebuilt, validated, and rolled back while the system keeps running.

pub mod bindings;
pub mod builder;
pub mod cache;
pub mod config;
pub mod control;
pub mod devkit;
pub mod gateway;
pub mod harness;
pub mod hostfs;
pub mod host_api;
pub mod llm;
pub mod loader;
pub mod pipeline;
pub mod revisions;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod skills;
pub mod slot;
pub mod store;
pub mod terminal;
pub mod watchdog;
pub mod watcher;
pub mod web;
