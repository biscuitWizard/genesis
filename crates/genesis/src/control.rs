//! Control over the orchestrator process itself.
//!
//! Restarting is how a change to the native binary — or to configuration read
//! only at startup — takes effect. Guest code cannot be trusted to do this
//! sensibly on its own, so it is rate limited by uptime and can be turned off.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::harness::Harness;

/// When this process started, for the minimum-uptime guard.
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn mark_start() {
    let _ = STARTED.set(Instant::now());
}

pub fn uptime() -> Duration {
    STARTED.get().map(|t| t.elapsed()).unwrap_or_default()
}

/// Schedules a restart and returns immediately.
///
/// The delay matters: the call has to return so the turn can finish and the
/// user can read why the process is about to go away. Restarting inside the
/// call would kill the turn mid-sentence and leave no explanation.
pub fn request_restart(
    harness: &Arc<Harness>,
    reason: &str,
    resume: bool,
    session_id: Option<&str>,
) -> Result<String> {
    let cfg = &harness.cfg;

    if !cfg.control.allow_restart {
        return Err(anyhow!(
            "restarting is off; set control.allow_restart in genesis.toml to turn it on"
        ));
    }

    let up = uptime();
    if up < cfg.control.min_uptime {
        return Err(anyhow!(
            "this process has only been up {:.0}s; restarts are refused before {:.0}s so a \
             failing restart cannot become a loop",
            up.as_secs_f64(),
            cfg.control.min_uptime.as_secs_f64()
        ));
    }

    // Recorded before the process goes away: on the way back up, startup
    // reconciliation reads this to decide whether to carry the turn on.
    if let Some(session) = session_id {
        if let Err(e) = harness.db.set_no_resume(session, !resume) {
            tracing::warn!(error = %e, "could not record the resume preference");
        }
    }

    let reason = reason.trim().to_string();
    let delay = cfg.control.restart_delay;
    tracing::warn!(reason = %reason, "restart requested; the process will replace itself shortly");

    let harness = harness.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        // Shells outlive the process otherwise, and would hold their pipes open.
        harness.terminals.close_all().await;

        if let Err(e) = respawn() {
            // Staying up is better than exiting into nothing.
            tracing::error!(error = %e, "restart failed; continuing to run");
        }
    });

    Ok(format!(
        "restarting in {:.1}s: {reason}. {}",
        delay.as_secs_f64(),
        if resume {
            "This turn will carry on once Genesis is back, so there is no need to repeat yourself."
        } else {
            "This turn ends here."
        }
    ))
}

/// Starts a replacement process and exits this one.
///
/// The replacement is spawned first so a failure to start leaves the current
/// process running rather than taking the system down.
fn respawn() -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    tracing::info!(exe = %exe.display(), "spawning replacement process");
    std::process::Command::new(&exe)
        .args(&args)
        .current_dir(std::env::current_dir()?)
        .spawn()
        .map_err(|e| anyhow!("cannot start {}: {e}", exe.display()))?;

    // The replacement retries binding, so it is fine that this process still
    // holds the port for a moment.
    tracing::info!("replacement started; exiting");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_zero_until_marked() {
        // Nothing has called mark_start in this test binary, so the guard reads
        // as a brand new process — which is the conservative direction.
        assert!(uptime() < Duration::from_secs(1));
    }
}
