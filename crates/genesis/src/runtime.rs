//! Wasmtime runtime plumbing: the engine, per-call stores, capability-scoped
//! linkers, and the epoch budget that keeps a misbehaving guest from wedging
//! the process.
//!
//! Every guest call gets a *fresh* store. That is what makes hot swapping safe:
//! no guest state survives a call, so swapping the component between calls can
//! never leave a half-migrated instance behind.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{Config as WasmConfig, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::bindings;
use crate::config::Config;
use crate::harness::Harness;
use crate::llm::StreamHandle;

/// How often the epoch counter advances. The deadline callback runs at most
/// this often, which bounds how long a runaway guest can spin undetected.
pub const EPOCH_TICK: Duration = Duration::from_millis(100);
/// Ticks granted between deadline-callback checks.
const TICKS_PER_CHECK: u64 = 1;

/// Which host capabilities a guest is allowed to link against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caps {
    /// The agent: everything.
    Agent,
    /// Gateways: session management and sys only. No LLM, no exec, no devkit.
    Gateway,
    /// Tools: sys and the sandbox. No LLM, no session log, no self-modification.
    Tool,
}

/// Wall-clock and CPU limits for a single guest call.
pub struct Budget {
    /// What this call is, for the trap message.
    pub label: String,
    pub started: Instant,
    /// Refreshed whenever a potentially-blocking host import returns, so time
    /// spent waiting on the network is not charged as guest CPU.
    pub last_yield: Instant,
    /// Wall-clock ceiling for the whole call.
    pub total: Duration,
    /// Longest the guest may run without returning to a blocking host import.
    /// Catches infinite loops long before the wall-clock budget expires.
    pub slice: Duration,
    pub cancelled: bool,
}

impl Budget {
    pub fn new(label: impl Into<String>, total: Duration, slice: Duration) -> Self {
        let now = Instant::now();
        Self {
            label: label.into(),
            started: now,
            last_yield: now,
            total,
            slice,
            cancelled: false,
        }
    }

    pub fn probe(label: impl Into<String>, total: Duration) -> Self {
        Self::new(label, total, total)
    }

    /// Records that the guest just came back from a blocking host call.
    pub fn yielded(&mut self) {
        self.last_yield = Instant::now();
    }

    fn violation(&self) -> Option<String> {
        if self.cancelled {
            return Some(format!("{}: cancelled by orchestrator", self.label));
        }
        let elapsed = self.started.elapsed();
        if elapsed > self.total {
            return Some(format!(
                "{}: exceeded wall-clock budget of {:?} (ran {:?})",
                self.label, self.total, elapsed
            ));
        }
        let spinning = self.last_yield.elapsed();
        if spinning > self.slice {
            return Some(format!(
                "{}: ran {:?} without yielding to a host call (limit {:?}) — likely an infinite loop",
                self.label, spinning, self.slice
            ));
        }
        None
    }
}

pub struct HostState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    /// The crate's no-op hook set; we do not customise wasi:http behaviour.
    http_hooks: [(); 0],
    table: ResourceTable,
    limits: StoreLimits,
    pub harness: Arc<Harness>,
    pub budget: Budget,
    /// Session this call acts on behalf of. Imports use it to scope access so a
    /// guest cannot reach into a session it was not invoked for.
    pub session_id: Option<String>,
    pub streams: HashMap<u64, StreamHandle>,
    pub next_stream_id: u64,
    /// Slots the guest asked to swap at the end of this call (self-modification
    /// never yanks a running instance).
    pub pending_swaps: Vec<crate::slot::Slot>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl HostState {
    /// Marks the end of a blocking host call so waiting is not charged as spin.
    pub fn yielded(&mut self) {
        self.budget.yielded();
    }
}

pub struct Runtime {
    pub engine: Engine,
    pub agent_linker: Linker<HostState>,
    pub gateway_linker: Linker<HostState>,
    pub tool_linker: Linker<HostState>,
    cfg: Arc<Config>,
}

