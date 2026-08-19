//! HTTP/WebSocket transport.
//!
//! The host owns the listener and the connection registry; the gateway
//! component owns the UI and the wire protocol. The one exception is `/admin`,
//! which is rendered here in native code with no WASM in its path — it is the
//! control surface that must keep working when every guest is broken.

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

use crate::bindings::gateway::GatewayAction;
use crate::gateway;
use crate::harness::{Harness, RenderedFrame};

pub async fn serve(harness: Arc<Harness>) -> Result<()> {
    tokio::spawn(render_loop(harness.clone()));

    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/admin", get(admin_page))
        .route("/admin/rollback", post(admin_rollback))
        .route("/", get(root_asset))
        .route("/{*path}", get(path_asset))
        .with_state(harness.clone());

    let listener = bind_with_retry(harness.cfg.bind_addr).await?;

    tracing::info!(addr = %harness.cfg.bind_addr, "genesis listening");
    axum::serve(listener, app).await.context("http server")?;
    Ok(())
}

/// Binds, retrying briefly while the address is still in use.
///
/// A restart spawns the replacement before this process exits, so the new one
/// arrives while the old still holds the port. Waiting a few seconds is the
/// difference between a seamless restart and a failed one.
async fn bind_with_retry(addr: std::net::SocketAddr) -> Result<tokio::net::TcpListener> {
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);
    let deadline = std::time::Instant::now() + PATIENCE;
    let mut reported = false;

    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
                && std::time::Instant::now() < deadline =>
            {
                if !reported {
                    tracing::info!(%addr, "address busy, waiting for the previous process to exit");
                    reported = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => return Err(anyhow::Error::from(e)).with_context(|| format!("binding {addr}")),
        }
    }
}

/// Renders every event exactly once, then fans the frame out to connections.
/// Rendering centrally keeps it to one guest call per event rather than one per
/// connected browser.
async fn render_loop(harness: Arc<Harness>) {
    let mut events = harness.events_tx.subscribe();
    let mut renderer = gateway::Renderer::new(harness.clone());

    loop {
        match events.recv().await {
            Ok(event) => {
                let session_id = event.session_id.clone();
                if let Some(frame) = renderer.render(event).await {
                    let _ = harness.frames_tx.send(RenderedFrame { session_id, frame });
                }
            }
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "renderer fell behind; some frames were dropped");
            }
            Err(RecvError::Closed) => return,
        }
    }
}

// --- assets ----------------------------------------------------------------

async fn root_asset(State(harness): State<Arc<Harness>>) -> Response {
    asset_response(&harness, "/").await
}

async fn path_asset(State(harness): State<Arc<Harness>>, Path(path): Path<String>) -> Response {
    asset_response(&harness, &format!("/{path}")).await
}

async fn asset_response(harness: &Arc<Harness>, path: &str) -> Response {
    match gateway::serve_asset(harness, path).await {
        Ok(Some(asset)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, asset.mime)],
            asset.bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        // The gateway is broken or missing: fall back to a host-rendered page
        // so the user is never staring at a dead socket.
        Err(e) => {
            let detail = format!("{e:#}");
            tracing::warn!(error = %detail, path, "gateway asset request failed");
            (StatusCode::SERVICE_UNAVAILABLE, Html(fallback_page(&detail))).into_response()
        }
    }
}

fn fallback_page(detail: &str) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>Genesis — gateway unavailable</title>
<style>body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1.5rem;color:#e6e6e6;background:#16161a}}
code{{background:#26262c;padding:.15em .4em;border-radius:4px}} a{{color:#7aa2f7}}</style>
<h1>The chat gateway is unavailable</h1>
<p>The orchestrator is running, but the gateway component could not serve this page.</p>
<pre><code>{}</code></pre>
<p>The system is still recoverable: <a href="/admin">open the admin console</a> to inspect
component revisions and roll the gateway back to a working one.</p>"#,
        html_escape(detail)
    )
}

// --- admin (host-owned; never routed through a guest) -----------------------

#[derive(serde::Deserialize)]
struct RollbackForm {
    target: String,
    revision: Option<String>,
}

