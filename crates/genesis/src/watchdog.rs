//! Liveness probes and circuit breakers.
//!
//! The epoch budget in `runtime` stops a single call from running away. This is
//! the layer above: it notices when a *revision* is consistently failing and
//! takes it out of service automatically, so a bad self-modification degrades
//! into a rollback rather than an agent nobody can reach.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::harness::Harness;
use crate::pipeline;
use crate::slot::Slot;

pub struct Breakers {
    /// slot key -> failure timestamps, newest last
    failures: Mutex<HashMap<String, Vec<Instant>>>,
    window: Duration,
    threshold: usize,
}

impl Breakers {
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            failures: Mutex::new(HashMap::new()),
            window,
            threshold,
        }
    }

    /// Records a failure and reports whether the breaker has tripped.
    pub fn record_failure(&self, slot: &Slot) -> bool {
        let Ok(mut map) = self.failures.lock() else {
            return false;
        };
        let entry = map.entry(slot.key()).or_default();
        let now = Instant::now();
        entry.retain(|t| now.duration_since(*t) < self.window);
        entry.push(now);
        entry.len() >= self.threshold
    }

    /// Called after a healthy result, and after a rollback, so a recovered slot
    /// starts from a clean slate.
    pub fn clear(&self, slot: &Slot) {
        if let Ok(mut map) = self.failures.lock() {
            map.remove(&slot.key());
        }
    }

    pub fn failure_count(&self, slot: &Slot) -> usize {
        self.failures
            .lock()
            .ok()
            .and_then(|m| m.get(&slot.key()).map(Vec::len))
            .unwrap_or(0)
    }
}

/// Reports a failed guest call to the breaker, rolling the slot back if it has
/// failed too often. Returns a message when a rollback happened.
pub async fn report_failure(harness: &Arc<Harness>, slot: &Slot, detail: &str) -> Option<String> {
    if !harness.breakers.record_failure(slot) {
        tracing::debug!(%slot, detail, "guest call failed");
        return None;
    }

    tracing::warn!(
        %slot,
        failures = harness.breakers.failure_count(slot),
        "circuit breaker tripped; rolling back"
    );

    match pipeline::rollback_slot(harness, slot, None).await {
        Ok(message) => {
            harness.breakers.clear(slot);
            let text = format!("{slot} kept failing ({detail}); {message}");
            tracing::warn!("{text}");
            Some(text)
        }
        Err(e) => {
            // Nothing to fall back to. Say so loudly rather than pretending.
            let text = format!(
                "{slot} kept failing ({detail}) and could not be rolled back: {e:#}"
            );
            tracing::error!("{text}");
            Some(text)
        }
    }
}

/// Periodically probes the active agent so a version that only fails at runtime
/// is discovered before a user runs into it.
pub fn spawn_prober(harness: Arc<Harness>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(harness.cfg.watchdog.probe_interval);
        ticker.tick().await; // the first tick fires immediately; skip it

        loop {
            ticker.tick().await;
            if let Err(e) = probe_agent(&harness).await {
                report_failure(&harness, &Slot::Agent, &format!("health probe: {e:#}")).await;
            } else {
                harness.breakers.clear(&Slot::Agent);
            }
        }
    });
}

async fn probe_agent(harness: &Arc<Harness>) -> anyhow::Result<()> {
    use crate::runtime::{Budget, Caps};

    let Some(loaded) = harness.loader.get(&Slot::Agent) else {
        return Ok(()); // nothing loaded yet; not a failure
    };

    let budget = Budget::probe("agent health probe", harness.cfg.probe_budget);
    let mut store = harness
        .runtime
        .new_store(harness.clone(), Caps::Agent, budget, None);

    let agent = crate::bindings::agent::Agent::instantiate_async(
        &mut store,
        &loaded.component,
        harness.runtime.linker(Caps::Agent),
    )
    .await
    .map_err(anyhow::Error::from)?;

    let health = agent
        .call_health(&mut store)
        .await
        .map_err(anyhow::Error::from)?;

    if health.trim().is_empty() {
        anyhow::bail!("health returned nothing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breakers() -> Breakers {
        Breakers::new(Duration::from_secs(120), 3)
    }

    #[test]
    fn breaker_trips_only_after_repeated_failures() {
        let b = breakers();
        let slot = Slot::Agent;

        assert!(!b.record_failure(&slot), "one failure is not a pattern");
        assert!(!b.record_failure(&slot));
        assert!(b.record_failure(&slot), "third failure trips it");
    }

    #[test]
    fn breakers_are_per_slot() {
        let b = breakers();
        b.record_failure(&Slot::Agent);
        b.record_failure(&Slot::Agent);

        // A different slot must not inherit the agent's failures.
        assert!(!b.record_failure(&Slot::gateway("web")));
        assert_eq!(b.failure_count(&Slot::Agent), 2);
        assert_eq!(b.failure_count(&Slot::gateway("web")), 1);
    }

    #[test]
    fn recovery_clears_the_breaker() {
        let b = breakers();
        let slot = Slot::tool("flaky");

        b.record_failure(&slot);
        b.record_failure(&slot);
        b.clear(&slot);

        assert_eq!(b.failure_count(&slot), 0);
        assert!(!b.record_failure(&slot), "counting restarts after recovery");
    }
}
