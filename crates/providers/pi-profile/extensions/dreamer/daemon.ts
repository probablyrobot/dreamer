// The Dreamer daemon's JSON-RPC endpoint, for the pi extension.
//
// Only Node built-ins are used, and only syntax Node can strip of its types,
// so this file also runs under plain `node` (Dreamer's tests do that).
//
// One connection per pi process. It narrows its own grant to an agent's
// grant (`grants.narrow`) before any tool runs, so the daemon treats it as
// an agent: it can never answer approvals, and acting outside the panes it
// created waits for the user (docs/grants.md).

import * as fs from "node:fs";
import * as net from "node:net";
import * as path from "node:path";

/** The JSON-RPC socket: `DREAMER_RPC_SOCKET`; else beside `DREAMER_SOCKET`
 * (the native socket's path); else in the runtime directory, found the way
 * the daemon finds it. */
export function socketPath(env: Record<string, string | undefined> = process.env): string {
	const set = (k: string) => {
		const v = env[k];
		return v !== undefined && v !== "" ? v : undefined;
	};
	const rpc = set("DREAMER_RPC_SOCKET");
	if (rpc) return rpc;
	const native = set("DREAMER_SOCKET");
	if (native) {
		return native.endsWith(".rpc.sock") ? native : path.join(path.dirname(native), "dreamerd.rpc.sock");
	}
	const runtime = set("DREAMER_RUNTIME_DIR");
	if (runtime) return path.join(runtime, "dreamerd.rpc.sock");
	const xdg = set("XDG_RUNTIME_DIR");
	if (xdg) return path.join(xdg, "dreamer", "dreamerd.rpc.sock");
	const uid = typeof process.getuid === "function" ? process.getuid() : 0;
	return path.join(set("TMPDIR") ?? "/tmp", `dreamer-${uid}`, "dreamerd.rpc.sock");
}

export type Level = "observe" | "own" | "elsewhere";

/** `DREAMER_AGENT_LEVEL`, default `elsewhere` (asks before acting on the
 * user's panes). */
export function level(env: Record<string, string | undefined> = process.env): Level {
	const v = env.DREAMER_AGENT_LEVEL;
	return v === "observe" || v === "own" || v === "elsewhere" ? v : "elsewhere";
}

/** The collector presets the daemon ships (docs/collectors.md). */
export const COLLECTOR_PRESETS = ["errors", "test-failures", "commands", "urls", "paths"];

/** The methods of the agent's grant at `level`: the same set `dream mcp
 * --level` uses (collectors only read panes, so every level may create
 * and query them; stopping one is an action), plus the event stream (approvals and collector rows). */
export function agentMethods(l: Level): string[] {
	const m = [
		"tmux.topology",
		"terminal.list",
		"terminal.read",
		"terminal.query",
		"terminal.watch",
		"jobs.get",
		"jobs.list",
		"actions.get",
		"actions.list",
		"agents.runs",
		"collect.create",
		"collect.list",
		"collect.query",
		"events.subscribe",
	];
	if (l !== "observe") {
		m.push("terminal.input", "terminal.run", "actions.submit", "jobs.start", "jobs.cancel", "collect.stop");
	}
	return m;
}

/** `collect.create`'s parameters from the collector_create tool's: the
 * definition as the API takes it (docs/collectors.md). `wake` is the
 * tool's own. */
export function collectorSpec(p: Record<string, any>): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const k of ["name", "preset", "from", "when", "capture", "keep", "per_second"]) {
		if (p[k] !== undefined && p[k] !== null && p[k] !== "") out[k] = p[k];
	}
	if (out.name === undefined && out.preset === undefined) throw new Error("give a name or a preset");
	return out;
}

/** `collect.query`'s parameters from the collector_query tool's: `fields`
 * as an object of exact values, `since_s` as seconds before `now`. */
export function collectorQuery(p: Record<string, any>, now: number = Date.now()): Record<string, unknown> {
	const q: Record<string, unknown> = { name: p.name };
	if (Array.isArray(p.panes) && p.panes.length > 0) q.panes = p.panes;
	if (p.fields && typeof p.fields === "object") {
		q.fields = Object.entries(p.fields).map(([name, value]) => ({ name, value: String(value) }));
	}
	if (typeof p.since_s === "number") q.since_unix_ms = Math.max(0, now - p.since_s * 1000);
	for (const k of ["after", "limit", "group_by"]) if (p[k] !== undefined) q[k] = p[k];
	return q;
}

