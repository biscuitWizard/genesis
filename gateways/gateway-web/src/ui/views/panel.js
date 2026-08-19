/* A slide-over panel for inspecting what the agent can do.
 *
 * Generic on purpose: it handles opening, closing, focus and the empty and
 * loading states. Each panel supplies only how to draw one item, so a third
 * inspector is a render function rather than another dialog.
 */

import { clear, el, icon } from "../lib/dom.js";

const X = ["M5 5l10 10", "M15 5l-10 10"];

let host = null;
let current = null; // id of the panel showing, or null

function ensureHost() {
  if (host) return host;
  host = el("div", { class: "panel-host", hidden: true });
  document.body.append(host);

  // Escape and backdrop clicks both close: a panel is an aside, never a trap.
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && current) close();
  });
  host.addEventListener("click", (event) => {
    if (event.target === host) close();
  });
  return host;
}

export function isOpen(id) {
  return current === id;
}

export function close() {
  current = null;
  if (host) {
    host.hidden = true;
    clear(host);
  }
}

/**
 * Opens (or re-renders) a panel.
 *
 * @param {object} config
 * @param {string} config.id            distinguishes panels from each other
 * @param {string} config.title
 * @param {string} [config.subtitle]
 * @param {Array}  [config.items]       undefined means "still loading"
 * @param {(item) => Node} config.renderItem
 * @param {string} [config.empty]       message when there are no items
 */
export function open(config) {
  const mount = ensureHost();
  current = config.id;
  mount.hidden = false;

  const body = config.items === undefined
    ? el("div", { class: "panel-note" }, "Loading…")
    : config.items.length === 0
      ? el("div", { class: "panel-note" }, config.empty || "Nothing to show.")
      : el("div", { class: "panel-list" }, config.items.map(config.renderItem));

  clear(mount).append(
    el(
      "aside",
      { class: "panel", role: "dialog", "aria-label": config.title },
      el(
        "header",
        { class: "panel-head" },
        el(
          "div",
          {},
          el("h2", { class: "panel-title" }, config.title),
          config.subtitle && el("p", { class: "panel-sub" }, config.subtitle)
        ),
        el(
          "button",
          { class: "icon-btn", title: "Close", "aria-label": "Close", onClick: close },
          icon(X, { size: 15, width: 1.8 })
        )
      ),
      el("div", { class: "panel-body" }, body)
    )
  );
}

// --- item renderers ---------------------------------------------------------

/** A skill: what it does, its full instructions, and a switch. */
export function skillItem(skill, onToggle) {
  const toggle = el("button", {
    class: `switch${skill.enabled ? " is-on" : ""}`,
    role: "switch",
    "aria-checked": String(skill.enabled),
    title: skill.enabled ? "Detach from this conversation" : "Attach to this conversation",
    onClick: () => onToggle(skill.id, !skill.enabled),
  }, el("span", { class: "switch-knob" }));

  return el(
    "article",
    { class: `card${skill.enabled ? " is-on" : ""}` },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h3", { class: "card-title" }, skill.name),
        skill.description && el("p", { class: "card-desc" }, skill.description)
      ),
      toggle
    ),
    skill.instructions &&
      el(
        "details",
        { class: "card-more" },
        el("summary", {}, "Instructions"),
        el("pre", { class: "card-pre" }, skill.instructions)
      )
  );
}

/** A tool: what it does, how it is provided, and its arguments. */
export function toolItem(tool) {
  const badges = (tool.capabilities || []).map((cap) =>
    el("span", { class: `badge is-${cap.replace(/[^a-z]/gi, "-")}` }, cap)
  );

  return el(
    "article",
    { class: "card" },
    el(
      "div",
      { class: "card-head" },
      el(
        "div",
        { class: "card-heading" },
        el("h3", { class: "card-title mono" }, tool.name),
        tool.description && el("p", { class: "card-desc" }, tool.description)
      ),
      badges.length ? el("div", { class: "badges" }, badges) : null
    ),
    el(
      "details",
      { class: "card-more" },
      el("summary", {}, "Arguments"),
      el("pre", { class: "card-pre" }, formatSchema(tool.schema))
    )
  );
}

/** Renders a JSON Schema as a readable argument list rather than raw JSON. */
function formatSchema(raw) {
  let schema;
  try {
    schema = JSON.parse(raw || "{}");
  } catch {
    return raw || "(none)";
  }

  const props = schema.properties || {};
  const names = Object.keys(props);
  if (!names.length) return "Takes no arguments.";

  const required = new Set(schema.required || []);
  return names
    .map((name) => {
      const spec = props[name] || {};
      const type = spec.type || "any";
      const flag = required.has(name) ? "required" : "optional";
      const note = spec.description ? `\n    ${spec.description}` : "";
      return `${name} (${type}, ${flag})${note}`;
    })
    .join("\n\n");
}
