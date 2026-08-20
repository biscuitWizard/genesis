/* The message transcript.
 *
 * Every event kind gets an entry in RENDERERS, so supporting a new one is a
 * single function rather than a new branch in a growing switch.
 */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

let root = null;
let live = null; // element collecting streamed tokens
// Tool calls awaiting their result, by call id. A call and its result are one
// row, so the result has to find the row the call opened.
const open = new Map();

export function mountTranscript() {
  root = $("transcript");
  showEmpty();
}

export function reset() {
  live = null;
  open.clear();
  clear(root);
}

export function showEmpty(message = "Nothing here yet.", hint = "Send a message to begin.") {
  clear(root).append(
    el(
      "div",
      { class: "empty" },
      el("div", {
        class: "empty-mark",
        html: `<svg viewBox="0 0 32 32" width="34" height="34" aria-hidden="true">
                 <circle cx="16" cy="16" r="9" fill="none" stroke="currentColor" stroke-width="2"/>
                 <circle cx="16" cy="16" r="3" fill="currentColor"/></svg>`,
      }),
      el("div", { class: "empty-title" }, message),
      el("div", { class: "empty-hint" }, hint)
    )
  );
}

/** True when the reader is following along at the bottom. */
function atBottom() {
  return root.scrollHeight - root.scrollTop - root.clientHeight < 140;
}

function toBottom(instant) {
  if (instant) {
    const previous = root.style.scrollBehavior;
    root.style.scrollBehavior = "auto";
    root.scrollTop = root.scrollHeight;
    root.style.scrollBehavior = previous;
  } else {
    root.scrollTop = root.scrollHeight;
  }
}

// --- pieces -----------------------------------------------------------------

function row(role, who, ...content) {
  const node = el(
    "div",
    { class: `row ${role}` },
    el("div", { class: "row-head" }, who),
    ...content
  );
  root.append(node);
  return node;
}

function meta(text, tone = "") {
  root.append(el("div", { class: `meta ${tone}`.trim() }, text));
}