/** New rows of a collector this session follows, gathered for a moment. */
export type Rows = { collector: string; first: number; last: number; count: number; panes: number[] };

/** How long rows gather before the agent hears of them. */
const ROWS_MS = 1500;

/** The narrowed grant: every pane, at the level. */
export function agentGrant(l: Level): Record<string, unknown> {
	return { methods: agentMethods(l), all_panes: true, level: l };
}

/** A JSON-RPC error from the daemon. */
export class RpcError extends Error {
	code: number;
	constructor(code: number, message: string) {
		super(message);
		this.code = code;
	}
}

/** JSON-RPC: method not found. */
export const METHOD_NOT_FOUND = -32601;

type Waiter = { resolve: (v: unknown) => void; reject: (e: Error) => void };

/** A line-framed JSON-RPC 2.0 client. */
export class Client {
	private socket: net.Socket;
	private buffer = "";
	private next = 1;
	private waiting = new Map<number, Waiter>();
	private closed: Error | undefined;
	/** Notifications from the daemon: topic and params. */
	onNotification: (method: string, params: unknown) => void = () => {};
	onClose: (why: Error) => void = () => {};

	private constructor(socket: net.Socket) {
		this.socket = socket;
		socket.setEncoding("utf8");
		socket.on("data", (chunk: string) => this.read(chunk));
		const end = (why: Error) => {
			if (this.closed) return;
			this.closed = why;
			for (const w of this.waiting.values()) w.reject(why);
			this.waiting.clear();
			this.onClose(why);
		};
		socket.on("error", (e: Error) => end(e));
		socket.on("close", () => end(new Error("the Dreamer daemon closed the connection")));
	}

	/** Connect to `socket`, failing after `timeoutMs`. */
	static connect(socket: string, timeoutMs = 3000): Promise<Client> {
		return new Promise((resolve, reject) => {
			const s = net.createConnection(socket);
			const timer = setTimeout(() => {
				s.destroy();
				reject(new Error(`no answer from the Dreamer daemon at ${socket}`));
			}, timeoutMs);
			s.once("connect", () => {
				clearTimeout(timer);
				resolve(new Client(s));
			});
			s.once("error", (e: NodeJS.ErrnoException) => {
				clearTimeout(timer);
				reject(
					new Error(
						e.code === "ENOENT" || e.code === "ECONNREFUSED"
							? `the Dreamer daemon is not running (no socket at ${socket}); start it with \`dream ensure-running\``
							: `${socket}: ${e.message}`,
					),
				);
			});
		});
	}

	private read(chunk: string) {
		this.buffer += chunk;
		let nl = this.buffer.indexOf("\n");
		while (nl >= 0) {
			const line = this.buffer.slice(0, nl).trim();
			this.buffer = this.buffer.slice(nl + 1);
			nl = this.buffer.indexOf("\n");
			if (line === "") continue;
			let msg: any;
			try {
				msg = JSON.parse(line);
			} catch {
				continue;
			}
			if (msg.id === undefined || msg.id === null) {
				if (typeof msg.method === "string") this.onNotification(msg.method, msg.params);
				continue;
			}
			const w = this.waiting.get(msg.id);
			if (!w) continue;
			this.waiting.delete(msg.id);
			if (msg.error) w.reject(new RpcError(msg.error.code ?? 0, msg.error.message ?? "error"));
			else w.resolve(msg.result);
		}
	}

	/** Call `method` with `params`; its result, or an `RpcError`. */
	call(method: string, params: Record<string, unknown> = {}): Promise<any> {
		if (this.closed) return Promise.reject(this.closed);
		const id = this.next++;
		return new Promise((resolve, reject) => {
			this.waiting.set(id, { resolve, reject });
			this.socket.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
		});
	}

	close() {
		this.socket.end();
	}
}

/** A split direction as the API writes it. */
export function direction(d: string | undefined): "vertical" | "horizontal" {
	if (d === undefined || d === "vertical") return "vertical";
	if (d === "horizontal") return "horizontal";
	throw new Error(`direction \`${d}\`: use horizontal or vertical`);
}