impl Runtime {
    pub fn new(cfg: Arc<Config>) -> Result<Arc<Self>> {
        let mut wasm_cfg = WasmConfig::new();
        wasm_cfg.epoch_interruption(true);
        wasm_cfg.wasm_component_model(true);
        let engine = Engine::new(&wasm_cfg)
            .map_err(anyhow::Error::from)
            .context("creating wasmtime engine")?;

        // A single ticker drives deadlines for every store in the process.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("genesis-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticker.increment_epoch();
            })
            .context("spawning epoch ticker")?;

        let agent_linker = build_linker(&engine, Caps::Agent)?;
        let gateway_linker = build_linker(&engine, Caps::Gateway)?;
        let tool_linker = build_linker(&engine, Caps::Tool)?;

        Ok(Arc::new(Self {
            engine,
            agent_linker,
            gateway_linker,
            tool_linker,
            cfg,
        }))
    }

    pub fn linker(&self, caps: Caps) -> &Linker<HostState> {
        match caps {
            Caps::Agent => &self.agent_linker,
            Caps::Gateway => &self.gateway_linker,
            Caps::Tool => &self.tool_linker,
        }
    }

    /// Builds a fresh store for one guest call.
    pub fn new_store(
        &self,
        harness: Arc<Harness>,
        caps: Caps,
        budget: Budget,
        session_id: Option<String>,
    ) -> Store<HostState> {
        let memory_cap = match caps {
            Caps::Agent => self.cfg.agent_memory_bytes,
            Caps::Gateway => self.cfg.gateway_memory_bytes,
            Caps::Tool => self.cfg.tool_memory_bytes,
        };

        let state = HostState {
            wasi: self.wasi_ctx(),
            http: WasiHttpCtx::new(),
            http_hooks: [],
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_cap)
                .instances(8)
                .tables(64)
                .build(),
            harness,
            budget,
            session_id,
            streams: HashMap::new(),
            next_stream_id: 1,
            pending_swaps: Vec::new(),
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(TICKS_PER_CHECK);
        store.epoch_deadline_callback(|ctx| match ctx.data().budget.violation() {
            Some(reason) => Err(wasmtime::Error::msg(reason)),
            None => Ok(UpdateDeadline::Continue(TICKS_PER_CHECK)),
        });
        store
    }
}

impl Runtime {
    /// The WASI capabilities a guest is handed, per configuration.
    ///
    /// Anything not granted here is not merely restricted, it is absent: WASI
    /// preview 2 gives a guest nothing by default, so an ungranted capability
    /// shows up as a runtime error rather than a link failure.
    fn wasi_ctx(&self) -> WasiCtx {
        let mut builder = WasiCtxBuilder::new();
        let wasi = &self.cfg.wasi;

        if wasi.network {
            builder.inherit_network();
        }
        builder.allow_ip_name_lookup(wasi.dns);
        if wasi.env {
            builder.inherit_env();
        }
        if wasi.stdio {
            builder.inherit_stdio();
        }

        for dir in &wasi.dirs {
            // A preopen has to exist before it can be handed over, and a
            // missing one would otherwise fail every single call.
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping wasi preopen");
                continue;
            }
            let guest_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "workspace".to_string());

            if let Err(e) =
                builder.preopened_dir(dir, &guest_name, DirPerms::all(), FilePerms::all())
            {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping wasi preopen");
            }
        }

        builder.build()
    }
}

/// A plain fn rather than a closure: the linker needs a higher-ranked
/// signature that closure inference will not produce on its own.
fn host_state(state: &mut HostState) -> &mut HostState {
    state
}

fn build_linker(engine: &Engine, caps: Caps) -> Result<Linker<HostState>> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking wasi")?;

    // `wasi:http` is what makes a web-facing tool possible at all. TLS is
    // terminated here rather than in the guest, because no TLS crate builds for
    // wasm32-wasip2: ring and openssl both need a C toolchain targeting wasm.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking wasi:http")?;

    bindings::sys::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;

    match caps {
        Caps::Agent => {
            bindings::session::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::llm::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::sandbox::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::tooling::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::devkit::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // Host access: the filesystem, shells, and the process itself.
            // Only the agent gets these; tools stay on the sandbox.
            bindings::hostfs::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::terminal::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::control::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::configuration::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
        Caps::Gateway => {
            bindings::session::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
        Caps::Tool => {
            bindings::sandbox::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
    }
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_work_within_limits() {
        let b = Budget::new("turn", Duration::from_secs(60), Duration::from_secs(10));
        assert!(b.violation().is_none());
    }

    #[test]
    fn budget_trips_on_wall_clock() {
        let mut b = Budget::new("turn", Duration::from_millis(1), Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(5));
        b.yielded(); // even after yielding, the total budget still applies
        let v = b.violation().expect("should trip");
        assert!(v.contains("wall-clock"), "{v}");
    }

    #[test]
    fn budget_trips_on_spin_without_yielding() {
        let b = Budget::new("turn", Duration::from_secs(60), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let v = b.violation().expect("should trip");
        assert!(v.contains("infinite loop"), "{v}");
    }

    #[test]
    fn yielding_resets_the_spin_timer() {
        let mut b = Budget::new("turn", Duration::from_secs(60), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(15));
        b.yielded();
        std::thread::sleep(Duration::from_millis(15));
        // 30ms elapsed in total but only 15ms since the last host call.
        assert!(b.violation().is_none());
    }

    #[test]
    fn cancellation_is_a_violation() {
        let mut b = Budget::new("turn", Duration::from_secs(60), Duration::from_secs(60));
        b.cancelled = true;
        assert!(b.violation().unwrap().contains("cancelled"));
    }
}
