/* Genesis web client — entry point.
 *
 * The transcript is a pure function of the session event log: everything the
 * user does is submitted to the host and comes back as an event, so several
 * tabs on one conversation stay in step with no client-side reconciliation.
 *
 * This file only wires pieces together. Behaviour lives in views/ and lib/.
 */

import { $ } from "./lib/dom.js";
import { Connection } from "./lib/socket.js";
import { store } from "./lib/store.js";
import { mountComposer } from "./views/composer.js";
import { mountSessions } from "./views/sessions.js";
import * as panel from "./views/panel.js";
import * as transcript from "./views/transcript.js";

// --- status -----------------------------------------------------------------

const statusEl = $("status");

function setStatus(state, text) {
  statusEl.className = `status is-${state}`;
  statusEl.textContent = text;
}

// Busy state is driven by turn events, so it survives reconnects.
store.watch("busy", (busy) => {
  if (busy) setStatus("busy", "working…");
  else if (statusEl.classList.contains("is-busy")) setStatus("online", "connected");
});

// --- connection -------------------------------------------------------------

const connection = new Connection({ onStatus: setStatus });

connection.onOpen(() => {
  connection.send({ type: "hello" });
  if (store.current) connection.send({ type: "open", id: store.current });
});

connection
  .on("catalog", (frame) => {
    store.set({ models: frame.models || [], modes: frame.modes || [] });
  })

  .on("sessions", (frame) => {
    store.set({ sessions: frame.sessions || [] });

    // The host names a conversation from its first message, so the header
    // follows whatever the list now says.
    const active = store.sessions.find((s) => s.id === store.current);
    if (active && active.title !== store.title) {
      store.set({ title: active.title });
      setHeader();
    }

    if (!store.current && store.sessions.length) openSession(store.sessions[0].id);
    if (!store.sessions.length) {
      store.set({ current: null, title: "" });
      setHeader();
      transcript.showEmpty("No conversations", "Start one with the + button.");
    }
  })

  .on("history", (frame) => {
    store.set({
      current: frame.session,
      title: frame.title || "Untitled",
      mode: frame.mode || "agent",
      model: frame.model || "",
      busy: false,
    });
    setHeader();
    transcript.replay(frame.events || []);
  })

  .on("settings", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ mode: frame.mode || "agent", model: frame.model || "" });
    setHeader();
  })

  .on("opened", (frame) => store.set({ current: frame.session }))

  .on("event", (frame) => {
    // The sidebar's title and preview are derived server-side, so refresh the
    // list at the two points they can change. Done before the session check so
    // background conversations stay current too.
    if (frame.kind === "user" || frame.kind === "turn-finished") {
      connection.send({ type: "list" });
    }
    if (frame.session !== store.current) return;
    transcript.applyEvent(frame);
  })

  .on("skills", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ skills: frame.skills || [] });
    if (panel.isOpen("skills")) drawSkills();
  })

  .on("tools", (frame) => {
    if (frame.session !== store.current) return;
    store.set({ tools: frame.tools || [] });
    if (panel.isOpen("tools")) drawTools();
  })

  .on("error", (frame) => transcript.applyEvent({ kind: "incident", text: frame.message }));

// --- inspector panels -------------------------------------------------------

function drawSkills() {
  const on = store.skills.filter((s) => s.enabled).length;
  panel.open({
    id: "skills",
    title: "Skills",
    subtitle: store.skills.length
      ? `${on} of ${store.skills.length} attached to this conversation`
      : undefined,
    items: store.skills,
    empty: "No skills found. Add a markdown file to the skills/ folder.",
    renderItem: (skill) =>
      panel.skillItem(skill, (id, enabled) =>
        connection.send({ type: "set-skill", id: store.current, skill: id, enabled })
      ),
  });
}

function drawTools() {
  panel.open({
    id: "tools",
    title: "Tools",
    subtitle: store.tools.length
      ? `${store.tools.length} available in ${store.modeLabel()} mode`
      : undefined,
    items: store.tools,
    empty: "No tools are available in this mode.",
    renderItem: panel.toolItem,
  });
}

$("show-skills").addEventListener("click", () => {
  if (!store.current) return;
  if (panel.isOpen("skills")) return panel.close();
  // Show what is already known immediately, then refresh from the host.
  store.set({ skills: [] });
  panel.open({ id: "skills", title: "Skills", items: undefined, renderItem: () => null });
  connection.send({ type: "skills", id: store.current });
});

$("show-tools").addEventListener("click", () => {
  if (!store.current) return;
  if (panel.isOpen("tools")) return panel.close();
  store.set({ tools: [] });
  panel.open({ id: "tools", title: "Tools", items: undefined, renderItem: () => null });
  connection.send({ type: "tools", id: store.current });
});

// --- header -----------------------------------------------------------------

function setHeader() {
  $("chat-title").textContent = store.title || "—";
  const bits = [];
  if (store.mode && store.mode !== "agent") bits.push(store.modeLabel());
  if (store.model) bits.push(store.modelLabel());
  $("chat-sub").textContent = bits.join(" · ");
}

store.watch("mode", setHeader);
store.watch("model", setHeader);

// --- actions ----------------------------------------------------------------

function openSession(id) {
  if (id === store.current) return;
  panel.close();
  connection.send({ type: "open", id, previous: store.current || undefined });
}

mountSessions({
  onOpen: openSession,
  onNew: () => connection.send({ type: "new", previous: store.current || undefined }),
});

transcript.mountTranscript();

const composer = mountComposer({
  onSend: (text, attachments) =>
    connection.send({ type: "send", id: store.current, text, attachments }),
  onSetMode: (mode) => connection.send({ type: "set-mode", id: store.current, mode }),
  onSetModel: (model) => connection.send({ type: "set-model", id: store.current, model }),
});

$("rename-chat").addEventListener("click", () => {
  if (!store.current) return;
  const title = prompt("Rename conversation", store.title);
  if (title?.trim()) connection.send({ type: "rename", id: store.current, title: title.trim() });
});

$("archive-chat").addEventListener("click", () => {
  if (!store.current || !confirm("Archive this conversation?")) return;
  connection.send({ type: "archive", id: store.current });
  store.set({ current: null, title: "" });
  setHeader();
  transcript.showEmpty("Archived", "Pick another conversation, or start a new one.");
});

connection.connect();
composer.focus();