/** Input events for `terminal.input`: typed text, a paste, then Enter. */
export function inputEvents(p: { text?: string; paste?: string; enter?: boolean }): Record<string, unknown>[] {
	const events: Record<string, unknown>[] = [];
	if (p.text !== undefined && p.text !== "") events.push({ text: { text: p.text } });
	if (p.paste !== undefined && p.paste !== "") {
		events.push({ paste: { data: Buffer.from(p.paste, "utf8").toString("base64"), more: false, id: 1 } });
	}
	if (p.enter) events.push({ key: { key: "Enter" } });
	if (events.length === 0) throw new Error("give text, paste, or enter");
	return events;
}

/** `terminal.read`'s answer as the tool returns it: the screen, or the last
 * `lines` lines of history and screen without trailing blank lines. */
export function readResult(t: any, lines: number | undefined): Record<string, unknown> {
	let out: string[] = Array.isArray(t.lines) ? t.lines.slice() : [];
	if (lines !== undefined) {
		while (out.length > 0 && out[out.length - 1].trim() === "") out.pop();
		const keep = Math.max(1, lines);
		if (out.length > keep) out = out.slice(out.length - keep);
	} else {
		out = out.slice(Math.min(t.history_lines ?? 0, out.length));
	}
	return { pane: t.pane, cols: t.cols, rows: t.rows, cursor: [t.cursor_x ?? 0, t.cursor_y ?? 0], lines: out };
}

/** The `actions.submit` kind for a layout tool. */
export function layoutKind(op: string, p: any): Record<string, unknown> {
	const one = (a: unknown, b: unknown, what: string) => {
		if ((a === undefined) === (b === undefined)) throw new Error(`give ${what}`);
	};
	switch (op) {
		case "split":
			return {
				split_pane: {
					pane: p.pane,
					direction: direction(p.direction),
					argv: p.argv ?? [],
					...(p.cwd ? { cwd: p.cwd } : {}),
				},
			};
		case "select":
			one(p.pane, p.window, "a pane or a window");
			return p.pane !== undefined ? { select_pane: { pane: p.pane } } : { select_window: { window: p.window } };
		case "rename":
			one(p.window, p.session, "a window or a session");
			return p.window !== undefined
				? { rename_window: { window: p.window, name: p.name } }
				: { rename_session: { session: p.session, name: p.name } };
		case "move":
			one(p.window, p.new_window, "a window or new_window");
			return { move_pane: { pane: p.pane, ...(p.window !== undefined ? { window: p.window } : { new_window: p.new_window }) } };
		case "zoom":
			return { zoom_pane: { pane: p.pane } };
		case "close":
			one(p.pane, p.window, "a pane or a window");
			return p.pane !== undefined ? { kill_pane: { pane: p.pane } } : { kill_window: { window: p.window } };
		default:
			throw new Error(`unknown layout action \`${op}\`; use split, select, rename, move, zoom, or close`);
	}
}

/** The CLIs a task can be handed to: pi tool name, `dream agents delegate
 * --to` value, and who it is. */
export const DELEGATES = {
	claude: { tool: "claude_code", label: "Claude Code" },
	codex: { tool: "codex", label: "Codex" },
} as const;

export type Delegate = keyof typeof DELEGATES;

/** `DREAMER_PI_DELEGATES` (set by `dream agents pi` from
 * `agents.pi.delegates`): a comma-separated list; unset means both. */
export function delegates(env: Record<string, string | undefined> = process.env): Delegate[] {
	const v = env.DREAMER_PI_DELEGATES;
	if (v === undefined) return ["claude", "codex"];
	return v
		.split(",")
		.map((x) => x.trim())
		.filter((x): x is Delegate => x === "claude" || x === "codex");
}

/** The program and arguments that hand `task` to `to`: Dreamer's
 * `dream agents delegate`, which runs the CLI headless on its own login
 * with API keys removed and writes its result to the job's artifacts. */
export function delegateArgv(dream: string, to: Delegate, task: string, opts: { cwd?: string; timeout_s?: number } = {}): string[] {
	if (task.trim() === "") throw new Error("give the task");
	const argv = [dream, "agents", "delegate", "--to", to];
	if (opts.cwd) argv.push("--cwd", opts.cwd);
	if (opts.timeout_s !== undefined) argv.push("--timeout", String(opts.timeout_s));
	argv.push("--", task);
	return argv;
}

/** How long one wait for a delegated run lasts before its state is read
 * again. */
const DELEGATE_WAIT_MS = 15_000;

/** Long enough to outlast waiting for an approval. */
const LEASE_MS = 120_000;

