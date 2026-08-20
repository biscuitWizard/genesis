//! Calling into the gateway component.
//!
//! The host owns the socket; the gateway owns every byte the user sees. Asset
//! requests and client messages are infrequent, so each gets a fresh store.
//! Event rendering is not — a streaming reply renders one frame per token — so
//! the renderer keeps a warm instance and recycles it on swap, trap, or age.

use anyhow::{Context, Result};
use std::sync::Arc;
use wasmtime::Store;

use crate::bindings::gateway::{Gateway, GatewayAction};
use crate::bindings::types::{Asset, OutboundEvent};
use crate::harness::Harness;
use crate::runtime::{Budget, Caps, HostState};
use crate::slot::Slot;

/// Rebuild the warm renderer instance after this many calls, so a long-lived
/// guest cannot accumulate unbounded state.
const RENDERER_MAX_CALLS: u64 = 5_000;

/// The gateway serving the browser UI, named in configuration.
fn gateway_slot(harness: &Arc<Harness>) -> Slot {
    Slot::gateway(&harness.cfg.primary_gateway)
}

async fn fresh_instance(
    harness: &Arc<Harness>,
    label: &str,
) -> Result<(Store<HostState>, Gateway, u64)> {
    let loaded = harness
        .loader
        .get(&gateway_slot(harness))
        .context("no gateway component is loaded")?;

    let budget = Budget::probe(label, harness.cfg.probe_budget);
    // Gateways run unscoped: managing every session is their job.
    let mut store = harness
        .runtime
        .new_store(harness.clone(), Caps::Gateway, budget, None);

    let instance = Gateway::instantiate_async(
        &mut store,
        &loaded.component,
        harness.runtime.linker(Caps::Gateway),
    )
    .await
    .map_err(anyhow::Error::from)
    .context("instantiating gateway")?;

    Ok((store, instance, loaded.revision))
}

pub async fn serve_asset(harness: &Arc<Harness>, path: &str) -> Result<Option<Asset>> {
    let (mut store, gw, _) = fresh_instance(harness, "gateway serve-asset").await?;
    gw.call_serve_asset(&mut store, path)
        .await
        .map_err(anyhow::Error::from)
        .context("gateway serve-asset")
}

pub async fn on_client_message(
    harness: &Arc<Harness>,
    client_id: &str,
    frame_json: &str,
) -> Result<Vec<GatewayAction>> {
    let (mut store, gw, _) = fresh_instance(harness, "gateway on-client-message").await?;
    gw.call_on_client_message(&mut store, client_id, frame_json)
        .await
        .map_err(anyhow::Error::from)
        .context("gateway on-client-message")
}

/// Warm renderer used by the single fan-out task.
pub struct Renderer {
    harness: Arc<Harness>,
    warm: Option<Warm>,
}

struct Warm {
    revision: u64,
    store: Store<HostState>,
    instance: Gateway,
    calls: u64,
}

impl Renderer {
    pub fn new(harness: Arc<Harness>) -> Self {
        Self {
            harness,
            warm: None,
        }
    }

    /// True when the cached instance is stale: a swap happened, it aged out, or
    /// there is nothing cached yet.
    fn needs_refresh(&self) -> bool {
        let Some(warm) = &self.warm else {
            return true;
        };
        if warm.calls >= RENDERER_MAX_CALLS {
            return true;
        }
        match self.harness.loader.get(&gateway_slot(&self.harness)) {
            Some(current) => current.revision != warm.revision,
            None => true,
        }
    }

    pub async fn render(&mut self, event: OutboundEvent) -> Option<String> {
        if self.needs_refresh() {
            match fresh_instance(&self.harness, "gateway render-event").await {
                Ok((store, instance, revision)) => {
                    self.warm = Some(Warm {
                        revision,
                        store,
                        instance,
                        calls: 0,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cannot render events: gateway unavailable");
                    self.warm = None;
                    return None;
                }
            }
        }

        let warm = self.warm.as_mut()?;
        warm.calls += 1;
        // The store outlives a single call, so its budget must be rearmed: the
        // last-yield mark keeps ageing between events, and would eventually
        // read as a guest that had stopped talking to the host.
        warm.store.data_mut().budget = Budget::probe(
            "gateway render-event",
            self.harness.cfg.probe_budget,
        );

        match warm.instance.call_render_event(&mut warm.store, &event).await {
            Ok(frame) => frame,
            Err(trap) => {
                // A trapped store is poisoned; drop it so the next event starts clean.
                tracing::warn!(error = %trap, "gateway render-event trapped");
                self.warm = None;
                None
            }
        }
    }
}
