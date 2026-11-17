// Drives the pi extension's pane context and send_to_pane (the profile's
// extensions/dreamer/daemon.ts) against a real dreamerd, as pi in a harness
// drawer would. Run by tests/pi_profile.rs with the daemon's environment;
// prints one JSON line per step and exits non-zero on the first failure.
//
// A second, unnarrowed connection plays the user: it shows the drawer,
// types into the pane, and answers the approval.
//
// argv: path to daemon.ts, the user's pane (one the agent did not create).

const [, , modulePath, paneArg] = process.argv;
const m = await import(modulePath);
const pane = Number(paneArg);

const step = (name, data) => console.log(JSON.stringify({ step: name, ...data }));
const fail = (name, e) => {
	step(name, { ok: false, error: e instanceof Error ? e.message : String(e) });
	process.exit(1);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, f, ms = 10000) => {
	const deadline = Date.now() + ms;
	for (;;) {
		const v = await f();
		if (v) return v;
		if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
		await sleep(50);
	}
};

const user = await m.Client.connect(m.socketPath(process.env));
const screen = async (p) => (await user.call("terminal.read", { pane: p })).lines.join("\n");
const type = (p, text, enter = true) =>
	user.call("terminal.input", { pane: p, event: { text: { text } } }).then(() =>
		enter ? user.call("terminal.input", { pane: p, event: { key: { key: "Enter" } } }) : undefined,
	);

let session;
try {
	await type(pane, "echo PANE''MARK");
	await until("the mark", async () => (await screen(pane)).includes("PANEMARK"));
	const list = await user.call("terminal.list", {});
	session = list.terminals.find((t) => t.pane === pane).session;
	const d = await user.call("drawer.toggle", { session, want: "show" });
	step("drawer", { ok: d.shown === true && d.from_pane === pane, drawer: d });
} catch (e) {
	fail("drawer", e);
}

// pi in that drawer: DREAMER_DRAWER names the session.
const s = new m.Session({ ...process.env, DREAMER_DRAWER: `$${session}` });
const approvals = [];
s.onApproval = (a) => approvals.push(a);

try {
	const r = await s.expand("what failed in @pane? (not me@example.com, not @nobody)");
	const ok =
		r.attached.length === 1 &&
		r.attached[0] === "@pane" &&
		r.text.startsWith("what failed in @pane?") &&
		r.text.includes(`Pane %${pane}`) &&
		r.text.includes("PANEMARK") &&
		r.text.includes("```text") &&
		!r.text.includes("@nobody\n");
	step("@pane", { ok, result: r });
} catch (e) {
	fail("@pane", e);
}

try {
	const r = await s.expand(`and @%${pane}`);
	step("@%N", { ok: r.attached[0] === `@%${pane}` && r.text.includes("PANEMARK"), result: r });
	const none = await s.expand("nothing here");
	step("plain", { ok: none.text === "nothing here" && none.attached.length === 0 });
	// Without command marks, `/pane last` falls back to the visible text.
	const last = await s.paneContext(pane, true);
	step("last", { ok: last.includes("PANEMARK") && last.includes("OSC 133"), text: last });
} catch (e) {
	fail("@%N", e);
}

// A collector by name: its newest rows.
try {
	await s.call("collect.create", m.collectorSpec({ preset: "errors" }));
	await type(pane, "printf 'error: BOOM''MARK\\n'");
	await until("an error row", async () => (await s.call("collect.query", { name: "errors", limit: 5 })).rows?.length > 0);
	const r = await s.expand("why @errors.");
	step("@errors", {
		ok: r.attached[0] === "@errors" && r.text.includes("Collector `errors`") && r.text.includes("BOOMMARK"),
		result: r,
	});
} catch (e) {
	fail("@errors", e);
}

// send_to_pane into the user's pane: asks, then pastes without Enter. The
// user allows it for the pane, so what follows is the lease alone.
try {
	const sent = s.send(pane, m.sendToPaneEvents({ code: "```sh\necho SENT''MARK\n```" }));
	await until("an approval", async () => approvals.length > 0);
	await user.call("approvals.answer", { id: approvals[0].id, decision: "pane" });
	const r = await sent;
	const text = await until("the sent text", async () => {
		const t = await screen(pane);
		return t.includes("echo SENT''MARK") ? t : undefined;
	});
	// No Enter: the command was typed, not run.
	step("send", { ok: r.bytes > 0 && !text.includes("\nSENTMARK"), result: r });
} catch (e) {
	fail("send", e);
}

// The user types in that pane: the agent's lease ends, and it stops.
try {
	await type(pane, "x", false);
	const again = await s.send(pane, m.sendToPaneEvents({ code: "echo AGAIN" })).then(
		() => "sent",
		(e) => e.message,
	);
	step("revoked", { ok: again.includes("a person typed") && approvals.filter((a) => !a.answered).length === 1, error: again });
	// And it stays stopped: the next try is refused without reaching the pane.
	const third = await s.send(pane, m.sendToPaneEvents({ code: "echo AGAIN" })).then(
		() => "sent",
		(e) => e.message,
	);
	step("stopped", { ok: third.includes("ask the user"), error: third });
	const t = await screen(pane);
	step("not sent", { ok: !t.includes("AGAIN") });
} catch (e) {
	fail("revoked", e);
}

s.close();
user.close();
