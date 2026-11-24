// Drives the pi extension's memory (the profile's extensions/dreamer/
// daemon.ts) against a real dreamerd, as pi in the harness drawer would:
// the index a new session starts with, the tools, and the chat's /memory
// command. Run by tests/pi_profile.rs after another harness remembered a
// fact in the project; prints one JSON line per step and exits non-zero on
// the first failure.
//
// argv: path to daemon.ts, the project directory, the remembered id.

const [, , modulePath, project, id] = process.argv;
const m = await import(modulePath);

const step = (name, data) => console.log(JSON.stringify({ step: name, ...data }));
const fail = (name, e) => {
	step(name, { ok: false, error: e instanceof Error ? e.message : String(e) });
	process.exit(1);
};

// pi in the drawer: an agent at the default level, its own pi session.
const pi = (session, level = "elsewhere") => {
	const s = new m.Session({ ...process.env, DREAMER_AGENT_LEVEL: level });
	s.memory = { cwd: project, session };
	return s;
};

try {
	const none = m.memoryIndex([]);
	const many = Array.from({ length: 100 }, (_, i) => ({ id: `project/m${i}`, title: "T".repeat(300), description: "h" }));
	const text = m.memoryIndex(many);
	const lines = text.split("\n").filter((l) => l.startsWith("- "));
	step("bounds", {
		ok:
			none === undefined &&
			Buffer.byteLength(text) <= m.INDEX_BYTES &&
			lines.length <= m.INDEX_LINES &&
			lines.every((l) => [...l].length <= 160) &&
			text.endsWith("more not listed; memory_recall finds them.)"),
	});
} catch (e) {
	fail("bounds", e);
}

const a = pi("pi-a");
try {
	// The new session's index lists what the other harness remembered.
	const index = await a.memoryIndex();
	step("index", { ok: index.startsWith(m.INDEX_HEAD) && index.includes(`- ${id}: `), index });
} catch (e) {
	fail("index", e);
}

let mine;
try {
	const r = await a.recall({ query: "staging port" });
	const remembered = await a.remember({ text: "Only this session cares.", kind: "project", scope: "session", title: "Session note" });
	mine = remembered.id;
	const own = await a.memoryIndex();
	const other = await pi("pi-b").memoryIndex();
	step("tools", {
		ok:
			r.items[0]?.id === id &&
			r.items[0]?.text.includes("8461") &&
			mine === "session/session-note" &&
			own.includes(mine) &&
			!other.includes(mine),
		recalled: r.items.map((i) => i.id),
		mine,
	});
} catch (e) {
	fail("tools", e);
}

try {
	// Observing, the agent may not forget what may be the user's.
	await pi("pi-a", "observe").forget(mine);
	fail("observe", new Error("an observing agent forgot a memory"));
} catch (e) {
	// FORBIDDEN, from the daemon's grant check.
	step("observe", { ok: e instanceof m.RpcError && e.code === -32005, error: e.message });
}

try {
	const list = await a.memoryCommand("");
	const shown = await a.memoryCommand(`show ${id}`);
	const edited = await a.memoryCommand(`edit ${id}`, async (t) => `${t.trim()} Edited in the chat.`);
	// Whatever keeps it (the default, sqlite, has no file): the daemon has
	// the edit.
	const kept = await a.call("memory.get", { id, context: { cwd: project } });
	step("/memory", {
		ok:
			list.includes(id) &&
			list.includes(mine) &&
			shown.includes("8461") &&
			edited.startsWith(`Updated ${id}`) &&
			kept.text.includes("Edited in the chat."),
		list,
		edited,
	});
	const forgot = await a.memoryCommand(`forget ${id}`);
	const after = await a.memoryCommand("list");
	let gone = false;
	try {
		await a.call("memory.get", { id, context: { cwd: project } });
	} catch (e) {
		gone = e instanceof m.RpcError;
	}
	step("/memory forget", {
		ok: forgot === `Forgot ${id}` && gone && !after.includes(id),
		forgot,
	});
	let unknown;
	try {
		await a.memoryCommand("frobnicate");
	} catch (e) {
		unknown = e.message;
	}
	step("/memory unknown", { ok: typeof unknown === "string" && unknown.includes("unknown") });
} catch (e) {
	fail("/memory", e);
}

a.close();
process.exit(0);
