// The Dreamer daemon's JSON-RPC endpoint, for the pi extension.
//
// Only Node built-ins are used, and only syntax Node can strip of its types,
// so this file also runs under plain `node` (Dreamer's tests do that).
//
// One connection per pi process. It narrows its own grant to an agent's
// grant (`grants.narrow`) before any tool runs, so the daemon treats it as
// an agent: it can never answer approvals, and acting outside the panes it
// created waits for the user (docs/grants.md). In an agent session the
// daemon runs (`DREAMER_SESSION_ID` and `DREAMER_SESSION_TICKET`), it
// takes that session's grant with `apps.register` instead.

import { execFile } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
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
 * and query them; stopping one is an action), plus the event stream
 * (approvals and collector rows) and `drawer.status` (the pane the drawer
 * came from). */
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
		"memory.recall",
		"memory.list",
		"memory.get",
		"memory.remember",
		"events.subscribe",
	];
	// Which pane the drawer came from, for `@pane` and `/pane`.
	m.push("drawer.status");
	if (l !== "observe") {
		m.push("terminal.input", "terminal.run", "actions.submit", "jobs.start", "jobs.cancel", "collect.stop", "memory.update", "memory.forget");
		// Opening the apps it builds and looking at the app panes it opened.
		m.push("apps.launch", "apps.tree", "apps.capture", "daemon.status", "daemon.version");
		// Opening pages in the browser, and loading another address in one.
		m.push("web.open", "web.navigate");
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

/** Bounds of one attachment: its newest lines and bytes. */
export const ATTACH_LINES = 200;
export const ATTACH_BYTES = 16 * 1024;

/** The newest `maxLines` lines of `lines`, without trailing blank lines,
 * then fewer until they fit in `maxBytes` (a single longer line keeps its
 * end). `cut` says whether anything was left out. */
export function bounded(
	lines: string[],
	maxLines: number = ATTACH_LINES,
	maxBytes: number = ATTACH_BYTES,
): { lines: string[]; cut: boolean; total: number } {
	let out = lines.slice();
	while (out.length > 0 && out[out.length - 1].trim() === "") out.pop();
	const total = out.length;
	let cut = false;
	if (out.length > maxLines) {
		out = out.slice(out.length - maxLines);
		cut = true;
	}
	let bytes = out.reduce((n, l) => n + Buffer.byteLength(l, "utf8") + 1, 0);
	while (out.length > 1 && bytes > maxBytes) {
		bytes -= Buffer.byteLength(out[0], "utf8") + 1;
		out.shift();
		cut = true;
	}
	if (out.length === 1 && Buffer.byteLength(out[0], "utf8") > maxBytes) {
		const b = Buffer.from(out[0], "utf8");
		// Keep the end; a character split at the cut becomes U+FFFD.
		out[0] = b.subarray(b.length - maxBytes).toString("utf8");
		cut = true;
	}
	return { lines: out, cut, total };
}

/** A fence of backticks longer than any run of backticks in `text`. */
export function fenceFor(text: string): string {
	let longest = 0;
	for (const m of text.matchAll(/`+/g)) longest = Math.max(longest, m[0].length);
	return "`".repeat(Math.max(3, longest + 1));
}

/** One attachment: a label line, then the bounded text, fenced. */
export function attachment(label: string, lines: string[]): string {
	const b = bounded(lines);
	const body = b.lines.join("\n");
	const fence = fenceFor(body);
	const note = b.cut ? ` (cut to the last ${b.lines.length} of ${b.total} lines)` : "";
	return `${label}${note}:\n${fence}text\n${body}\n${fence}`;
}

/** A context reference in a prompt: `@pane` (the pane the drawer came
 * from), `@%N` (pane N), or `@name` (a collector, if one has that name). */
export type ContextRef = { token: string; pane?: number; from?: boolean; collector?: string };

/** The `@` references in `text`, each once, in order. An `@` counts only at
 * the start or after a space, a quote, or an opening bracket, so mail
 * addresses are left alone; trailing dots are not part of a name. */
export function contextRefs(text: string): ContextRef[] {
	const out: ContextRef[] = [];
	const seen = new Set<string>();
	for (const m of text.matchAll(/(^|[\s(\[{"'])@(%\d+|[A-Za-z0-9_][A-Za-z0-9_.-]*)/g)) {
		const word = m[2].replace(/[.]+$/, "");
		const token = `@${word}`;
		if (word === "" || seen.has(token)) continue;
		seen.add(token);
		if (word === "pane") out.push({ token, from: true });
		else if (word.startsWith("%")) out.push({ token, pane: Number(word.slice(1)) });
		else out.push({ token, collector: word });
	}
	return out;
}

/** `DREAMER_DRAWER` (`$N`, set in the harness drawer): its session id. */
export function drawerSession(env: Record<string, string | undefined> = process.env): number | undefined {
	const m = /^\$(\d+)$/.exec(env.DREAMER_DRAWER ?? "");
	return m ? Number(m[1]) : undefined;
}

/** How a pane is named in an attachment's label. */
export function paneLabel(pane: number, info: any): string {
	const parts: string[] = [];
	if (info?.title) parts.push(`title "${info.title}"`);
	if (info?.command) parts.push(`running ${info.command}`);
	if (info?.cwd) parts.push(`in ${info.cwd}`);
	return `Pane %${pane}${parts.length > 0 ? ` (${parts.join(", ")})` : ""}`;
}

/** A collector row as text: one line of metadata, then its lines. */
export function rowLines(r: any): string[] {
	const when = r.time_unix_ms ? new Date(Number(r.time_unix_ms)).toISOString().replace(/\.\d+Z$/, "Z") : "";
	const fields = (r.fields ?? []).map((f: any) => `${f.name}=${f.value}`).join(" ");
	const status = r.exit_status !== undefined && r.exit_status !== null ? `exit ${r.exit_status}` : "";
	const head = [`#${r.id}`, when, `%${r.pane}`, r.command ? `[${r.command}]` : "", status, fields]
		.filter((x) => x !== "")
		.join(" ");
	const out = [head];
	for (const l of r.before ?? []) out.push(`    ${l}`);
	if (r.line) out.push(`  > ${r.line}`);
	for (const l of r.after ?? []) out.push(`    ${l}`);
	if (!r.line) for (const l of r.lines ?? []) out.push(`    ${l}`);
	return out;
}

/** `send_to_pane`'s input: `code` without a Markdown fence around it, as a
 * paste, then Enter only when asked. */
export function sendToPaneEvents(p: { code: string; enter?: boolean }): Record<string, unknown>[] {
	let code = p.code;
	const m = /^\s*(`{3,}|~{3,})[^\n]*\n([\s\S]*?)\n?\1\s*$/.exec(code);
	if (m) code = m[2];
	if (code === "") throw new Error("give the code to type");
	return inputEvents({ paste: code, enter: p.enter === true });
}

/** The newest rows a collector attachment shows. */
export const ATTACH_ROWS = 20;

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

/** Memory's scopes and kinds (docs/memory.md). */
export const MEMORY_SCOPES = ["user", "host", "project", "session"];
export const MEMORY_KINDS = ["user", "feedback", "project", "reference"];

/** The memory index's bounds: the same as the daemon's. */
export const INDEX_LINES = 40;
export const INDEX_BYTES = 4096;
const INDEX_LINE_CHARS = 160;

/** What the memory index says before its lines: the same text the daemon
 * puts before a session's first prompt. */
export const INDEX_HEAD =
	"Dreamer memory: notes kept from earlier sessions for this user, this host, this project, and this session (newest first: id, title, and what it is about). Read one in full with memory_recall, and keep a fact worth having in a later session with memory_remember (from a shell: `dream memory recall WORDS`, `dream memory remember`).";

/** One memory's index line: id, title, and its one-line hook. */
export function memoryLine(m: any): string {
	const title = String(m?.title ?? "").trim();
	const hook = String(m?.description ?? "").trim();
	let l = (hook === "" || hook === title ? `- ${m?.id}: ${title}` : `- ${m?.id}: ${title} — ${hook}`).replace(/[\r\n]/g, " ");
	const chars = [...l];
	if (chars.length > INDEX_LINE_CHARS) l = chars.slice(0, INDEX_LINE_CHARS - 1).join("") + "…";
	return l;
}

/** The memory index of `items` (newest first), bounded like the daemon's;
 * undefined when there are none. */
export function memoryIndex(items: any[]): string | undefined {
	if (!Array.isArray(items) || items.length === 0) return undefined;
	let out = INDEX_HEAD;
	let listed = 0;
	for (const m of items.slice(0, INDEX_LINES)) {
		const l = memoryLine(m);
		if (Buffer.byteLength(out) + 1 + Buffer.byteLength(l) > INDEX_BYTES - 80) break;
		out += `\n${l}`;
		listed++;
	}
	const rest = items.length - listed;
	if (rest > 0) out += `\n(${rest} more not listed; memory_recall finds them.)`;
	return out;
}

/** A memory in full, as a tool result or the chat shows it. */
export function memoryText(m: any): string {
	const head = `${m.id} (${m.kind}): ${m.title}`;
	const desc = m.description && m.description !== m.title ? `\n${m.description}` : "";
	const links = Array.isArray(m.links) && m.links.length > 0 ? `\nlinks: ${m.links.join(", ")}` : "";
	const file = m.path ? `\nfile: ${m.path}` : "";
	return `${head}${desc}${links}${file}\n\n${m.text}`;
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
	/** The tmux session whose harness drawer this runs in, if it does. */
	readonly drawer: number | undefined;
	/** An approval this session waits for was requested or answered. */
	onApproval: (a: { id: string; pane?: number; what: string; answered?: string }) => void = () => {};
	/** Collectors whose new rows this session tells its agent about. */
	readonly following = new Set<string>();
	/** New rows of followed collectors, gathered for a moment. */
	onRows: (rows: Rows[]) => void = () => {};
	private gathering = new Map<string, Rows>();
	private gatherTimer: ReturnType<typeof setTimeout> | undefined;

	/** Inside an agent session the daemon runs: its id and ticket file. */
	private readonly agentSession: { id: string; ticket: string } | undefined;
	/** Memory's context: the project's directory and the session id. */
	memory: { cwd: string; session: string };

	constructor(env: Record<string, string | undefined> = process.env) {
		this.socket = socketPath(env);
		this.level = level(env);
		this.drawer = drawerSession(env);
		const id = env.DREAMER_SESSION_ID ?? "";
		const ticket = env.DREAMER_SESSION_TICKET ?? "";
		this.agentSession = id && ticket ? { id, ticket } : undefined;
		this.memory = { cwd: process.cwd(), session: id };
	}

	/** In an agent session the daemon runs, which gets the memory index
	 * with its first prompt from the daemon. */
	get daemonSession(): boolean {
		return this.agentSession !== undefined;
	}

	/** The pane that had focus before the drawer was shown or focused. */
	async fromPane(): Promise<number> {
		if (this.drawer === undefined) {
			throw new Error("@pane is the pane the harness drawer came from, and this pi is not running in a drawer; name a pane with @%N");
		}
		const s = await this.call("drawer.status", { session: this.drawer });
		if (typeof s?.from_pane !== "number") {
			throw new Error("the drawer has no pane it came from (that pane is gone); name a pane with @%N");
		}
		return s.from_pane;
	}

	private async paneInfo(pane: number): Promise<any> {
		const list = await this.call("terminal.list", {});
		return (list?.terminals ?? []).find((t: any) => t.pane === pane);
	}

	/** A pane as an attachment: its visible text, or with `last` its last
	 * finished command and output as a command-end collector recorded it
	 * (the shell's OSC 133 marks), else its visible text. */
	async paneContext(pane: number, last = false): Promise<string> {
		const info = await this.paneInfo(pane);
		if (!info) throw new Error(`no pane %${pane}`);
		const label = paneLabel(pane, info);
		if (last) {
			const row = await this.lastCommand(pane);
			if (row) {
				const status = row.exit_status !== undefined && row.exit_status !== null ? `, exit ${row.exit_status}` : "";
				return attachment(`${label}, its last command \`${row.command ?? "?"}\`${status}`, row.lines ?? []);
			}
		}
		const t = await this.call("terminal.read", { pane });
		const lines = readResult(t, undefined).lines as string[];
		const why = last ? "visible text; no command-end collector has recorded a command there (it needs the shell's OSC 133 marks)" : "visible text";
		return attachment(`${label}, ${why}`, lines);
	}

	/** The newest row any running command-end collector has for `pane`. */
	private async lastCommand(pane: number): Promise<any | undefined> {
		let list: any;
		try {
			list = await this.call("collect.list", {});
		} catch {
			return undefined;
		}
		let best: any;
		for (const c of list?.collectors ?? []) {
			if (c?.spec?.when?.command_end !== true) continue;
			try {
				const q = await this.call("collect.query", { name: c.name, panes: [pane], limit: 1 });
				const r = q?.rows?.[q.rows.length - 1];
				if (r && (!best || Number(r.time_unix_ms) > Number(best.time_unix_ms))) best = r;
			} catch {
				// A collector stopped meanwhile.
			}
		}
		return best;
	}

	/** A collector's newest rows as an attachment. */
	async collectorContext(name: string): Promise<string> {
		const q = await this.call("collect.query", { name, limit: ATTACH_ROWS });
		const rows: any[] = q?.rows ?? [];
		const lines = rows.flatMap(rowLines);
		const more = q?.truncated ? `, the newest ${rows.length} of ${q.matched}` : "";
		return attachment(`Collector \`${name}\`: ${rows.length} row(s)${more}`, lines.length > 0 ? lines : ["(no rows)"]);
	}

	/** The `@` references of a prompt as attachments, appended after it.
	 * `@name` counts only when a collector has that name; anything that
	 * cannot be attached is said in `problems` and left as typed. */
	async expand(text: string): Promise<{ text: string; attached: string[]; problems: string[] }> {
		const refs = contextRefs(text);
		const attached: string[] = [];
		const problems: string[] = [];
		if (refs.length === 0) return { text, attached, problems };
		let collectors: Set<string> | undefined;
		const blocks: string[] = [];
		for (const r of refs) {
			try {
				if (r.collector !== undefined) {
					if (!collectors) {
						const list = await this.call("collect.list", {}).catch(() => undefined);
						collectors = new Set((list?.collectors ?? []).map((c: any) => String(c.name)));
					}
					if (!collectors.has(r.collector)) continue;
					blocks.push(await this.collectorContext(r.collector));
				} else {
					const pane = r.from ? await this.fromPane() : (r.pane as number);
					blocks.push(await this.paneContext(pane));
				}
				attached.push(r.token);
			} catch (e) {
				problems.push(`${r.token}: ${e instanceof Error ? e.message : String(e)}`);
			}
		}
		if (blocks.length === 0) return { text, attached, problems };
		return { text: `${text}\n\n${blocks.join("\n\n")}`, attached, problems };
	}

	/** The connection, made and narrowed on first use. */
	connection(): Promise<Client> {
		if (this.client) return Promise.resolve(this.client);
		if (!this.connecting) {
			this.connecting = (async () => {
				const c = await Client.connect(this.socket);
				try {
					if (this.agentSession) {
						// pi in an agent session the daemon runs: the
						// session's own grant, traded for its ticket.
						const ticket = fs.readFileSync(this.agentSession.ticket, "utf8").trim();
						await c.call("apps.register", { launch_id: `session:${this.agentSession.id}`, ticket });
					} else {
						await c.call("grants.narrow", agentGrant(this.level));
					}
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

	/** A page in the browser: a new pane beside `beside` (default the pane
	 * pi runs in) or in a new window, or with `pane` another address in
	 * that web pane. The daemon makes a typed address a URL. */
	async browser(
		p: { url: string; pane?: number; beside?: number; new_window?: boolean },
		env: Record<string, string | undefined> = process.env,
	): Promise<Record<string, unknown>> {
		if (p.pane !== undefined) {
			await this.acting_on(p.pane, () => this.call("web.navigate", { pane: p.pane, url: p.url }));
			return { pane: p.pane, url: p.url };
		}
		const own = /^%(\d+)$/.exec(env.TMUX_PANE ?? "");
		const beside = p.new_window ? undefined : (p.beside ?? (own ? Number(own[1]) : undefined));
		let params: Record<string, unknown> = { url: p.url, pane: beside };
		if (beside === undefined) {
			const t = await this.call("tmux.topology", {});
			const session = t?.sessions?.[0]?.id;
			if (session === undefined) throw new Error("tmux has no sessions");
			params = { url: p.url, session };
		}
		const o = await this.acting_on(beside, () => this.call("web.open", params));
		return { pane: o.pane, url: p.url, beside: beside ?? null };
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

	/** Memory's context for a call: this session's project and id. */
	private memoryContext(): Record<string, string> {
		const cx: Record<string, string> = {};
		if (this.memory.cwd) cx.cwd = path.resolve(this.memory.cwd);
		if (this.memory.session) cx.session = this.memory.session;
		return cx;
	}

	/** `memory.recall` in this session's scopes. */
	async recall(p: { query?: string; scope?: string; limit?: number }): Promise<any> {
		const q: Record<string, unknown> = { query: p.query ?? "", context: this.memoryContext() };
		if (p.scope) q.scope = p.scope;
		if (p.limit !== undefined) q.limit = p.limit;
		return this.call("memory.recall", q);
	}

	/** `memory.remember` with this session's context. */
	async remember(p: {
		text: string;
		kind: string;
		scope: string;
		title?: string;
		description?: string;
		links?: string[];
	}): Promise<any> {
		const q: Record<string, unknown> = { text: p.text, kind: p.kind, scope: p.scope, context: this.memoryContext() };
		for (const k of ["title", "description"] as const) if (p[k]) q[k] = p[k];
		if (Array.isArray(p.links) && p.links.length > 0) q.links = p.links;
		return this.call("memory.remember", q);
	}

	async forget(id: string): Promise<any> {
		return this.call("memory.forget", { id, context: this.memoryContext() });
	}

	/** The index of this session's memory scopes, or undefined when there
	 * is nothing to recall. */
	async memoryIndex(): Promise<string | undefined> {
		const r = await this.call("memory.list", { context: this.memoryContext() });
		return memoryIndex(r?.items ?? []);
	}

	/** The chat's `/memory` command: `list` (the default) or `recall
	 * WORDS`, `show ID`, `edit ID` (the text, through `edit`), and
	 * `forget ID`. Returns what to show. */
	async memoryCommand(args: string, edit?: (text: string) => Promise<string | undefined>): Promise<string> {
		const words = args.trim().split(/\s+/).filter((w) => w !== "");
		const [verb = "list", ...rest] = words;
		const id = () => {
			if (rest.length !== 1) throw new Error(`/memory ${verb} takes one id, such as project/small-commits`);
			return rest[0];
		};
		const cx = this.memoryContext();
		switch (verb) {
			case "list": {
				const r = await this.call("memory.list", { context: cx });
				const items: any[] = r?.items ?? [];
				if (items.length === 0) return "No memories in this session's scopes.";
				return items.map((m) => `${memoryLine(m).slice(2)} (${m.kind})`).join("\n");
			}
			case "recall": {
				const r = await this.recall({ query: rest.join(" ") });
				const items: any[] = r?.items ?? [];
				if (items.length === 0) return "Nothing recalled.";
				return items.map(memoryText).join("\n\n");
			}
			case "show":
				return memoryText(await this.call("memory.get", { id: id(), context: cx }));
			case "edit": {
				const m = await this.call("memory.get", { id: id(), context: cx });
				if (!edit) throw new Error("/memory edit needs the chat's editor");
				const text = await edit(String(m.text ?? ""));
				if (text === undefined || text.trim() === String(m.text ?? "").trim()) return `${m.id}: unchanged`;
				const u = await this.call("memory.update", { id: m.id, text, context: cx });
				return `Updated ${u.id}${u.path ? ` (${u.path})` : ""}`;
			}
			case "forget": {
				const f = await this.forget(id());
				return `Forgot ${f.id}${f.path ? `; removed ${f.path}` : ""}`;
			}
			default:
				throw new Error(`/memory: unknown \`${verb}\`; use /memory [list | recall WORDS | show ID | edit ID | forget ID]`);
		}
	}

	close() {
		if (this.gatherTimer) clearTimeout(this.gatherTimer);
		this.gatherTimer = undefined;
		this.client?.close();
		this.client = undefined;
	}
}

/** The templates `dream app new` starts from. */
export const APP_TEMPLATES = ["table", "form", "chat"];

/** Where the app tools work: `DREAMER_APPS_ROOT`, else pi's working
 * directory; with its links resolved. */
export function appsRoot(env: Record<string, string | undefined> = process.env, cwd: string = process.cwd()): string {
	const r = env.DREAMER_APPS_ROOT;
	return fs.realpathSync(r !== undefined && r !== "" ? r : cwd);
}

/** `dir` (relative to `root`, or absolute) as a path inside `root`,
 * following the links of the part that exists; else an error. The app
 * tools never read or write outside the apps directory. */
export function appDir(root: string, dir: string): string {
	if (typeof dir !== "string" || dir.trim() === "") throw new Error("give a directory");
	if (dir.split(/[\\/]/).includes("..")) throw new Error(`\`${dir}\`: \`..\` is not allowed; name a directory inside ${root}`);
	let existing = path.isAbsolute(dir) ? dir : path.join(root, dir);
	const rest: string[] = [];
	while (!fs.existsSync(existing)) {
		const up = path.dirname(existing);
		if (up === existing) break;
		rest.unshift(path.basename(existing));
		existing = up;
	}
	const real = path.join(fs.realpathSync(existing), ...rest);
	if (real !== root && !real.startsWith(root + path.sep)) {
		throw new Error(`\`${dir}\` is outside the apps directory ${root}; the app tools work only there`);
	}
	return real;
}

/** What `dream ARGS` printed, and whether it succeeded. */
export type Ran = { ok: boolean; stdout: string; stderr: string };

/** Run `dream` with exact arguments (no shell). */
export function runDream(dream: string, args: string[]): Promise<Ran> {
	return new Promise((resolve) => {
		execFile(dream, args, { maxBuffer: 16 * 1024 * 1024, timeout: 300_000 }, (err, stdout, stderr) => {
			const spawnFailed = err !== null && typeof (err as any).code === "string";
			resolve({
				ok: err === null,
				stdout: String(stdout ?? ""),
				stderr: spawnFailed ? `${dream}: ${err?.message}` : String(stderr ?? "").trim(),
			});
		});
	});
}

let previews = 0;

/** The app tools: `dream app` in the apps directory, and launching an
 * installed app on the session's own connection, so the daemon checks it
 * against the session's grant and the session owns the new pane. */
export class Apps {
	readonly session: Session;
	readonly dream: string;
	readonly root: string;

	constructor(session: Session, dream: string, root?: string) {
		this.session = session;
		this.dream = dream;
		this.root = root ?? appsRoot();
	}

	private app(args: string[]): Promise<Ran> {
		return runDream(this.dream, ["app", ...args]);
	}

	/** `dream app new`. */
	async create(p: { name: string; template?: string; dir?: string }): Promise<Record<string, unknown>> {
		const dir = appDir(this.root, p.dir ?? p.name);
		const r = await this.app(["new", p.name, "--template", p.template ?? "table", "--dir", dir]);
		if (!r.ok) throw new Error(r.stderr);
		return { dir, id: `local.${p.name}`, output: r.stdout.trim() };
	}

	/** `dream app check --json`: ok, and problems with file, line, and column. */
	async check(p: { dir: string }): Promise<any> {
		const r = await this.app(["check", appDir(this.root, p.dir), "--json"]);
		try {
			return JSON.parse(r.stdout);
		} catch {
			throw new Error(r.stderr || "dream app check said nothing");
		}
	}

	/** The app drawn from its fixtures: as text, or (`image`) a PNG as the
	 * desktop draws it, in base64, taken at this session's level. */
	async preview(p: { dir: string; mode?: string; size?: string }): Promise<{ text: string; image?: string }> {
		const dir = appDir(this.root, p.dir);
		const mode = p.mode ?? "text";
		if (mode === "text") {
			const r = await this.app(["preview", dir, "--text", ...(p.size ? ["--size", p.size] : [])]);
			if (!r.ok) throw new Error(r.stderr);
			return { text: r.stdout };
		}
		if (mode !== "image") throw new Error(`mode \`${mode}\`: use text or image`);
		if (p.size) throw new Error("size is for mode text; the desktop draws the pane at its size");
		const file = path.join(fs.realpathSync(os.tmpdir()), `dreamer-pi-preview-${process.pid}-${previews++}.png`);
		try {
			const r = await this.app(["preview", dir, "--png", file, "--level", this.session.level]);
			if (!r.ok) throw new Error(r.stderr);
			const said = [r.stdout.trim(), r.stderr].filter((s) => s !== "").join("\n");
			return { text: said, image: fs.readFileSync(file).toString("base64") };
		} finally {
			fs.rmSync(file, { force: true });
		}
	}

	/** `dream app test`: ok, and what differs. */
	async test(p: { dir: string; update?: boolean }): Promise<Record<string, unknown>> {
		const r = await this.app(["test", appDir(this.root, p.dir), ...(p.update ? ["--update"] : [])]);
		return { ok: r.ok, output: r.ok ? r.stdout.trim() : r.stderr };
	}

	/** `dream app install`, which needs the level that launches apps. */
	async install(p: { dir: string; name?: string }): Promise<Record<string, unknown>> {
		if (this.session.level === "observe") {
			throw new Error("installing an app needs the own or elsewhere level; this session only observes");
		}
		const r = await this.app(["install", appDir(this.root, p.dir), ...(p.name ? ["--name", p.name] : [])]);
		if (!r.ok) throw new Error(r.stderr);
		// "installed ID VERSION as NAME (DIR)"
		const name = r.stdout.split(" as ")[1]?.split(/\s+/)[0] ?? "";
		return { name, output: r.stdout.trim() };
	}

	/** Open an installed app beside `beside` (default the pane pi runs in),
	 * or in a new window, on this session's connection. */
	async launch(
		p: { name: string; beside?: number; new_window?: boolean },
		env: Record<string, string | undefined> = process.env,
	): Promise<Record<string, unknown>> {
		const r = await this.app(["launch", p.name, "--request"]);
		if (!r.ok) throw new Error(r.stderr);
		const request = JSON.parse(r.stdout);
		const own = /^%(\d+)$/.exec(env.TMUX_PANE ?? "");
		const beside = p.new_window ? undefined : (p.beside ?? (own ? Number(own[1]) : undefined));
		const t = await this.session.call("tmux.topology", {});
		const window = (t?.panes ?? []).find((x: any) => x.id === beside)?.window;
		const session = (t?.windows ?? []).find((w: any) => w.id === window)?.sessions?.[0] ?? t?.sessions?.[0]?.id;
		if (session === undefined) throw new Error("tmux has no sessions");
		const params = { ...request, session, ...(beside !== undefined ? { split: beside } : {}) };
		const l = await this.session.acting_on(beside, () => this.session.call("apps.launch", params));
		return { app_id: request.app_id, name: p.name, pane: l.pane, beside: beside ?? null, launch_id: l.launch_id };
	}
}