function cut(text, max) {
  const flat = String(text ?? "").replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

/** Pretty-print JSON when it is JSON, and leave anything else alone. */
function pretty(raw) {
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

/** The gist of a call's arguments, short enough to sit on the summary line. */
function gist(raw) {
  if (!raw) return "";
  let value;
  try {
    value = JSON.parse(raw);
  } catch {
    return cut(raw, 80);
  }
  if (value === null || typeof value !== "object") return cut(value, 80);

  return cut(
    Object.entries(value)
      .map(([k, v]) => `${k}: ${cut(typeof v === "string" ? v : JSON.stringify(v), 40)}`)
      .join("  ·  "),
    90
  );
}

function section(label, body) {
  return [el("div", { class: "tool-label" }, label), el("pre", {}, body || "")];
}

/* One row per tool call.
 *
 * A call and the result it returns are one thing to a reader - what ran, and
 * what came back - so they share a row instead of stacking two cards. The row
 * opens on the call and is completed by the result, matched on the call id the
 * host puts on both.
 */
function toolRow(ev) {
  const node = el(
    "details",
    { class: "tool is-running" },
    el(
      "summary",
      {},
      el("span", { class: "tool-name" }, ev.name),
      el("span", { class: "tool-args" }, gist(ev.arguments)),
      el("span", { class: "tool-status" }, "running")
    ),
    ...section("arguments", pretty(ev.arguments))
  );
  root.append(node);
  return node;
}

/** Fills in the row a call opened, or makes a standalone one if it is missing. */
function completeToolRow(ev) {
  let node = open.get(ev.id);
  open.delete(ev.id);

  if (!node) {
    // A result with no call in view: replaying a truncated log, or a call that
    // predates this connection. Better a row on its own than a dropped result.
    node = el("details", { class: "tool" }, el("summary", {},
      el("span", { class: "tool-name" }, ev.name),
      el("span", { class: "tool-args" }, ""),
      el("span", { class: "tool-status" }, "")));
    root.append(node);
  }

  node.classList.remove("is-running");
  node.classList.toggle("is-bad", !ev.ok);
  const status = node.querySelector(".tool-status");
  if (status) status.textContent = ev.ok ? "ok" : "failed";
  node.append(...section("result", ev.content));
}

function thumbs(attachments) {
  if (!attachments?.length) return null;
  return el(
    "div",
    { class: "thumbs" },
    attachments.map((a) =>
      a.data
        ? el("img", { class: "thumb", src: a.data, alt: a.name, title: a.name })
        : el("div", { class: "thumb-file" }, a.name)
    )
  );
}

// --- event renderers --------------------------------------------------------

const RENDERERS = {
  user(ev) {
    live = null;
    row("user", "you",
      ev.text ? el("div", { class: "bubble-text" }, ev.text) : null,
      thumbs(ev.attachments));
  },

  delta(ev) {
    if (!live) {
      const node = row("assistant", "genesis", el("div", { class: "bubble-text is-streaming" }));
      live = node.querySelector(".bubble-text");
    }
    live.textContent += ev.text;
  },

  assistant(ev) {
    // The final message is authoritative: it replaces whatever streamed, so a
    // reconnect part-way through still lands on the right text.
    if (live) {
      live.textContent = ev.text;
      live.classList.remove("is-streaming");
      live = null;
    } else if (ev.text?.trim()) {
      row("assistant", "genesis", el("div", { class: "bubble-text" }, ev.text));
    }

    // Cache hits are invisible otherwise, and a saving you cannot see is one
    // you cannot trust.
    const cached = ev.usage?.cached ?? 0;
    if (cached > 0) {
      const total = ev.usage?.prompt ?? 0;
      const share = total > 0 ? Math.round((cached / total) * 100) : 0;
      meta(`${cached.toLocaleString()} of ${total.toLocaleString()} prompt tokens cached (${share}%)`, "is-good");
    }
  },

  "tool-call"(ev) {
    live = null;
    open.set(ev.id, toolRow(ev));
  },

  "tool-result": completeToolRow,

  compacted(ev) {
    live = null;
    // Foldable rather than a bare line: the summary is what the model now sees
    // in place of those messages, so it should be readable on demand.
    const node = el(
      "details",
      { class: "tool" },
      el(
        "summary",
        {},
        el("span", { class: "tool-name" }, "context compacted"),
        el(
          "span",
          { class: "tool-args" },
          `${ev.replaced} earlier messages summarized · was ~${(ev.tokens_before ?? 0).toLocaleString()} tokens`
        ),
        el("span", { class: "tool-status" }, "summary")
      ),
      ...section("summary", ev.summary)
    );
    root.append(node);
  },

  nudge: (ev) => meta(`you interrupted: ${ev.text}`, "is-nudge"),
  note: (ev) => meta(ev.text),

  incident(ev) {
    live = null;
    // An incident ends a turn. Without this, replaying a log that ends in one
    // would leave the composer showing "working…" with nothing running.
    store.set({ busy: false });
    meta(ev.text, "is-incident");
  },

  modification(ev) {
    const revision = ev.revision != null ? ` → r${String(ev.revision).padStart(4, "0")}` : "";
    const detail = ev.detail ? ` — ${ev.detail}` : "";
    meta(
      `${ev.ok ? "updated" : "could not update"} ${ev.slot}${revision}${detail}`,
      ev.ok ? "is-good" : "is-incident"
    );
  },

  "turn-started": () => store.set({ busy: true }),

  "turn-finished"(ev) {
    store.set({ busy: false });
    live = null;
    const bits = [];
    if (ev.iterations > 1) bits.push(`${ev.iterations} steps`);
    if (ev.cost > 0) bits.push(`$${ev.cost.toFixed(4)}`);
    if (ev.stopped_by && ev.stopped_by !== "stop") bits.push(ev.stopped_by);
    if (bits.length) meta(bits.join(" · "));
  },
};

export function applyEvent(ev) {
  const follow = atBottom();
  // Any event at all means this conversation is no longer empty, so the
  // placeholder goes before the first message is appended after it.
  root.querySelector(".empty")?.remove();
  RENDERERS[ev.kind]?.(ev);
  if (follow) toBottom(false);
}

export function replay(events) {
  reset();
  if (!events.length) {
    showEmpty("No messages yet.", "Say something to get started.");
    return;
  }
  events.forEach((ev) => RENDERERS[ev.kind]?.(ev));
  // A restored transcript should start at the end, without animating there.
  toBottom(true);
}
