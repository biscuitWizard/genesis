//! Generated bindings for the three guest worlds.
//!
//! The `agent` world imports every host interface, so its generated modules are
//! the canonical definitions; the `gateway` and `tool` worlds reuse them via
//! `with:` so there is exactly one `Host` trait per interface to implement and
//! one Rust type per WIT type.

#![allow(clippy::all)]

pub mod agent {
    wasmtime::component::bindgen!({
        world: "agent",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

pub mod gateway {
    wasmtime::component::bindgen!({
        world: "gateway",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
        with: {
            "genesis:harness/types": crate::bindings::agent::genesis::harness::types,
            "genesis:harness/sys": crate::bindings::agent::genesis::harness::sys,
            "genesis:harness/session": crate::bindings::agent::genesis::harness::session,
        },
    });
}

pub mod tool {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
        with: {
            "genesis:harness/types": crate::bindings::agent::genesis::harness::types,
            "genesis:harness/sys": crate::bindings::agent::genesis::harness::sys,
            "genesis:harness/sandbox": crate::bindings::agent::genesis::harness::sandbox,
        },
    });
}

/// Canonical shared types (records, variants) used throughout the host.
#[allow(unused_imports)]
pub use agent::genesis::harness::types;

/// Host interface modules, each exposing a `Host` trait the orchestrator
/// implements and an `add_to_linker` used when building a linker.
#[allow(unused_imports)]
pub use agent::genesis::harness::{
    configuration, control, devkit, hostfs, llm, sandbox, session, sys, terminal, tooling,
};
