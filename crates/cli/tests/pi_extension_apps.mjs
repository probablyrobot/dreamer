// Drives the pi extension's app tools (the profile's
// extensions/dreamer/daemon.ts, `Apps`) against a real dreamerd and the
// `dream` under test. Run by tests/pi_profile.rs with the daemon's
// environment; prints one JSON line per step and exits non-zero on the
// first failure.
//
// argv: path to daemon.ts, the `dream` program, and the apps directory.

const [, , modulePath, dream, root] = process.argv;
const m = await import(modulePath);
const fs = await import("node:fs");
const path = await import("node:path");

const step = (name, data) => console.log(JSON.stringify({ step: name, ...data }));
const fail = (name, e) => {
	step(name, { ok: false, error: e instanceof Error ? e.message : String(e) });
	process.exit(1);
};
const refused = async (name, f, want) => {
	try {
		const r = await f();
		fail(name, `not refused: ${JSON.stringify(r)}`);
	} catch (e) {
		const why = e instanceof Error ? e.message : String(e);
		if (!why.includes(want)) fail(name, `refused, but not for ${want}: ${why}`);
		step(name, { ok: true, refused: why });
	}
};

const s = new m.Session(process.env);
const apps = new m.Apps(s, dream, fs.realpathSync(root));

let dir;
try {
	const r = await apps.create({ name: "hello" });
	dir = r.dir;
	step("new", { ok: dir === path.join(apps.root, "hello"), result: r });
} catch (e) {
	fail("new", e);
}

// An edit with a type error: the check says where.
const source = path.join(dir, "app.ts");
const good = fs.readFileSync(source, "utf8");
const line = good.split("\n").findIndex((l) => l.includes("ui.table({")) + 1;
fs.writeFileSync(source, good.replace("ui.table({", "ui.tabel({"));
try {
	const c = await apps.check({ dir: "hello" });
	const p = c.problems?.[0];
	step("type error", {
		ok: c.ok === false && p?.file === "app.ts" && p?.line === line && p?.column > 0 && p.message.includes("ui.tabel"),
		result: c,
	});
} catch (e) {
	fail("type error", e);
}
fs.writeFileSync(source, good);
try {
	const c = await apps.check({ dir: "hello" });
	step("fixed", { ok: c.ok === true, result: c });
} catch (e) {
	fail("fixed", e);
}
await refused("outside", () => apps.check({ dir: "../elsewhere" }), "is not allowed");

try {
	const t = await apps.preview({ dir: "hello" });
	step("preview text", { ok: t.text.includes("cargo") && t.image === undefined, text: t.text });
	const r = await apps.test({ dir: "hello" });
	step("test", { ok: r.ok === true, result: r });
} catch (e) {
	fail("preview text", e);
}
await refused("preview image", () => apps.preview({ dir: "hello", mode: "image" }), "no Dreamer window is running");

let name;
try {
	const r = await apps.install({ dir: "hello" });
	name = r.name;
	step("install", { ok: name === "local.hello", result: r });
} catch (e) {
	fail("install", e);
}

// The app opened beside a pane this session created: no approval needed.
try {
	const run = await s.call("terminal.run", { argv: ["/bin/sh"], name: "agent-own", timeout_ms: 300 });
	const own = run.job?.pane;
	const r = await apps.launch({ name, beside: own }, {});
	const t = await s.call("tmux.topology", {});
	const win = (p) => (t.panes ?? []).find((x) => x.id === p)?.window;
	const list = await s.call("terminal.list", {});
	const owned = (list.terminals ?? []).find((x) => x.pane === r.pane)?.owned;
	step("launch", { ok: typeof r.pane === "number" && win(r.pane) === win(own) && owned === true, result: r });
} catch (e) {
	fail("launch", e);
}

// At observe: building is open, installing and launching are refused.
const watcher = new m.Session({ ...process.env, DREAMER_AGENT_LEVEL: "observe" });
const looking = new m.Apps(watcher, dream, apps.root);
try {
	const c = await looking.check({ dir: "hello" });
	step("observe check", { ok: c.ok === true });
} catch (e) {
	fail("observe check", e);
}
await refused("observe install", () => looking.install({ dir: "hello" }), "observes");
await refused("observe launch", () => looking.launch({ name, new_window: true }, {}), "launch apps");
watcher.close();
s.close();
