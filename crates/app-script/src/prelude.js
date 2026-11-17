// The app host's side of an app.ts, run in the app's own context before
// the app. It defines the only globals an app gets (`app`, `ui`,
// `dreamer`, the timers, and `console`) and returns the hooks the host
// drives it through, which the app cannot reach.
//
// `host` holds the host's functions: call(id, method, paramsJson),
// timer(id, ms), cancel(id), log(text), fatal(text).
(function (host, methods, info, fixture) {
  "use strict";

  let spec = null;
  let state = {};
  let data = undefined;
  let loaded = false;
  let live = false;
  let error = "";
  let dirty = true;
  let loadSeq = 0;
  let refresh = 0;
  let handlers = new Map();

  const internal = (e) =>
    typeof InternalError === "function" && e instanceof InternalError;

  function text(e) {
    if (e instanceof Error) return e.message || e.name;
    return String(e);
  }

  // An error in a handler, `load`, or a timer: shown on the status line.
  // Running out of memory ends the app. QuickJS throws `null` when it
  // cannot even allocate the error, so a thrown `null` ends it too.
  function fail(e) {
    if (e === null) {
      host.fatal("out of memory (or the app threw null)");
      return;
    }
    if (internal(e)) {
      host.fatal(text(e));
      return;
    }
    error = text(e);
    dirty = true;
  }

  function plain(v) {
    return (
      v !== null &&
      typeof v === "object" &&
      !Array.isArray(v) &&
      typeof v.then !== "function"
    );
  }

  function merge(p) {
    if (plain(p)) state = Object.assign({}, state, p);
    dirty = true;
  }

  function settle(r) {
    if (r && typeof r.then === "function") {
      r.then((v) => {
        error = "";
        merge(v);
      }, fail);
    } else {
      error = "";
      merge(r);
    }
  }

  const ctx = Object.freeze({
    get state() {
      return state;
    },
    get data() {
      return data;
    },
    get live() {
      return live;
    },
    set(p) {
      merge(p);
    },
    reload() {
      reload();
    },
  });

  function invoke(fn, args) {
    try {
      settle(fn(...args, ctx));
    } catch (e) {
      fail(e);
    }
  }

  function reload() {
    if (fixture !== null) return;
    if (!spec || typeof spec.load !== "function") {
      loaded = true;
      dirty = true;
      return;
    }
    const seq = ++loadSeq;
    let r;
    try {
      r = spec.load(ctx);
    } catch (e) {
      fail(e);
      return;
    }
    Promise.resolve(r).then((d) => {
      if (seq !== loadSeq) return;
      data = d;
      loaded = true;
      error = "";
      dirty = true;
    }, fail);
  }

  // Calls to the daemon, under the app's grant.
  class DreamerError extends Error {
    constructor(message, code) {
      super(message);
      this.name = "DreamerError";
      this.code = code;
    }
  }
  const pending = new Map();
  let nextCall = 0;
  function call(method, params) {
    if (typeof method !== "string") {
      return Promise.reject(new TypeError("dreamer.call: give a method name"));
    }
    let json;
    try {
      json = JSON.stringify(params === undefined ? {} : params);
    } catch (e) {
      return Promise.reject(e);
    }
    const id = ++nextCall;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      host.call(id, method, json);
    });
  }

  const dreamer = { call, DreamerError };
  for (const m of methods) {
    const parts = m.split(".");
    let o = dreamer;
    for (const p of parts.slice(0, -1)) {
      if (!Object.prototype.hasOwnProperty.call(o, p)) o[p] = {};
      o = o[p];
    }
    o[parts[parts.length - 1]] = (params) => call(m, params);
  }
  const freeze = (o) => {
    for (const v of Object.values(o)) {
      if (v && typeof v === "object") freeze(v);
    }
    return Object.freeze(o);
  };
  freeze(dreamer);

  // Timers, run by the host.
  const timers = new Map();
  let nextTimer = 0;
  function schedule(fn, ms, args, every) {
    if (typeof fn !== "function") throw new TypeError("a timer needs a function");
    const id = ++nextTimer;
    const wait = Math.max(every ? 10 : 0, Number(ms) || 0);
    timers.set(id, { fn, args, every: every ? wait : 0 });
    host.timer(id, wait);
    return id;
  }
  const setTimeout = (fn, ms, ...args) => schedule(fn, ms, args, false);
  const setInterval = (fn, ms, ...args) => schedule(fn, ms, args, true);
  function clearTimeout(id) {
    if (timers.delete(id)) host.cancel(id);
  }

  // Builders for view-tree nodes.
  const cell = (v) =>
    v === undefined || v === null
      ? ""
      : typeof v === "object"
        ? JSON.stringify(v)
        : String(v);
  function rowKeys(rows, rowKey) {
    const seen = new Set();
    return rows.map((r, i) => {
      let k =
        r !== null && typeof r === "object" && r[rowKey] !== undefined
          ? String(r[rowKey])
          : String(i);
      while (seen.has(k)) k = k + "~";
      seen.add(k);
      return k;
    });
  }
  function opts(a, field) {
    if (a !== null && typeof a === "object" && !Array.isArray(a)) return a;
    return field ? { [field]: a } : {};
  }
  const layout = (type) => (a, b) =>
    Array.isArray(a)
      ? Object.assign({}, b || {}, { type, children: a })
      : Object.assign({ children: [] }, a || {}, { type });
  const content = (type, field, more) => (a, b) =>
    Object.assign({}, more || {}, opts(a, field), b || {}, { type });

  const ui = {
    column: layout("column"),
    row: layout("row"),
    scroll: layout("scroll"),
    tabs: layout("tabs"),
    split: layout("split"),
    text: content("text", "text", { text: "" }),
    markdown: content("markdown", "text", { text: "" }),
    code: content("code", "text", { text: "" }),
    diff: content("diff", "text", { text: "" }),
    badge: content("badge", "text", { text: "" }),
    gauge: content("gauge", "ratio", { ratio: 0, label: "" }),
    progress: content("progress", "ratio", { ratio: 0 }),
    chart: content("chart", "values", { values: [] }),
    status: content("status", "items", { items: [] }),
    button: content("button", "label", { label: "" }),
    input: content("input", "value", { value: "" }),
    select: content("select", "options", { options: [] }),
    toggle: content("toggle", "label", { label: "", on: false }),
    terminal: content("terminal", "pane"),
    image: content("image", "src", { src: "" }),
    table(o) {
      o = opts(o);
      const columns = (o.columns || []).map(String);
      const rows = Array.from(o.rows || []);
      const keys = rowKeys(rows, o.rowKey === undefined ? "id" : o.rowKey);
      const children = rows.map((r, i) => ({
        type: "item",
        key: keys[i],
        cells: columns.map((c) =>
          r !== null && typeof r === "object" ? cell(r[c]) : cell(r),
        ),
      }));
      return Object.assign({}, o, { type: "table", columns, children, rows, keys });
    },
    list(o) {
      o = opts(o);
      const rows = Array.from(o.rows || []);
      const keys = rowKeys(rows, o.rowKey === undefined ? "id" : o.rowKey);
      const children = rows.map((r, i) => ({
        type: "item",
        key: keys[i],
        cells: [
          r !== null && typeof r === "object" && o.field !== undefined
            ? cell(r[o.field])
            : cell(r),
        ],
      }));
      return Object.assign({}, o, { type: "list", children, rows, keys });
    },
    log(o) {
      o = opts(o, "lines");
      const lines = Array.from(o.lines || []);
      const children = lines.map((l, i) => ({
        type: "item",
        key: String(i),
        cells: [cell(l)],
      }));
      return Object.assign({}, o, { type: "log", children });
    },
    screen(root, o) {
      return Object.assign({}, o || {}, { root, screen: true });
    },
  };
  freeze(ui);

  // The wire form of each type: its fields, and their defaults.
  const FIELDS = {
    column: {},
    row: {},
    scroll: {},
    tabs: { active: 0 },
    split: { direction: "row", sizes: [] },
    table: { columns: [], selected: undefined },
    list: { selected: undefined },
    log: {},
    item: { cells: [] },
    text: { text: "" },
    markdown: { text: "" },
    code: { text: "", language: "" },
    diff: { text: "" },
    badge: { text: "", tone: "neutral" },
    gauge: { ratio: 0, label: "" },
    progress: { ratio: 0, label: "" },
    chart: { values: [] },
    status: { items: [] },
    button: { label: "" },
    input: { label: "", value: "", placeholder: "" },
    select: { label: "", options: [], selected: 0 },
    toggle: { label: "", on: false },
    terminal: { pane: undefined },
    image: { src: "" },
  };
  const ON = ["onPress", "onChange", "onSubmit", "onEnter", "onSelect"];

  function wire(n, fallback) {
    if (n === null || n === undefined || n === false) return null;
    if (typeof n === "string" || typeof n === "number") {
      n = { type: "text", text: String(n) };
    }
    const fields = n && typeof n === "object" ? FIELDS[n.type] : undefined;
    if (!fields) {
      throw new TypeError(
        "view: `" + (n && n.type) + "` is not a node (build nodes with ui.*)",
      );
    }
    const key = n.key === undefined || n.key === null ? fallback : String(n.key);
    const out = { key, type: n.type };
    if (n.title !== undefined && n.title !== null) out.title = String(n.title);
    for (const [f, d] of Object.entries(fields)) {
      let v = n[f] === undefined ? d : n[f];
      if (n.type === "select" && f === "selected" && n.value !== undefined) {
        v = Math.max(0, (n.options || []).map(String).indexOf(String(n.value)));
      }
      if (f === "pane" && (v === undefined || v === null)) {
        throw new TypeError("view: a terminal needs `pane`, the pane it shows");
      }
      if (v === undefined) continue;
      if (f === "label" || f === "text" || f === "value" || f === "placeholder" || f === "language" || f === "src") {
        v = String(v);
      } else if (f === "pane") {
        // A pane by number, or by its tmux name: 3 or "%3".
        const p = typeof v === "string" ? Number(v.replace(/^%/, "")) : Number(v);
        if (!Number.isInteger(p) || p < 0 || p > 0xffffffff) {
          throw new TypeError("view: a terminal's `pane` is a pane number, not `" + cell(v) + "`");
        }
        v = p;
      } else if (f === "options" || f === "cells" || f === "columns") {
        v = Array.from(v, String);
      } else if (f === "selected" && (n.type === "table" || n.type === "list")) {
        v = String(v);
      }
      out[f] = v;
    }
    const on = {};
    for (const h of ON) if (typeof n[h] === "function") on[h] = n[h];
    if (!handlers.has(key)) {
      const rows = new Map();
      if (n.keys && n.rows) n.keys.forEach((k, i) => rows.set(k, n.rows[i]));
      handlers.set(key, { on, rows, options: out.options });
    }
    if (Array.isArray(n.children)) {
      const kids = [];
      n.children.forEach((c, i) => {
        const w = wire(c, "n" + i);
        if (w) kids.push(w);
      });
      if (kids.length) out.children = kids;
    }
    return out;
  }

  function app(s) {
    if (spec) throw new Error("app() is called once");
    if (!s || typeof s.view !== "function") {
      throw new TypeError("app() needs an object with a view(state, data) function");
    }
    spec = s;
    state = Object.assign({}, s.state || {});
    if (fixture !== null) {
      data = fixture;
      loaded = true;
    }
    dirty = true;
  }

  const console = Object.freeze({
    log: (...a) => host.log(a.map(cell).join(" ")),
    warn: (...a) => host.log(a.map(cell).join(" ")),
    error: (...a) => host.log(a.map(cell).join(" ")),
  });

  const g = globalThis;
  for (const [k, v] of Object.entries({
    app,
    ui,
    dreamer,
    setTimeout,
    setInterval,
    clearTimeout,
    clearInterval: clearTimeout,
    console,
  })) {
    Object.defineProperty(g, k, { value: v, enumerable: false, writable: false });
  }

  function link(l) {
    live = l;
    dirty = true;
    if (live && spec) {
      reload();
      const every = Number(spec.refresh) || 0;
      if (every >= 200 && !refresh) {
        refresh = setInterval(() => live && reload(), every);
      }
    }
  }

  // The hooks the host drives the app through.
  return {
    // After the app's module ran.
    start(l) {
      if (!spec) throw new Error("the app did not call app({ ... })");
      if (fixture === null && typeof spec.load !== "function") loaded = true;
      link(l);
    },
    link,
    result(id, ok, message, code) {
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (message == null) p.resolve(ok == null ? null : JSON.parse(ok));
      else p.reject(new DreamerError(message, code));
    },
    fire(id) {
      const t = timers.get(id);
      if (!t) return;
      if (t.every) host.timer(id, t.every);
      else timers.delete(id);
      try {
        const r = t.fn(...t.args);
        if (r && typeof r.then === "function") r.then(undefined, fail);
      } catch (e) {
        fail(e);
      }
      dirty = true;
    },
    notify(topic, payload) {
      if (spec && typeof spec.onEvent === "function") {
        invoke(spec.onEvent, [topic, JSON.parse(payload)]);
      }
    },
    // A component's event: false when the node has no handler for it.
    event(key, action, value) {
      const h = handlers.get(key);
      if (!h) return false;
      const v = value == null ? undefined : JSON.parse(value);
      let fn;
      let args;
      switch (action) {
        case "press":
          fn = h.on.onPress;
          args = [];
          break;
        case "change":
          fn = h.on.onChange;
          args = h.options ? [h.options[v], v] : [v];
          break;
        case "submit":
          fn = h.on.onSubmit || h.on.onEnter;
          args = [v];
          break;
        case "select":
          fn = h.on.onSelect;
          args = [h.rows.get(v), v];
          break;
        case "enter":
          fn = h.on.onEnter;
          args = [h.rows.get(v), v];
          break;
        default:
          return false;
      }
      if (typeof fn !== "function") return false;
      invoke(fn, args);
      return true;
    },
    // A key no component took.
    key(name) {
      if (spec && typeof spec.onKey === "function") invoke(spec.onKey, [name]);
    },
    // The frame as JSON, or null when nothing changed.
    render() {
      if (!dirty) return null;
      dirty = false;
      handlers = new Map();
      let frame;
      if (!spec) {
        frame = { root: { key: "app", type: "text", text: "" } };
      } else if (!loaded) {
        frame = {
          root: {
            key: "loading",
            type: "text",
            text: error ? "" : live ? "loading…" : "waiting for the daemon…",
          },
        };
      } else {
        const v = spec.view(state, data, ctx);
        frame = v && v.screen === true ? v : { root: v };
      }
      const root = wire(frame.root, "root");
      if (!root) throw new TypeError("view: returned no node");
      const status = error
        ? "error: " + error
        : frame.status === undefined || frame.status === null
          ? ""
          : String(frame.status);
      return JSON.stringify({
        title: frame.title === undefined ? info.title : String(frame.title),
        status,
        focus: frame.focus === undefined || frame.focus === null ? undefined : String(frame.focus),
        root,
      });
    },
  };
})
