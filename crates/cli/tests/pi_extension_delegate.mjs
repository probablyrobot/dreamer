// Drives the pi extension's delegation (extensions/dreamer/daemon.ts)
// against a stand-in daemon: the session's calls are recorded and answered
// here, so no daemon, CLI, or model is involved. Run by tests/pi_profile.rs;
// prints one JSON line per step and exits non-zero on the first failure.
//
// argv: path to daemon.ts, a scratch directory.

const [, , modulePath, dir] = process.argv;
const m = await import(modulePath);
const fs = await import("node:fs");
const path = await import("node:path");

const step = (name, data) => console.log(JSON.stringify({ step: name, ...data }));
const fail = (name, why) => {
	step(name, { ok: false, error: why instanceof Error ? why.message : String(why) });
	process.exit(1);
};
const check = (name, cond, data = {}) => (cond ? step(name, { ok: true, data }) : fail(name, JSON.stringify(data)));

// Which delegates are offered.
check("delegates-default", JSON.stringify(m.delegates({})) === '["claude","codex"]');
check("delegates-some", JSON.stringify(m.delegates({ DREAMER_PI_DELEGATES: "codex" })) === '["codex"]');
check("delegates-none", m.delegates({ DREAMER_PI_DELEGATES: "" }).length === 0);
check("tool-names", m.DELEGATES.claude.tool === "claude_code" && m.DELEGATES.codex.tool === "codex");

// The command: `dream agents delegate`, the task after `--`, verbatim.
const argv = m.delegateArgv("/opt/dream", "claude", "--to codex; rm -rf /", { cwd: "/w", timeout_s: 60 });
check("argv", JSON.stringify(argv) === JSON.stringify(["/opt/dream", "agents", "delegate", "--to", "claude", "--cwd", "/w", "--timeout", "60", "--", "--to codex; rm -rf /"]), { argv });
try {
	m.delegateArgv("dream", "codex", "  ");
	fail("empty-task", "an empty task was accepted");
} catch {
	step("empty-task", { ok: true });
}

/** A session whose daemon is this script: `answers[method]` answers. */
const standIn = (answers) => {
	const s = new m.Session({ DREAMER_RPC_SOCKET: path.join(dir, "none.sock") });
	const calls = [];
	s.call = async (method, params) => {
		calls.push({ method, params });
		const a = answers[method];
		if (a === undefined) throw new Error(`unexpected call ${method}`);
		return typeof a === "function" ? a(params) : a;
	};
	return { s, calls };
};

// A run that outlasts terminal.run's wait: the job is followed until it
// ends, then its result file is read.
const artifacts = path.join(dir, "job1");
fs.mkdirSync(artifacts, { recursive: true });
let polls = 0;
{
	const { s, calls } = standIn({
		"terminal.run": { finished: false, job: { id: "j1", pane: 7, artifacts }, output: ["[claude, billed to your own Claude Code login]"] },
		"jobs.get": () => {
			polls += 1;
			if (polls === 2) {
				fs.writeFileSync(
					path.join(artifacts, "delegate.json"),
					JSON.stringify({ delegate: "claude", ok: true, answer: "Forty-two.", tools: { calls: 1, failed: 0, summary: "Read" } }),
				);
			}
			return { id: "j1", pane: 7, state: polls >= 2 ? "succeeded" : "running" };
		},
		"terminal.watch": { outcome: "timeout" },
	});
	const r = await s.delegate("/opt/dream", "claude", "What is the answer?", { cwd: "/w" });
	const run = calls[0];
	check("run-request", run.method === "terminal.run" && run.params.argv[0] === "/opt/dream" && run.params.name === "claude_code" && run.params.cwd === "/w" && run.params.split === undefined, run);
	check("followed", calls.some((c) => c.method === "terminal.watch" && c.params.pane === 7 && c.params.exit === true), { calls: calls.map((c) => c.method) });
	check("answer", r.ok === true && r.answer === "Forty-two." && r.pane === 7 && r.tools.summary === "Read", r);
}

// A logged-out CLI: the result says so, and nothing else is tried.
const artifacts2 = path.join(dir, "job2");
fs.mkdirSync(artifacts2, { recursive: true });
fs.writeFileSync(
	path.join(artifacts2, "delegate.json"),
	JSON.stringify({ delegate: "codex", ok: false, error: "codex is not logged in", state: "logged out (log in with the CLI itself)" }),
);
{
	const { s, calls } = standIn({
		"terminal.run": { finished: true, job: { id: "j2", pane: 8, artifacts: artifacts2 }, output: [] },
	});
	const r = await s.delegate("dream", "codex", "Fix it");
	check("logged-out", r.ok === false && /not logged in/.test(r.error) && calls.length === 1 && calls[0].params.name === "codex", r);
}

// No result file: the error carries the pane's last output.
{
	const { s } = standIn({
		"terminal.run": { finished: true, job: { id: "j3", pane: 9, artifacts: path.join(dir, "job3") }, output: ["dream: crashed"] },
	});
	try {
		await s.delegate("dream", "codex", "Fix it");
		fail("no-result", "no error");
	} catch (e) {
		check("no-result", /pane %9/.test(e.message) && /crashed/.test(e.message), { error: e.message });
	}
}

// Cancelling stops the job.
{
	const ac = new AbortController();
	ac.abort();
	const { s, calls } = standIn({
		"terminal.run": { finished: false, job: { id: "j4", pane: 10, artifacts: path.join(dir, "job4") }, output: [] },
		"jobs.cancel": {},
	});
	try {
		await s.delegate("dream", "claude", "Long task", {}, ac.signal);
		fail("cancel", "no error");
	} catch (e) {
		check("cancel", calls.some((c) => c.method === "jobs.cancel" && c.params.id === "j4"), { error: e.message });
	}
}
