/* Client state.
 *
 * One object, one change notification. Views subscribe to what they care about
 * and re-render; nothing mutates the DOM behind the store's back.
 */

export const store = {
  sessions: [],
  current: null,
  title: "",
  mode: "",
  model: "",
  models: [],
  modes: [],
  busy: false,
  attachments: [],
  skills: [],
  tools: [],

  _watchers: new Map(),

  /** Subscribes to one key. Returns an unsubscribe function. */
  watch(key, fn) {
    if (!this._watchers.has(key)) this._watchers.set(key, new Set());
    this._watchers.get(key).add(fn);
    return () => this._watchers.get(key).delete(fn);
  },

  /** Applies a patch and notifies watchers of the keys that actually changed. */
  set(patch) {
    const touched = [];
    for (const [key, value] of Object.entries(patch)) {
      if (this[key] === value) continue;
      this[key] = value;
      touched.push(key);
    }
    for (const key of touched) {
      this._watchers.get(key)?.forEach((fn) => fn(this[key], this));
    }
    return touched;
  },

  /** Forces watchers to run even when the reference is unchanged. */
  touch(...keys) {
    for (const key of keys) {
      this._watchers.get(key)?.forEach((fn) => fn(this[key], this));
    }
  },

  modeLabel() {
    return this.modes.find((m) => m.id === this.mode)?.label || "Agent";
  },

  modelLabel() {
    if (!this.model) return "Default model";
    return this.models.find((m) => m.id === this.model)?.label || this.model;
  },
};