/// The manual override. Deliberately a plain HTML form so it works with no
/// JavaScript, no websocket, and no guest code involved.
async fn admin_rollback(
    State(harness): State<Arc<Harness>>,
    axum::Form(form): axum::Form<RollbackForm>,
) -> Html<String> {
    let revision = form
        .revision
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .and_then(|r| r.trim().parse::<u64>().ok());

    let result = if let Some(snapshot_id) = form.target.strip_prefix("system:") {
        match snapshot_id.parse::<u64>() {
            Ok(id) => crate::pipeline::rollback_system(&harness, id).await,
            Err(_) => Err(anyhow::anyhow!("bad snapshot id")),
        }
    } else {
        match crate::slot::Slot::parse(&form.target) {
            Ok(slot) => crate::pipeline::rollback_slot(&harness, &slot, revision).await,
            Err(e) => Err(e),
        }
    };

    let banner = match result {
        Ok(message) => {
            tracing::warn!("admin rollback: {message}");
            format!(r#"<p class="banner ok">{}</p>"#, html_escape(&message))
        }
        Err(e) => format!(
            r#"<p class="banner bad">rollback failed: {}</p>"#,
            html_escape(&format!("{e:#}"))
        ),
    };

    Html(render_admin(&harness, &banner))
}

async fn admin_page(State(harness): State<Arc<Harness>>) -> Html<String> {
    Html(render_admin(&harness, ""))
}

fn render_admin(harness: &Arc<Harness>, banner: &str) -> String {
    let mut sections = String::new();

    let slots = harness.revisions.slots_with_history().unwrap_or_default();
    if slots.is_empty() {
        sections.push_str("<p class=note>No components have been built yet.</p>");
    }

    for slot in slots {
        let history = harness.revisions.history(&slot).unwrap_or_default();
        let live = harness.loader.get(&slot).map(|c| c.revision);

        let rows = history
            .iter()
            .rev()
            .map(|row| {
                let is_live = live == Some(row.revision);
                format!(
                    "<tr><td>r{:04}{}</td><td class=\"{}\">{}</td><td>{}</td><td class=note>{}</td>\
                     <td>{}</td></tr>",
                    row.revision,
                    if is_live { " &larr; live" } else { "" },
                    status_class(row.status),
                    row.status.label(),
                    row.origin.label(),
                    html_escape(&row.note),
                    if is_live {
                        String::new()
                    } else {
                        format!(
                            r#"<form method=post action="/admin/rollback">
                               <input type=hidden name=target value="{}">
                               <input type=hidden name=revision value="{}">
                               <button>restore</button></form>"#,
                            html_escape(&slot.key()),
                            row.revision
                        )
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        sections.push_str(&format!(
            r#"<h3><code>{}</code></h3>
<table><tr><th>revision</th><th>status</th><th>origin</th><th>note</th><th></th></tr>
{}</table>"#,
            html_escape(&slot.key()),
            if rows.is_empty() {
                "<tr><td colspan=5><em>no revisions</em></td></tr>".to_string()
            } else {
                rows
            }
        ));
    }

    // Whole-system snapshots, newest first.
    let mut snapshots = harness.revisions.snapshots().unwrap_or_default();
    snapshots.reverse();
    snapshots.truncate(15);

    let snapshot_rows = snapshots
        .iter()
        .map(|s| {
            let contents = s
                .slots
                .iter()
                .map(|(k, v)| format!("{k}=r{v:04}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "<tr><td>#{}</td><td class=note>{}</td><td class=note>{}</td><td>{}</td></tr>",
                s.id,
                html_escape(&s.cause),
                html_escape(&contents),
                format_args!(
                    r#"<form method=post action="/admin/rollback">
                       <input type=hidden name=target value="system:{}">
                       <button>restore all</button></form>"#,
                    s.id
                )
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let sessions = harness.db.list_sessions(true).map(|s| s.len()).unwrap_or(0);

    format!(
        r#"<!doctype html><meta charset="utf-8"><title>Genesis admin</title>
<style>
 body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:60rem;margin:3rem auto;padding:0 1.5rem;color:#e6e6e6;background:#16161a}}
 h1{{font-size:1.4rem;margin-bottom:.2rem}} h2{{font-size:1.1rem;margin-top:2.2rem}}
 h3{{font-size:.95rem;margin:1.4rem 0 .2rem;font-weight:600}}
 table{{border-collapse:collapse;width:100%;margin:.4rem 0 1rem}}
 th,td{{text-align:left;padding:.45rem .7rem;border-bottom:1px solid #2a2a32;vertical-align:middle}}
 th{{font-size:.7rem;text-transform:uppercase;letter-spacing:.06em;color:#9a9aa8}}
 code{{background:#26262c;padding:.15em .4em;border-radius:4px}}
 button{{font:inherit;font-size:.8rem;color:#e6e6e6;background:#26262c;border:1px solid #35353f;
        border-radius:6px;padding:3px 10px;cursor:pointer}}
 button:hover{{background:#31313c}} form{{margin:0}}
 a{{color:#7aa2f7}} .note{{color:#9a9aa8;font-size:.85rem}}
 .active{{color:#9ece6a}} .known-good{{color:#7aa2f7}} .disabled{{color:#f7768e}}
 .rolled-back{{color:#e0af68}} .candidate{{color:#9a9aa8}}
 .banner{{padding:.7rem 1rem;border-radius:8px;margin:1rem 0}}
 .banner.ok{{background:#1d2a1d;border:1px solid #2f4a2f}}
 .banner.bad{{background:#2f1d21;border:1px solid #5a2f38}}
</style>
<h1>Genesis admin</h1>
<p class=note>Served directly by the orchestrator — no WebAssembly in this page's path.
It keeps working when every guest is broken.</p>
{banner}
<h2>Components</h2>
{sections}
<h2>System snapshots</h2>
<p class=note>Each row is the exact set of revisions the whole system was running at that
moment. Restoring one moves every slot back together.</p>
<table><tr><th>id</th><th>cause</th><th>slots</th><th></th></tr>
{snapshot_rows}</table>
<p class=note>{sessions} session(s) on record.</p>
<p><a href="/">&larr; back to chat</a></p>"#,
        banner = banner,
        sections = sections,
        snapshot_rows = if snapshot_rows.is_empty() {
            "<tr><td colspan=4><em>none yet</em></td></tr>".to_string()
        } else {
            snapshot_rows
        },
        sessions = sessions,
    )
}

fn status_class(status: crate::revisions::Status) -> &'static str {
    status.label()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// --- websocket -------------------------------------------------------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(harness): State<Arc<Harness>>) -> Response {
    ws.on_upgrade(move |socket| connection(socket, harness))
}

async fn connection(socket: WebSocket, harness: Arc<Harness>) {
    let client_id = uuid::Uuid::new_v4().to_string();
    let (mut sink, mut incoming) = socket.split();
    let mut frames = harness.frames_tx.subscribe();
    // Which sessions this browser tab is currently watching.
    let mut watching: HashSet<String> = HashSet::new();

    tracing::debug!(%client_id, "websocket connected");

    loop {
        tokio::select! {
            client_msg = incoming.next() => {
                let Some(Ok(msg)) = client_msg else { break };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    // Keep-alives are handled by axum; nothing else is expected.
                    _ => continue,
                };

                let actions = match gateway::on_client_message(&harness, &client_id, &text).await {
                    Ok(actions) => actions,
                    Err(e) => {
                        // Show the whole chain: the outer context alone ("gateway
                        // on-client-message") says nothing about what went wrong.
                        let detail = format!("{e:#}");
                        tracing::warn!(error = %detail, "gateway rejected a client message");
                        let _ = sink.send(Message::Text(error_frame(&detail).into())).await;
                        continue;
                    }
                };

                for action in actions {
                    match action {
                        GatewayAction::Reply(frame) => {
                            if sink.send(Message::Text(frame.into())).await.is_err() {
                                return;
                            }
                        }
                        GatewayAction::Broadcast(b) => {
                            let _ = harness.frames_tx.send(RenderedFrame {
                                session_id: b.session_id,
                                frame: b.frame,
                            });
                        }
                        GatewayAction::Subscribe(session_id) => {
                            watching.insert(session_id);
                        }
                        GatewayAction::Unsubscribe(session_id) => {
                            watching.remove(&session_id);
                        }
                    }
                }
            }

            broadcast = frames.recv() => {
                match broadcast {
                    Ok(rendered) => {
                        if watching.contains(&rendered.session_id)
                            && sink.send(Message::Text(rendered.frame.into())).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, %client_id, "connection fell behind");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    tracing::debug!(%client_id, "websocket closed");
}

fn error_frame(detail: &str) -> String {
    serde_json::json!({ "type": "error", "message": detail }).to_string()
}