/** The agent's session with the daemon: one narrowed connection, its
 * interaction leases, and the approvals it is waiting for. */
export class Session {
	private client: Client | undefined;
	private connecting: Promise<Client> | undefined;
	private leases = new Map<number, string>();
	private takenOver = new Set<number>();
	/** Panes of acting calls in flight; an approval requested for one of
	 * them is this session's. */
	private acting = new Map<number, number>();
	private mine = new Set<string>();
	collectors: boolean | undefined;
	readonly socket: string;
	readonly level: Level;
	/** An approval this session waits for was requested or answered. */
	onApproval: (a: { id: string; pane?: number; what: string; answered?: string }) => void = () => {};
	/** Collectors whose new rows this session tells its agent about. */
	readonly following = new Set<string>();
	/** New rows of followed collectors, gathered for a moment. */
	onRows: (rows: Rows[]) => void = () => {};
	private gathering = new Map<string, Rows>();
	private gatherTimer: ReturnType<typeof setTimeout> | undefined;

	constructor(env: Record<string, string | undefined> = process.env) {
		this.socket = socketPath(env);
		this.level = level(env);
	}

	/** The connection, made and narrowed on first use. */
	connection(): Promise<Client> {
		if (this.client) return Promise.resolve(this.client);
		if (!this.connecting) {
			this.connecting = (async () => {
				const c = await Client.connect(this.socket);
				try {
					await c.call("grants.narrow", agentGrant(this.level));
				} catch (e) {
					c.close();
					throw e;
				}
				c.onClose = () => {
					this.client = undefined;
					this.leases.clear();
				};
				c.onNotification = (method, params) => this.notified(method, params);
				// Approvals are shown, never answered (this grant cannot);
				// and collector rows wake the agent when it follows them.
				await c.call("events.subscribe", { kinds: ["approvals", "collect.row"] }).catch(() => {});
				try {
					await c.call("collect.list", {});
					this.collectors = true;
				} catch (e) {
					this.collectors = !(e instanceof RpcError && e.code === METHOD_NOT_FOUND);
				}
				this.client = c;
				return c;
			})().finally(() => {
				this.connecting = undefined;
			});
		}
		return this.connecting;
	}

	private notified(method: string, params: any) {
		if (method !== "events.batch") return;
		for (const e of params?.events ?? []) {
			if (e.kind === "collect.row") {
				this.row(String(e.subject ?? ""), Number(e.state ?? 0), e.pane);
				continue;
			}
			const id = String(e.subject ?? "");
			if (e.kind === "approvals.requested") {
				const pane = typeof e.pane === "number" ? e.pane : undefined;
				const ours = pane === undefined ? this.acting.size > 0 : this.acting.has(pane);
				if (!ours) continue;
				this.mine.add(id);
				this.onApproval({ id, pane, what: String(e.state ?? "") });
			} else if (e.kind === "approvals.answered" && this.mine.has(id)) {
				this.mine.delete(id);
				this.onApproval({ id, pane: e.pane, what: "", answered: String(e.state ?? "") });
			}
		}
	}

	/** A row of `collector`: gathered, then told in one batch. */
	private row(collector: string, id: number, pane: unknown) {
		if (!this.following.has(collector)) return;
		const g = this.gathering.get(collector) ?? { collector, first: id, last: id, count: 0, panes: [] };
		g.first = Math.min(g.first, id);
		g.last = Math.max(g.last, id);
		g.count += 1;
		if (typeof pane === "number" && !g.panes.includes(pane)) g.panes.push(pane);
		this.gathering.set(collector, g);
		if (this.gatherTimer) return;
		this.gatherTimer = setTimeout(() => {
			this.gatherTimer = undefined;
			const rows = [...this.gathering.values()];
			this.gathering.clear();
			if (rows.length > 0) this.onRows(rows);
		}, ROWS_MS);
	}

	async call(method: string, params: Record<string, unknown> = {}): Promise<any> {
		return (await this.connection()).call(method, params);
	}

	/** An acting call on `pane`: approvals requested meanwhile are ours. */
	async acting_on<T>(pane: number | undefined, f: () => Promise<T>): Promise<T> {
		const key = pane ?? -1;
		this.acting.set(key, (this.acting.get(key) ?? 0) + 1);
		try {
			return await f();
		} finally {
			const n = (this.acting.get(key) ?? 1) - 1;
			if (n <= 0) this.acting.delete(key);
			else this.acting.set(key, n);
		}
	}

