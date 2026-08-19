/* The composer: text, attachments, and the mode and model pickers. */

import { $, clear, el, icon } from "../lib/dom.js";
import { store } from "../lib/store.js";
import { Picker } from "./picker.js";

const X = ["M4 4l8 8", "M12 4l-8 8"];

/** Images only, and small enough that base64 in a websocket frame is sane. */
const MAX_BYTES = 8 * 1024 * 1024;

export function mountComposer({ onSend, onSetMode, onSetModel }) {
  const form = $("composer");
  const input = $("input");
  const sendBtn = $("send");
  const tray = $("attachments");
  const fileInput = $("file-input");
  const veil = $("drop-veil");

  // --- pickers --------------------------------------------------------------

  const modePicker = new Picker($("mode-picker"), {
    title: "How Genesis should work in this conversation",
    options: () => store.modes.map((m) => ({ id: m.id, label: m.label, note: m.description })),
    selected: () => store.mode,
    render: () => store.modeLabel(),
    dotClass: (id) => (id === "plan" ? "is-plan" : ""),
    onSelect: onSetMode,
  });

  const modelPicker = new Picker($("model-picker"), {
    title: "Model for this conversation",
    options: () => [
      { id: "", label: "Default model", note: "Whatever the harness is configured with." },
      ...store.models.map((m) => ({ id: m.id, label: m.label, note: m.id })),
    ],
    selected: () => store.model,
    render: () => store.modelLabel(),
    onSelect: onSetModel,
  });

  store.watch("modes", () => modePicker.refresh());
  store.watch("models", () => modelPicker.refresh());
  store.watch("mode", () => modePicker.refresh());
  store.watch("model", () => modelPicker.refresh());

  // --- attachments ----------------------------------------------------------

  function drawTray() {
    clear(tray);
    tray.hidden = store.attachments.length === 0;

    store.attachments.forEach((file, index) => {
      tray.append(
        el(
          "div",
          { class: "chip", title: file.name },
          file.mime.startsWith("image/")
            ? el("img", { src: `data:${file.mime};base64,${file.data}`, alt: "" })
            : null,
          el("span", { class: "chip-name" }, file.name),
          el(
            "button",
            {
              type: "button",
              class: "chip-x",
              title: "Remove",
              "aria-label": `Remove ${file.name}`,
              onClick: () => {
                store.attachments.splice(index, 1);
                store.touch("attachments");
              },
            },
            icon(X, { size: 11, width: 1.9 })
          )
        )
      );
    });
    updateSendState();
  }

  store.watch("attachments", drawTray);

  async function addFiles(files) {
    for (const file of files) {
      if (!file.type.startsWith("image/")) continue;
      if (file.size > MAX_BYTES) {
        alert(`${file.name} is too large (limit ${Math.round(MAX_BYTES / 1024 / 1024)} MB).`);
        continue;
      }
      store.attachments.push({
        name: file.name || "pasted-image.png",
        mime: file.type,
        data: await toBase64(file),
      });
    }
    store.touch("attachments");
  }

  function toBase64(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      // The result is a data URL; the payload is everything after the comma.
      reader.onload = () => resolve(String(reader.result).split(",", 2)[1] || "");
      reader.onerror = reject;
      reader.readAsDataURL(file);
    });
  }

  $("attach").addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", async () => {
    await addFiles([...fileInput.files]);
    fileInput.value = "";
  });

  input.addEventListener("paste", (event) => {
    const files = [...(event.clipboardData?.files || [])];
    if (files.length) {
      event.preventDefault();
      addFiles(files);
    }
  });

  // --- drag and drop --------------------------------------------------------

  // The veil is held open by a timer that every `dragover` refreshes, rather
  // than by counting dragenter/dragleave pairs. Those fire once per child
  // element and go missing entirely when a drag ends outside the window, which
  // strands the overlay on screen. A lapsing timer cannot get stuck: the moment
  // events stop arriving, the veil clears itself.
  const VEIL_LINGER_MS = 160;
  let veilTimer = null;

  const draggingFiles = (event) =>
    [...(event.dataTransfer?.types || [])].includes("Files");

  function holdVeil() {
    veil.hidden = false;
    clearTimeout(veilTimer);
    veilTimer = setTimeout(dropVeil, VEIL_LINGER_MS);
  }

  function dropVeil() {
    clearTimeout(veilTimer);
    veilTimer = null;
    veil.hidden = true;
  }

  // Bound to the window so a drop anywhere attaches, and so a file dropped
  // outside the composer never navigates the page away.
  window.addEventListener("dragover", (event) => {
    if (!draggingFiles(event)) return;
    event.preventDefault();
    holdVeil();
  });

  window.addEventListener("drop", async (event) => {
    if (!draggingFiles(event)) return;
    event.preventDefault();
    dropVeil();
    await addFiles([...(event.dataTransfer?.files || [])]);
  });

  // Belt and braces for the cases the timer would only catch a beat later.
  window.addEventListener("dragend", dropVeil);
  window.addEventListener("blur", dropVeil);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) dropVeil();
  });

  // --- sending --------------------------------------------------------------

  function updateSendState() {
    const hasContent = input.value.trim() !== "" || store.attachments.length > 0;
    sendBtn.disabled = !hasContent || !store.current;
  }

  function autosize() {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, window.innerHeight * 0.4)}px`;
  }

  input.addEventListener("input", () => {
    autosize();
    updateSendState();
  });

  // Enter sends. Sending mid-reply is allowed on purpose: the orchestrator
  // turns it into a nudge for the running turn rather than a second turn.
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form.requestSubmit();
    }
  });

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!store.current || (!text && !store.attachments.length)) return;

    onSend(text, store.attachments.slice());

    input.value = "";
    store.attachments.length = 0;
    store.touch("attachments");
    autosize();
    input.focus();
  });

  store.watch("current", updateSendState);
  drawTray();
  return { focus: () => input.focus() };
}
