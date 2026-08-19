//! Long-lived shell sessions on the host.
//!
//! A session keeps its working directory, environment and shell state between
//! commands, which is what separates it from a one-shot exec.
//!
//! Knowing when a command has finished is the hard part of driving a shell over
//! pipes: the stream never ends, so there is nothing to wait for. Each command
//! is therefore followed by an echo of a unique marker, and `run` reads until
//! that marker appears. The marker is what turns an endless stream back into
//! request and response.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::bindings::types::{TerminalInfo, TerminalOutput};
use crate::config::Config;

struct Session {
    id: String,
    cwd: String,
    shell: String,
    child: Child,
    stdin: ChildStdin,
    /// Everything the shell has written that `read` has not yet returned.
    buffer: Arc<Mutex<String>>,
    commands: u32,
    last_used: Instant,
}

#[derive(Default)]
pub struct Terminals {
    sessions: tokio::sync::Mutex<HashMap<String, Session>>,
    counter: std::sync::atomic::AtomicU64,
}

impl Terminals {
    pub fn new() -> Self {
        Self::default()
    }

    fn require_enabled(cfg: &Config) -> Result<()> {
        if cfg.terminal.enabled {
            Ok(())
        } else {
            Err(anyhow!(
                "terminal access is off; set terminal.enabled in genesis.toml to turn it on"
            ))
        }
    }

    // --- lifecycle ---------------------------------------------------------

