/* The conversation sidebar. */

import { $, clear, el } from "../lib/dom.js";
import { store } from "../lib/store.js";

export function mountSessions({ onOpen, onNew }) {
  const list = $("session-list");
  $("new-chat").addEventListener("click", onNew);

  const draw = () => {
    clear(list);

    if (!store.sessions.length) {
      list.append(el("div", { class: "session-preview", style: "padding:10px 11px" },
        "No conversations yet."));
      return;
    }

    for (const session of store.sessions) {
      list.append(
        el(
          "button",
          {
            class: `session${session.id === store.current ? " is-active" : ""}`,
            title: session.title || "Untitled",
            onClick: () => onOpen(session.id),
          },
          el("div", { class: "session-title" }, session.title || "Untitled"),
          el("div", { class: "session-preview" }, session.preview || "no messages yet")
        )
      );
    }
  };

  store.watch("sessions", draw);
  store.watch("current", draw);
  draw();
}