	/** Type into a pane under this session's lease on it, taken on first
	 * use. A person typing there revokes it; the session then stops. */
	async send(pane: number, events: Record<string, unknown>[]): Promise<{ pane: number; bytes: number }> {
		if (this.takenOver.has(pane)) {
			throw new Error(`a person typed into pane %${pane} while this agent held it; ask the user before typing there again`);
		}
		let sent = 0;
		let renewed = false;
		for (let i = 0; i < events.length; ) {
			let lease = this.leases.get(pane);
			if (!lease) {
				const l = await this.call("leases.acquire", { pane, ttl_ms: LEASE_MS });
				lease = String(l.id);
				this.leases.set(pane, lease);
			}
			try {
				const r = await this.acting_on(pane, () =>
					this.call("terminal.input", { pane, lease, event: events[i] }),
				);
				sent += Number(r?.bytes ?? 0);
				i++;
			} catch (e) {
				this.leases.delete(pane);
				const why = e instanceof Error ? e.message : String(e);
				if (why.includes("a person typed")) {
					this.takenOver.add(pane);
					throw new Error(`${why}. A person took over pane %${pane}; ask the user before typing there again`);
				}
				if (!renewed && why.includes("expired")) {
					renewed = true;
					continue;
				}
				throw e;
			}
		}
		return { pane, bytes: sent };
	}

	/** One layout action; an error unless it completed. */
	async layout(op: string, p: any): Promise<any> {
		const kind = layoutKind(op, p);
		const pane = typeof p.pane === "number" ? p.pane : undefined;
		const r = await this.acting_on(pane, () => this.call("actions.submit", { action: { kind } }));
		if (r?.state !== "completed") {
			throw new Error(`${op}: ${r?.state ?? "no answer"}${r?.reason ? `: ${r.reason}` : ""}`);
		}
		return r;
	}

	/** Hand `task` to `to` as a job in a new window this session owns (so
	 * the user can watch it), wait for it to end, and return the result it
	 * wrote: the answer, or why there is none (a missing or logged-out CLI
	 * is reported, never replaced). `signal` cancels the job. */
	async delegate(
		dream: string,
		to: Delegate,
		task: string,
		opts: { cwd?: string; timeout_s?: number } = {},
		signal?: AbortSignal,
	): Promise<Record<string, unknown>> {
		const argv = delegateArgv(dream, to, task, opts);
		const r = await this.acting_on(undefined, () =>
			this.call("terminal.run", {
				argv,
				...(opts.cwd ? { cwd: opts.cwd } : {}),
				name: DELEGATES[to].tool,
				timeout_ms: DELEGATE_WAIT_MS,
				lines: 40,
			}),
		);
		const job = r?.job;
		if (!job || typeof job.pane !== "number") throw new Error("terminal.run started no job");
		let output: string[] = Array.isArray(r.output) ? r.output : [];
		let finished = r.finished === true;
		while (!finished) {
			if (signal?.aborted) {
				await this.call("jobs.cancel", { id: job.id }).catch(() => {});
				throw new Error(`cancelled; the ${DELEGATES[to].label} run in pane %${job.pane} was stopped`);
			}
			const j = await this.call("jobs.get", { id: job.id });
			if (j?.state !== "running") break;
			const w = await this.call("terminal.watch", { pane: job.pane, exit: true, timeout_ms: DELEGATE_WAIT_MS });
			if (w?.outcome === "closed") break;
		}
		let result: Record<string, unknown>;
		const file = job.artifacts ? path.join(job.artifacts, "delegate.json") : "";
		try {
			result = JSON.parse(fs.readFileSync(file, "utf8"));
		} catch {
			if (!finished) {
				try {
					output = readResult(await this.call("terminal.read", { pane: job.pane, history: 40 }), 40).lines as string[];
				} catch {
					// The pane may be gone.
				}
			}
			throw new Error(
				`${DELEGATES[to].label} left no result (pane %${job.pane}); its last output:\n${output.slice(-20).join("\n")}`,
			);
		}
		return { ...result, pane: job.pane };
	}

	close() {
		if (this.gatherTimer) clearTimeout(this.gatherTimer);
		this.gatherTimer = undefined;
		this.client?.close();
		this.client = undefined;
	}
}
