// Drives the pi extension's daemon client (the profile's
// extensions/dreamer/daemon.ts) against a real dreamerd. Run by
// tests/pi_profile.rs with the daemon's environment; prints one JSON line
// per step and exits non-zero on the first failure.
//
// argv: path to daemon.ts, the pane to act on (one this client did not
// create), and the path of a file whose existence asks the test to deny the
// approval this script then waits for.

const [, , modulePath, paneArg, signal] = process.argv;
const m = await import(modulePath);
const pane = Number(paneArg);
const fs = await import("node:fs");

const step = (name, data) => console.log(JSON.stringify({ step: name, ...data }));
const fail = (name, e) => {
	step(name, { ok: false, error: e instanceof Error ? e.message : String(e) });
	process.exit(1);
};

const s = new m.Session(process.env);
const approvals = [];
s.onApproval = (a) => approvals.push(a);

try {
	const list = await s.call("terminal.list", {});
	step("list", { ok: true, panes: (list.terminals ?? []).map((t) => t.pane) });
} catch (e) {
	fail("list", e);
}

try {
	const t = await s.call("terminal.read", { pane });
	step("read", { ok: true, ...m.readResult(t, undefined) });
} catch (e) {
	fail("read", e);
}

try {
	const g = await s.call("grants.current", {});
	step("grant", { ok: false, grant: g });
	process.exit(1);
} catch (e) {
	// The narrowed grant does not include grants.current: it is an agent's.
	step("grant", { ok: true, refused: e.message });
}

try {
	await s.call("approvals.list", {});
	fail("approvals.list", "an agent listed approvals");
} catch (e) {
	step("approvals.list", { ok: true, refused: e.message });
}

// Typing into a pane this client did not create waits for the user.
const typing = s.send(pane, m.inputEvents({ text: "echo from-agent", enter: true })).then(
	(r) => ({ ok: true, r }),
	(e) => ({ ok: false, error: e.message }),
);
const deadline = Date.now() + 10000;
while (approvals.length === 0 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 50));
if (approvals.length === 0) fail("approval", "no approval was shown");
step("approval", { ok: true, approval: approvals[0] });
fs.writeFileSync(signal, approvals[0].id);
const sent = await typing;
step("send", { ok: !sent.ok, denied: sent.error });
const until = Date.now() + 5000;
while (!approvals.some((a) => a.answered) && Date.now() < until) await new Promise((r) => setTimeout(r, 50));
const answered = approvals.find((a) => a.answered);
step("answered", { ok: answered?.answered === "deny", approval: answered });

// A window the client opened is its own: typing there asks nobody.
try {
	const r = await s.call("terminal.run", { argv: ["/bin/sh"], name: "agent-own", timeout_ms: 300 });
	const own = r.job?.pane;
	step("run", { ok: typeof own === "number" && r.finished === false, pane: own });
	const before = approvals.length;
	await s.send(own, m.inputEvents({ text: "echo own-pane", enter: true }));
	step("own", { ok: approvals.length === before });
	// Renaming that window is acting on its own pane too.
	const list = await s.call("terminal.list", {});
	const info = (list.terminals ?? []).find((t) => t.pane === own);
	step("owned", { ok: info?.owned === true });
	const renamed = await s.layout("rename", { window: info.window, name: "renamed-by-agent" });
	step("rename", { ok: renamed.state === "completed" && approvals.length === before });
} catch (e) {
	fail("own", e);
}
s.close();