    pub async fn open(&self, cfg: &Config, cwd: Option<&str>) -> Result<String> {
        Self::require_enabled(cfg)?;

        let mut sessions = self.sessions.lock().await;
        self.reap(&mut sessions, cfg);

        if sessions.len() >= cfg.terminal.max_sessions {
            return Err(anyhow!(
                "already at the limit of {} terminal sessions; close one first",
                cfg.terminal.max_sessions
            ));
        }

        // The working directory goes through the same confinement as the
        // filesystem tools, so a session cannot start outside the roots.
        let dir = match cwd {
            Some(raw) => crate::hostfs::resolve(cfg, raw)?,
            None => cfg
                .filesystem
                .roots
                .first()
                .cloned()
                .unwrap_or_else(|| cfg.root.clone()),
        };
        if !dir.is_dir() {
            return Err(anyhow!("{} is not a directory", dir.display()));
        }

        let mut child = Command::new(&cfg.terminal.shell)
            .args(&cfg.terminal.shell_args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("cannot start {}: {e}", cfg.terminal.shell))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let buffer = Arc::new(Mutex::new(String::new()));

        // Both streams feed one buffer, so interleaved output reads the way it
        // would in a real terminal.
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, buffer.clone(), cfg.terminal.max_output_bytes);
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, buffer.clone(), cfg.terminal.max_output_bytes);
        }

        let id = format!(
            "term-{}",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1
        );

        sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                cwd: dir.display().to_string(),
                shell: cfg.terminal.shell.clone(),
                child,
                stdin,
                buffer,
                commands: 0,
                last_used: Instant::now(),
            },
        );

        tracing::info!(terminal = %id, dir = %dir.display(), "terminal session opened");
        Ok(id)
    }

    pub async fn close(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let Some(mut session) = sessions.remove(id) else {
            return Err(anyhow!("no terminal session {id}"));
        };
        let _ = session.child.kill().await;
        tracing::info!(terminal = %id, "terminal session closed");
        Ok(format!("closed {id}"))
    }

    pub async fn list(&self) -> Vec<TerminalInfo> {
        let mut sessions = self.sessions.lock().await;
        let mut out: Vec<TerminalInfo> = sessions
            .values_mut()
            .map(|s| TerminalInfo {
                id: s.id.clone(),
                cwd: s.cwd.clone(),
                shell: s.shell.clone(),
                // `try_wait` reports without blocking; `Some` means it exited.
                alive: matches!(s.child.try_wait(), Ok(None)),
                commands: s.commands,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Returns and clears whatever the shell has written since the last read.
    pub async fn read(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("no terminal session {id}"))?;
        session.last_used = Instant::now();
        Ok(take(&session.buffer))
    }

    // --- running -----------------------------------------------------------

    pub async fn run(
        &self,
        cfg: &Config,
        id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<TerminalOutput> {
        Self::require_enabled(cfg)?;

        let marker = format!(
            "__genesis_done_{}__",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        let buffer = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| anyhow!("no terminal session {id}"))?;

            if matches!(session.child.try_wait(), Ok(Some(_))) {
                return Err(anyhow!("terminal session {id} has exited; open a new one"));
            }

            // Anything left from a previous command would otherwise be reported
            // as this command's output.
            take(&session.buffer);

            let script = format!("{command}\n{}\n", echo_marker(cfg, &marker));
            session
                .stdin
                .write_all(script.as_bytes())
                .await
                .map_err(|e| anyhow!("cannot write to {id}: {e}"))?;
            session
                .stdin
                .flush()
                .await
                .map_err(|e| anyhow!("cannot flush {id}: {e}"))?;

            session.commands += 1;
            session.last_used = Instant::now();
            session.buffer.clone()
        };

        // Poll rather than hold the session lock, so other calls are not blocked
        // by a long-running command.
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(output) = split_at_marker(&buffer, &marker) {
                let truncated = output.len() > cfg.terminal.max_output_bytes;
                return Ok(TerminalOutput {
                    output: clip(output, cfg.terminal.max_output_bytes),
                    timed_out: false,
                    truncated,
                });
            }
            if Instant::now() >= deadline {
                // Whatever arrived so far is still worth returning: a command
                // that is merely slow has usually printed something useful.
                let partial = take(&buffer);
                let truncated = partial.len() > cfg.terminal.max_output_bytes;
                return Ok(TerminalOutput {
                    output: clip(partial, cfg.terminal.max_output_bytes),
                    timed_out: true,
                    truncated,
                });
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    /// Drops sessions whose shell has exited or that have gone idle.
    fn reap(&self, sessions: &mut HashMap<String, Session>, cfg: &Config) {
        let idle = cfg.terminal.idle_timeout;
        sessions.retain(|id, session| {
            let exited = matches!(session.child.try_wait(), Ok(Some(_)));
            let stale = idle > Duration::ZERO && session.last_used.elapsed() > idle;
            if exited || stale {
                tracing::debug!(terminal = %id, exited, stale, "reaping terminal session");
                return false;
            }
            true
        });
    }

    /// Kills every session, for shutdown.
    pub async fn close_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for (_, mut session) in sessions.drain() {
            let _ = session.child.kill().await;
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// Echoes the marker in a way the configured shell understands.
fn echo_marker(cfg: &Config, marker: &str) -> String {
    if cfg.terminal.shell.to_lowercase().contains("powershell")
        || cfg.terminal.shell.to_lowercase().contains("pwsh")
    {
        // Write-Output rather than echo: it is not aliased away by a profile.
        format!("Write-Output '{marker}'")
    } else {
        format!("echo '{marker}'")
    }
}

/// Reads lines from a stream into the shared buffer.
fn pump<R>(stream: R, buffer: Arc<Mutex<String>>, cap: usize)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(mut buf) = buffer.lock() else { return };
            buf.push_str(&line);
            buf.push('\n');
            // Keep the tail: a runaway command must not grow this without bound.
            if buf.len() > cap * 4 {
                let keep = buf.len() - cap * 2;
                *buf = buf.split_off(keep);
            }
        }
    });
}

fn take(buffer: &Arc<Mutex<String>>) -> String {
    match buffer.lock() {
        Ok(mut buf) => std::mem::take(&mut *buf),
        Err(_) => String::new(),
    }
}

/// Returns the output before the marker once it has arrived, consuming it.
fn split_at_marker(buffer: &Arc<Mutex<String>>, marker: &str) -> Option<String> {
    let mut buf = buffer.lock().ok()?;

    // The command that echoes the marker is itself echoed by some shells, so
    // match the last occurrence: that one is the real completion.
    let at = buf.rfind(marker)?;
    let before = buf[..at].to_string();
    let after = buf[at + marker.len()..].to_string();
    *buf = after.trim_start_matches('\n').to_string();

    // Drop the echoed command line that produced the marker, if present.
    let cleaned: Vec<&str> = before
        .lines()
        .filter(|line| !line.contains(marker))
        .collect();
    Some(cleaned.join("\n").trim_end().to_string())
}

fn clip(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    // Keep the tail: the end of a command's output is usually the part that
    // says what happened.
    let mut cut = text.len() - cap;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[earlier output trimmed]\n{}", &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_of(text: &str) -> Arc<Mutex<String>> {
        Arc::new(Mutex::new(text.to_string()))
    }

    #[test]
    fn marker_absent_means_the_command_is_still_running() {
        let buf = buffer_of("building...\n");
        assert!(split_at_marker(&buf, "__genesis_done_1__").is_none());
        // Nothing is consumed while waiting.
        assert_eq!(buf.lock().unwrap().as_str(), "building...\n");
    }

    #[test]
    fn output_before_the_marker_is_returned_and_consumed() {
        let buf = buffer_of("line one\nline two\n__genesis_done_1__\nleftover\n");
        let output = split_at_marker(&buf, "__genesis_done_1__").unwrap();
        assert_eq!(output, "line one\nline two");
        // What arrived after belongs to whatever comes next.
        assert_eq!(buf.lock().unwrap().as_str(), "leftover\n");
    }

    #[test]
    fn an_echoed_command_line_does_not_end_the_command_early() {
        // Some shells echo the line that will print the marker; the real
        // completion is the last occurrence, not the first.
        let buf = buffer_of("Write-Output '__genesis_done_2__'\nreal output\n__genesis_done_2__\n");
        let output = split_at_marker(&buf, "__genesis_done_2__").unwrap();
        assert_eq!(output, "real output");
    }

    #[test]
    fn output_is_clipped_from_the_front_keeping_the_end() {
        let text = format!("{}IMPORTANT TAIL", "x".repeat(500));
        let clipped = clip(text, 50);
        assert!(clipped.ends_with("IMPORTANT TAIL"), "{clipped}");
        assert!(clipped.starts_with("[earlier output trimmed]"));
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(clip("done".to_string(), 100), "done");
    }

    #[test]
    fn the_marker_echo_matches_the_shell() {
        let mut cfg = Config::load().unwrap();
        cfg.terminal.shell = "powershell".into();
        assert!(echo_marker(&cfg, "M").starts_with("Write-Output"));
        cfg.terminal.shell = "sh".into();
        assert!(echo_marker(&cfg, "M").starts_with("echo"));
    }
}
