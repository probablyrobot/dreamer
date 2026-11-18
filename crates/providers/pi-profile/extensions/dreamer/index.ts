// Dreamer's extension for pi: the terminals, layout, and collectors of the
// Dreamer daemon as native pi tools, durable memory, delegation to Claude
// Code and Codex, and the profile's history.
//
// Each tool is one call to the daemon over its JSON-RPC socket, on a
// connection that has narrowed itself to an agent's grant
// (`DREAMER_AGENT_LEVEL`, default `elsewhere`), or, in an agent session
// the daemon runs, holds that session's grant. The daemon checks every
// call; acting on a pane this session did not create waits for the user's
// approval. This extension shows those approvals and never answers them.
// The app tools run `dream app` in the apps directory, and open an
// installed app with `apps.launch` on that same connection.
//
// Shipped with Dreamer as part of its pi profile. Edits here are kept when
// Dreamer updates, but Dreamer's newer version of this file is then not
// applied; see `dream agents profile status`.

import * as fs from "node:fs";
import * as path from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
	APP_TEMPLATES,
	Apps,
	COLLECTOR_PRESETS,
	DELEGATES,
	MEMORY_KINDS,
	MEMORY_SCOPES,
	Session,
	collectorQuery,
	collectorSpec,
	delegates,
	inputEvents,
	memoryText,
	readResult,
	sendToPaneEvents,
} from "./daemon.ts";

type Ui = { notify(message: string, kind?: "info" | "warning" | "error"): void; setStatus(key: string, text: string | undefined): void };

const json = (v: unknown) => ({
	content: [{ type: "text" as const, text: JSON.stringify(v, null, 2) }],
	details: {},
});

export default function (pi: ExtensionAPI) {
	const session = new Session();
	const agentDir = process.env.PI_CODING_AGENT_DIR ?? "";
	const dream = process.env.DREAMER_DREAM_BIN || "dream";
	// Dreamer's profile check starts pi only to see that it loads.
	const checking = process.env.DREAMER_PI_CHECK === "1";
	let ui: Ui | undefined;
	let sessionId = "";
	let reloadRequested = false;
	let collectorTools = false;

	session.onApproval = (a) => {
		const where = a.pane !== undefined ? ` in pane %${a.pane}` : "";
		if (a.answered) {
			ui?.setStatus("dreamer", undefined);
			ui?.notify(`Approval ${a.id}${where}: ${a.answered}`, a.answered === "deny" || a.answered === "timeout" ? "warning" : "info");
		} else {
			ui?.setStatus("dreamer", `waiting for your approval (${a.id})`);
			ui?.notify(
				`Dreamer asks your approval${where}: ${a.what}\nAnswer in Dreamer, or run \`dream approvals allow ${a.id}\` (add --pane for this pane from now on) or \`dream approvals deny ${a.id}\`.`,
				"warning",
			);
		}
		pi.appendEntry("dreamer-approval", { ...a, time: Date.now() });
	};

	/** Run a daemon call for a tool; register collector tools once the
	 * daemon is known to offer them. */
	const call = async (method: string, params: Record<string, unknown>) => {
		const r = await session.call(method, params);
		if (session.collectors && !collectorTools) registerCollectors();
		return r;
	};

	const tool = (def: {
		name: string;
		label: string;
		description: string;
		parameters: any;
		run: (p: any) => Promise<unknown>;
	}) =>
		pi.registerTool({
			name: def.name,
			label: def.label,
			description: def.description,
			parameters: def.parameters,
			async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
				if (ctx?.hasUI) ui = ctx.ui;
				return json(await def.run(params));
			},
		});

	const pane = Type.Integer({ minimum: 0, description: "The pane id: the number in %N." });

	tool({
		name: "terminals_list",
		label: "Terminals",
		description:
			"List every terminal pane: its session, window, command, directory, title, size, tier, how long it has been quiet, and whether you created it (you may act there without asking).",
		parameters: Type.Object({}),
		run: () => call("terminal.list", {}),
	});

	tool({
		name: "terminal_read",
		label: "Read terminal",
		description: "Read a pane as text: its screen, or with `lines` the last that many lines of history and screen.",
		parameters: Type.Object({
			pane,
			lines: Type.Optional(Type.Integer({ minimum: 1, maximum: 2000, description: "Return the last this many lines instead of the screen." })),
		}),
		run: async (p) => {
			const lines = p.lines === undefined ? undefined : Math.min(p.lines, 2000);
			const t = await call("terminal.read", { pane: p.pane, ...(lines !== undefined ? { history: lines } : {}) });
			return readResult(t, lines);
		},
	});

	tool({
		name: "terminal_search",
		label: "Search terminals",
		description:
			"Search panes' recent output with a regular expression (or plain text with `literal`): one pane, some, or every pane. Bounded in lines, matches, bytes, and time; a cut-short answer says so.",
		parameters: Type.Object({
			pattern: Type.String(),
			literal: Type.Optional(Type.Boolean()),
			ignore_case: Type.Optional(Type.Boolean()),
			panes: Type.Optional(Type.Array(Type.Integer({ minimum: 0 }), { description: "Empty searches every pane." })),
			lines: Type.Optional(Type.Integer({ minimum: 1, maximum: 2000, description: "Newest lines per pane; default 200." })),
			max_matches: Type.Optional(Type.Integer({ minimum: 1, maximum: 1000 })),
			max_bytes: Type.Optional(Type.Integer({ minimum: 1 })),
			timeout_ms: Type.Optional(Type.Integer({ minimum: 1, maximum: 10000 })),
		}),
		run: (p) => call("terminal.query", p),
	});

	tool({
		name: "terminal_watch",
		label: "Watch terminal",
		description:
			"Wait until a pattern appears in a pane's new output, the pane goes quiet for quiet_ms, or (exit) its program ends; whichever comes first, or the timeout (default 30 s). Returns what happened and the matching line.",
		parameters: Type.Object({
			pane,
			pattern: Type.Optional(Type.String()),
			literal: Type.Optional(Type.Boolean()),
			ignore_case: Type.Optional(Type.Boolean()),
			quiet_ms: Type.Optional(Type.Integer({ minimum: 50 })),
			exit: Type.Optional(Type.Boolean()),
			timeout_ms: Type.Optional(Type.Integer({ minimum: 1, maximum: 600000 })),
		}),
		run: (p) => call("terminal.watch", p),
	});

	tool({
		name: "terminal_run",
		label: "Run in a terminal",
		description:
			"Run a command in a new window (or a pane split from `split`) that you then own, wait for it, and return its exit status and output. Give `argv` (no shell) or `command` (run with /bin/sh -c). If it outlasts the timeout it keeps running: follow it with terminal_watch (exit) and terminal_read.",
		parameters: Type.Object({
			argv: Type.Optional(Type.Array(Type.String())),
			command: Type.Optional(Type.String()),
			cwd: Type.Optional(Type.String()),
			split: Type.Optional(Type.Integer({ minimum: 0, description: "Split this pane for it instead of opening a window." })),
			direction: Type.Optional(Type.Union([Type.Literal("vertical"), Type.Literal("horizontal")])),
			name: Type.Optional(Type.String({ description: "The new window's name." })),
			timeout_ms: Type.Optional(Type.Integer({ minimum: 1, maximum: 600000 })),
			lines: Type.Optional(Type.Integer({ minimum: 1, maximum: 2000 })),
		}),
		run: async (p) => {
			const hasArgv = Array.isArray(p.argv) && p.argv.length > 0;
			if (hasArgv === (typeof p.command === "string")) throw new Error("give either argv or command");
			const argv = hasArgv ? p.argv : ["/bin/sh", "-c", p.command];
			const { command: _c, ...rest } = p;
			return session.acting_on(p.split, () => call("terminal.run", { ...rest, argv }));
		},
	});

	tool({
		name: "terminal_send",
		label: "Type into a terminal",
		description:
			"Type `text`, paste `paste`, and press Enter (`enter`) in a pane, under an interaction lease: a person typing there takes the pane back, and you then stop. In a pane you did not create this waits for the user's approval.",
		parameters: Type.Object({
			pane,
			text: Type.Optional(Type.String()),
			paste: Type.Optional(Type.String()),
			enter: Type.Optional(Type.Boolean()),
		}),
		run: async (p) => {
			const events = inputEvents(p);
			await session.connection();
			return session.send(p.pane, events);
		},
	});

	tool({
		name: "send_to_pane",
		label: "Send to a pane",
		description:
			"Type a code block into a pane as one paste, for the user to run or edit there: `code` is the block (a Markdown fence around it is removed). Enter is pressed only with `enter`. Under an interaction lease: a person typing there takes the pane back, and you then stop. In a pane you did not create this waits for the user's approval.",
		parameters: Type.Object({
			pane,
			code: Type.String({ description: "The text to type, such as a command or a code block." }),
			enter: Type.Optional(Type.Boolean({ description: "Press Enter after it; default false." })),
		}),
		run: async (p) => {
			const events = sendToPaneEvents(p);
			await session.connection();
			return session.send(p.pane, events);
		},
	});

	// Pane context: `@pane`, `@%N`, and `@collector` in a prompt append the
	// pane's visible text or the collector's newest rows to it; `/pane`
	// attaches the same to the next prompt.
	const attach = (content: string) =>
		(pi as any).sendMessage({ customType: "dreamer-context", content, display: true }, { deliverAs: "nextTurn" });

	pi.registerCommand("pane", {
		description: "Attach a pane to your next prompt: the pane the drawer came from (or %N), its visible text, or with `last` its last command and output",
		handler: async (args: string, ctx: any) => {
			if (ctx?.hasUI) ui = ctx.ui;
			const words = (args ?? "").trim().split(/\s+/).filter((w) => w !== "");
			const last = words.includes("last");
			const named = words.find((w) => /^%?\d+$/.test(w));
			const other = words.find((w) => w !== "last" && w !== named);
			if (other !== undefined) {
				ui?.notify(`/pane: unknown argument \`${other}\`; use /pane [last] [%N]`, "error");
				return;
			}
			try {
				const p = named !== undefined ? Number(named.replace("%", "")) : await session.fromPane();
				attach(await session.paneContext(p, last));
				ui?.notify(`Attached pane %${p}${last ? " (last command)" : ""} to your next prompt`, "info");
			} catch (e) {
				ui?.notify(`/pane: ${e instanceof Error ? e.message : String(e)}`, "error");
			}
		},
	});

	pi.on("input", async (event: any, ctx: any) => {
		if (checking || typeof event?.text !== "string" || !event.text.includes("@")) return;
		if (ctx?.hasUI) ui = ctx.ui;
		let r: { text: string; attached: string[]; problems: string[] };
		try {
			r = await session.expand(event.text);
		} catch (e) {
			ui?.notify(`Dreamer could not attach context: ${e instanceof Error ? e.message : String(e)}`, "warning");
			return;
		}
		for (const p of r.problems) ui?.notify(`Not attached: ${p}`, "warning");
		if (r.attached.length === 0) return;
		return { action: "transform", text: r.text, ...(event.images ? { images: event.images } : {}) };
	});

	tool({
		name: "layout",
		label: "Change the layout",
		description:
			"Change the terminal layout: `split` a pane (optionally running an exact `argv`; you own the new pane), `select` a pane or window, `rename` a window or session, `move` a pane to another window or a new one, `zoom` a pane, or `close` a pane or window. Outside what you created this waits for the user's approval; closing always asks.",
		parameters: Type.Object({
			action: Type.Union(["split", "select", "rename", "move", "zoom", "close"].map((a) => Type.Literal(a))),
			pane: Type.Optional(Type.Integer({ minimum: 0 })),
			window: Type.Optional(Type.Integer({ minimum: 0, description: "A window id: the number in @N." })),
			session: Type.Optional(Type.Integer({ minimum: 0, description: "A session id: the number in $N." })),
			name: Type.Optional(Type.String({ description: "For rename." })),
			new_window: Type.Optional(Type.String({ description: "For move: the name of a new window." })),
			direction: Type.Optional(Type.Union([Type.Literal("vertical"), Type.Literal("horizontal")])),
			argv: Type.Optional(Type.Array(Type.String(), { description: "For split: the exact program and arguments." })),
			cwd: Type.Optional(Type.String()),
		}),
		run: async (p) => {
			await session.connection();
			return session.layout(p.action, p);
		},
	});

	tool({
		name: "browser_open",
		label: "Open a page",
		description:
			"Open a web page for the user in Dreamer's browser: a new pane beside the pane you run in (or `beside` another pane, or in a new window), drawn by the user's desktop; or, with `pane`, load the address in that web pane. You own a pane you opened. Splitting or loading into a pane you did not create waits for the user's approval. The page gets no Dreamer API, and you cannot read it from here. Not at the observe level.",
		parameters: Type.Object({
			url: Type.String({ description: "The address, as typed into a browser's address bar: a URL, example.com, localhost:3000, or an absolute file path." }),
			pane: Type.Optional(Type.Integer({ minimum: 0, description: "Load it in this web pane instead of a new one." })),
			beside: Type.Optional(Type.Integer({ minimum: 0, description: "Open the new pane beside this pane." })),
			new_window: Type.Optional(Type.Boolean({ description: "Open it in a new window instead." })),
		}),
		run: async (p) => {
			await session.connection();
			return session.browser(p);
		},
	});

	// Reading and driving pages the user's desktop shows: refused unless
	// the grant holds web.inspect (docs/grants.md).
	const webPane = Type.Integer({ minimum: 0, description: "A web pane, or the pane of a web app." });
	const node = Type.Optional(Type.Integer({ minimum: 1, description: "The element: a node id from web_read's tree." }));
	const selector = Type.Optional(Type.String({ description: "Or the element: a CSS selector (its first match)." }));

	tool({
		name: "web_read",
		label: "Read a page",
		description:
			"Read a page the user's Dreamer window shows, in a web pane or a web app's pane: its address, title, and text, and with tree its accessibility tree (each node's id, depth, role, name, and value). Needs the web.inspect capability in your grant.",
		parameters: Type.Object({ pane: webPane, tree: Type.Optional(Type.Boolean()) }),
		run: async (p) => {
			await session.connection();
			return session.call("web.read", { pane: p.pane, tree: p.tree ?? false });
		},
	});

	tool({
		name: "web_click",
		label: "Click in a page",
		description:
			"Click an element of a page in a web pane or a web app's pane, named by node (an id from web_read's tree) or selector: it is scrolled into view and clicked in its middle, as with a mouse. Needs web.inspect; in a pane you did not open it waits for the user's approval.",
		parameters: Type.Object({ pane: webPane, node, selector }),
		run: async (p) => {
			await session.connection();
			return session.acting_on(p.pane, () => session.call("web.click", { pane: p.pane, node: p.node, selector: p.selector }));
		},
	});

	tool({
		name: "web_type",
		label: "Type in a page",
		description:
			"Type text into a page in a web pane or a web app's pane: into the element named by node or selector (focused first), else where the page's focus is; with submit, then press Enter, which submits a form from its field. The clipboard is not used. Needs web.inspect; in a pane you did not open it waits for the user's approval.",
		parameters: Type.Object({ pane: webPane, text: Type.String(), node, selector, submit: Type.Optional(Type.Boolean()) }),
		run: async (p) => {
			await session.connection();
			const params = { pane: p.pane, text: p.text, node: p.node, selector: p.selector, submit: p.submit ?? false };
			return session.acting_on(p.pane, () => session.call("web.type", params));
		},
	});

	pi.registerTool({
		name: "web_screenshot",
		label: "Page image",
		description:
			"An image of a page in a web pane or a web app's pane, as Chromium draws it in the user's Dreamer window: the page only. Needs web.inspect.",
		parameters: Type.Object({ pane: webPane }),
		async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
			if (ctx?.hasUI) ui = ctx.ui;
			await session.connection();
			const r = await session.webScreenshot(params.pane);
			return {
				content: [
					{ type: "image", data: r.image, mimeType: "image/png" },
					{ type: "text", text: `${r.width}x${r.height} pixels` },
				],
				details: {},
			};
		},
	});

	// Building apps: `dream app` in the apps directory (DREAMER_APPS_ROOT,
	// else pi's working directory; nothing outside it), and opening an
	// installed app on this session's connection, under its grant.
	let apps: Apps | undefined;
	const appTools = () => {
		apps ??= new Apps(session, dream);
		return apps;
	};
	const appDirParam = Type.String({ description: "The package directory, inside the apps directory (pi's working directory unless DREAMER_APPS_ROOT says otherwise)." });

	tool({
		name: "app_new",
		label: "New app",
		description: `Create a working app package to start from: app.toml, app.ts (TypeScript), fixtures to preview and test it, and types/dreamer.d.ts. Templates: ${APP_TEMPLATES.join(", ")}. Then edit app.ts and run app_check. The dreamer-apps skill has the components and the API.`,
		parameters: Type.Object({
			name: Type.String({ description: "Lowercase letters, digits, and -, starting with a letter; the app's id is local.NAME." }),
			template: Type.Optional(Type.Union(APP_TEMPLATES.map((t) => Type.Literal(t)))),
			dir: Type.Optional(Type.String({ description: "The package directory, inside the apps directory; default NAME." })),
		}),
		run: (p) => appTools().create(p),
	});

	tool({
		name: "app_check",
		label: "Check app",
		description:
			"Check an app package: build its app.ts, then the manifest, the grant it needs, TypeScript type errors, and the view tree. Returns ok, and problems as file, line, column, and message; fix each and check again.",
		parameters: Type.Object({ dir: appDirParam }),
		run: (p) => appTools().check(p),
	});

	pi.registerTool({
		name: "app_preview",
		label: "Preview app",
		description:
			"Preview an app from its fixtures. mode text (default) returns the screen as text, as the app host draws it in a terminal. mode image returns a PNG of the app as the Dreamer desktop draws it natively; it needs a running Dreamer window, opens the preview in a window of its own, and closes it again.",
		parameters: Type.Object({
			dir: appDirParam,
			mode: Type.Optional(Type.Union([Type.Literal("text"), Type.Literal("image")])),
			size: Type.Optional(Type.String({ description: "For text: WIDTHxHEIGHT, such as 80x24." })),
		}),
		async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
			if (ctx?.hasUI) ui = ctx.ui;
			const r = await appTools().preview(params);
			const content: any[] = [];
			if (r.image !== undefined) content.push({ type: "image", data: r.image, mimeType: "image/png" });
			content.push({ type: "text", text: r.text });
			return { content, details: {} };
		},
	});

	tool({
		name: "app_test",
		label: "Test app",
		description:
			"Test an app: replay fixtures/events.jsonl and compare the screen after each event with fixtures/expect/. Returns ok and what differs. With update, record the screens drawn now as the expected ones (after a deliberate change).",
		parameters: Type.Object({ dir: appDirParam, update: Type.Optional(Type.Boolean()) }),
		run: (p) => appTools().test(p),
	});

	tool({
		name: "app_install",
		label: "Install app",
		description:
			"Install an app package for the user: check it, draw it from its fixtures, and copy it into Dreamer's apps directory. A version it replaces is kept (dream app rollback NAME puts it back). Returns the installed name for app_launch. Not at the observe level.",
		parameters: Type.Object({
			dir: appDirParam,
			name: Type.Optional(Type.String({ description: "The installed name; default the app's id." })),
		}),
		run: (p) => appTools().install(p),
	});

	tool({
		name: "app_launch",
		label: "Open app",
		description:
			"Open an installed app for the user in a pane beside the pane you run in (or `beside` another pane, or in a new window), with the grant it declares; that grant must be within yours. You own the new pane. Splitting a pane you did not create waits for the user's approval. Not at the observe level.",
		parameters: Type.Object({
			name: Type.String({ description: "The installed name (what app_install said)." }),
			beside: Type.Optional(Type.Integer({ minimum: 0, description: "Open it beside this pane." })),
			new_window: Type.Optional(Type.Boolean({ description: "Open it in a new window instead." })),
		}),
		run: (p) => appTools().launch(p),
	});

	// Durable memory (docs/memory.md): the agent recalls and remembers with
	// these tools, and they show in the chat as tool calls; the first
	// prompt of a session gets the index of what there is to recall; and
	// `/memory` lists, shows, edits, and forgets items from the chat.
	const memoryResult = (text: string, details: unknown) => ({
		content: [{ type: "text" as const, text }],
		details,
	});
	const scopeT = (what: string) => Type.Union(MEMORY_SCOPES.map((s) => Type.Literal(s)), { description: what });

	pi.registerTool({
		name: "memory_recall",
		label: "Recall",
		description:
			"Recall durable memory: notes kept across sessions and agents about the user, this host, this project, and this session. Finds memories by words (no query: the newest), best first, each with its id, kind, title, and text. Recall before asking the user something they may have told an agent before.",
		parameters: Type.Object({
			query: Type.Optional(Type.String({ description: "Words to look for." })),
			scope: Type.Optional(scopeT("Only this scope; default every scope this session reaches.")),
			limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 100, description: "Default 10." })),
		}),
		async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
			if (ctx?.hasUI) ui = ctx.ui;
			const r = await session.recall(params);
			const items: any[] = r?.items ?? [];
			const text = items.length === 0 ? "Nothing recalled." : items.map(memoryText).join("\n\n");
			return memoryResult(text, r);
		},
	});

	pi.registerTool({
		name: "memory_remember",
		label: "Remember",
		description:
			"Remember a fact worth having in a later session, with any agent: something the user told you about themselves or how to work, a decision in this project, or where to find something. One fact per memory; do not store secrets or what the code or git history already records. Returns the new memory's id and file.",
		parameters: Type.Object({
			text: Type.String({ description: "The fact, in a sentence or a short paragraph." }),
			kind: Type.Union(MEMORY_KINDS.map((k) => Type.Literal(k)), {
				description:
					"user (the user's role, preferences, knowledge), feedback (guidance on how to work), project (decisions, people, dates in the work), or reference (where to find something).",
			}),
			scope: scopeT("user (everywhere), host (this machine), project (this project), or session (this session only)."),
			title: Type.Optional(Type.String({ description: "A short title; default the text's first line." })),
			description: Type.Optional(Type.String({ description: "One line saying what it is about, for the index." })),
			links: Type.Optional(Type.Array(Type.String(), { description: "Paths, URLs, or other memories' ids." })),
		}),
		async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
			if (ctx?.hasUI) ui = ctx.ui;
			const m = await session.remember(params);
			return memoryResult(`Remembered ${m.id}: ${m.title}${m.path ? ` (${m.path})` : ""}`, m);
		},
	});

	// Forgetting may delete what the user wrote: from `own` up.
	if (session.level !== "observe") {
		pi.registerTool({
			name: "memory_forget",
			label: "Forget",
			description:
				"Forget a memory that is wrong or no longer true: deletes its file. Only when the user asks, or a memory you kept turned out wrong.",
			parameters: Type.Object({ id: Type.String({ description: "The memory's id, such as project/small-commits." }) }),
			async execute(_id: string, params: any, _signal: unknown, _onUpdate: unknown, ctx: any) {
				if (ctx?.hasUI) ui = ctx.ui;
				const m = await session.forget(params.id);
				return memoryResult(`Forgot ${m.id}${m.path ? `; removed ${m.path}` : ""}`, m);
			},
		});
	}

	pi.registerCommand("memory", {
		description: "Dreamer memory: list (default), recall WORDS, show ID, edit ID, or forget ID",
		getArgumentCompletions: (prefix: string) =>
			["list", "recall", "show", "edit", "forget"].filter((v) => v.startsWith(prefix)).map((v) => ({ value: v, label: v })),
		handler: async (args: string, ctx: any) => {
			if (ctx?.hasUI) ui = ctx.ui;
			try {
				const text = await session.memoryCommand(args ?? "", (t) => ctx.ui.editor("Edit the memory's text", t));
				ui?.notify(text, "info");
			} catch (e) {
				ui?.notify(`/memory: ${e instanceof Error ? e.message : String(e)}`, "error");
			}
		},
	});

	// The memory index goes with the session's first prompt. In an agent
	// session the daemon runs, the daemon adds it instead.
	let indexed = false;
	pi.on("before_agent_start", async (event: any) => {
		if (checking || indexed || session.daemonSession || event?.messageType !== "user") return;
		indexed = true;
		const content = await session.memoryIndex().catch(() => undefined);
		if (!content) return;
		return { message: { customType: "dreamer-memory", content, display: true } };
	});

	// Delegation: hand a task to Claude Code or Codex, run headless on the
	// CLI's own login by `dream agents delegate`, as a job in a window this
	// session owns so the user can watch it. `agents.pi.delegates` chooses
	// which (DREAMER_PI_DELEGATES). At the observe level an agent runs
	// nothing, so there are none.
	for (const to of session.level === "observe" ? [] : delegates()) {
		const d = DELEGATES[to];
		pi.registerTool({
			name: d.tool,
			label: d.label,
			description: `Hand a self-contained task to ${d.label}, run headless on its own login in a new window you own (the user can watch it there), and get back its final answer and a summary of the tools it used. It can read files under cwd but not change them. Give it everything it needs in the task: it does not see this conversation. If ${d.label} is not installed or not logged in, this says so; it never falls back to an API key.`,
			parameters: Type.Object({
				task: Type.String({ description: `What ${d.label} should do or answer, with all the context it needs.` }),
				cwd: Type.Optional(Type.String({ description: "The directory it works in; default pi's working directory." })),
				timeout_s: Type.Optional(Type.Integer({ minimum: 1, maximum: 7200, description: "Stop it after this long; default 1800." })),
			}),
			async execute(_id: string, params: any, signal: AbortSignal | undefined, _onUpdate: unknown, ctx: any) {
				if (ctx?.hasUI) ui = ctx.ui;
				const r: any = await session.delegate(
					dream,
					to,
					params.task,
					{ cwd: params.cwd ?? process.cwd(), timeout_s: params.timeout_s },
					signal,
				);
				if (r.ok !== true) throw new Error(`${r.error ?? "the run failed"} (pane %${r.pane})`);
				const tools = r.tools?.summary ?? "none";
				return {
					content: [{ type: "text" as const, text: `${r.answer}\n\n[${d.label} in pane %${r.pane}${r.model ? `, ${r.model}` : ""}; tools: ${tools}]` }],
					details: r,
				};
			},
		});
	}

	const registerCollectors = () => {
		if (collectorTools) return;
		collectorTools = true;
		const from = Type.Object(
			{
				all: Type.Optional(Type.Boolean({ description: "Every pane, including panes created later." })),
				session: Type.Optional(Type.String({ description: "The panes of the session with this name." })),
				window: Type.Optional(Type.Integer({ minimum: 0, description: "The panes of window @N." })),
				panes: Type.Optional(Type.Array(Type.Integer({ minimum: 0 }), { description: "These panes." })),
				command: Type.Optional(
					Type.String({ description: "Panes whose current command (cargo, pytest, ...) matches this regex, while it does." }),
				),
			},
			{ additionalProperties: false, description: "Which panes: exactly one of all, session, window, panes, command." },
		);
		const when = Type.Object(
			{
				line: Type.Optional(
					Type.String({ description: "A committed output line matches this regex (Rust syntax); named groups (?P<name>...) become fields." }),
				),
				ignore_case: Type.Optional(Type.Boolean()),
				command_end: Type.Optional(
					Type.Boolean({ description: "A command finished (needs the shell's OSC 133 marks): exit_status and the field duration_ms." }),
				),
				idle_ms: Type.Optional(Type.Integer({ minimum: 100, description: "The pane wrote nothing for this many ms." })),
				bell: Type.Optional(Type.Boolean()),
				exit: Type.Optional(Type.Boolean({ description: "The pane's process ended." })),
				every_s: Type.Optional(Type.Integer({ minimum: 1, description: "Every N seconds (keeps the screen)." })),
			},
			{ additionalProperties: false, description: "What makes a row: exactly one of line, command_end, idle_ms, bell, exit, every_s." },
		);
		const capture = Type.Object(
			{
				fields: Type.Optional(
					Type.Array(
						Type.Object({
							name: Type.String(),
							value: Type.String({ description: "A template: $1, ${1}, or ${name} is a group of `line`." }),
						}),
					),
				),
				json: Type.Optional(Type.Boolean({ description: "The line is a JSON object: its members become fields." })),
				screen: Type.Optional(Type.Boolean()),
				lines: Type.Optional(Type.Integer({ minimum: 1, maximum: 1000, description: "Keep the pane's last N lines." })),
				before: Type.Optional(Type.Integer({ minimum: 0, maximum: 100 })),
				after: Type.Optional(Type.Integer({ minimum: 0, maximum: 100 })),
			},
			{ additionalProperties: false },
		);
		const keep = Type.Object(
			{
				rows: Type.Optional(Type.Integer({ minimum: 1, maximum: 100000 })),
				max_age_s: Type.Optional(Type.Integer({ minimum: 1 })),
			},
			{ additionalProperties: false },
		);
		tool({
			name: "collector_create",
			label: "Create a collector",
			description: `Create a collector: a standing rule in the Dreamer daemon that turns terminal output into rows, kept across restarts. Start from a preset (${COLLECTOR_PRESETS.join(", ")}) or give from, when, capture, and keep; parts you give replace the preset's. It only reads panes. With wake (the default), you are told of new rows as they come; read them with collector_query.`,
			parameters: Type.Object({
				name: Type.Optional(Type.String({ description: "Letters, digits, -, _, or .; unique. Defaults to the preset's name." })),
				preset: Type.Optional(Type.Union(COLLECTOR_PRESETS.map((p) => Type.Literal(p)))),
				from: Type.Optional(from),
				when: Type.Optional(when),
				capture: Type.Optional(capture),
				keep: Type.Optional(keep),
				per_second: Type.Optional(Type.Integer({ minimum: 1, maximum: 1000, description: "Rows per second per pane; default 10." })),
				wake: Type.Optional(Type.Boolean({ description: "Tell me about new rows (default true)." })),
			}),
			run: async (p) => {
				const status = await call("collect.create", collectorSpec(p));
				if (p.wake !== false && status?.name) session.following.add(String(status.name));
				return status;
			},
		});
		tool({
			name: "collector_query",
			label: "Query a collector",
			description:
				"Rows a collector recorded, oldest first: filter by panes, exact field values, time (since_s), or row id (after); or with group_by count rows per value of a field (or pane, session, window, cwd, command, trigger), most first.",
			parameters: Type.Object({
				name: Type.String(),
				panes: Type.Optional(Type.Array(Type.Integer({ minimum: 0 }))),
				fields: Type.Optional(Type.Record(Type.String(), Type.String(), { description: 'Exact values, such as {"severity": "error"}.' })),
				since_s: Type.Optional(Type.Integer({ minimum: 0, description: "Only rows from the last this many seconds." })),
				after: Type.Optional(Type.Integer({ minimum: 0, description: "Only rows after this row id: pick up where you left off." })),
				limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 10000, description: "Default 100; without after, the newest." })),
				group_by: Type.Optional(Type.String()),
			}),
			run: (p) => call("collect.query", collectorQuery(p)),
		});
		tool({
			name: "collector_list",
			label: "List collectors",
			description:
				"Every collector: its definition, whether it runs, rows kept, collected, and dropped, the panes it follows, and its last error.",
			parameters: Type.Object({}),
			run: () => call("collect.list", {}),
		});
		tool({
			name: "collector_stop",
			label: "Stop a collector",
			description: "Stop a collector; its rows stay queryable. With delete, also remove its definition and rows.",
			parameters: Type.Object({ name: Type.String(), delete: Type.Optional(Type.Boolean()) }),
			run: async (p) => {
				session.following.delete(p.name);
				return call("collect.stop", { name: p.name, delete: p.delete === true });
			},
		});
	};

	// New rows of the collectors this session created wake the agent, one
	// short note per batch.
	session.onRows = (batches) => {
		if (checking) return;
		const text = batches
			.map((b) => {
				const panes = b.panes.map((p) => `%${p}`).join(", ") || "-";
				return `Collector \`${b.collector}\` recorded ${b.count} new row(s) (ids ${b.first}..${b.last}, pane(s) ${panes}). Read them with collector_query name="${b.collector}" after=${b.first - 1}.`;
			})
			.join("\n");
		const p = pi as any;
		if (typeof p.sendMessage === "function") {
			p.sendMessage({ customType: "dreamer-collector", content: text, display: true }, { triggerTurn: true, deliverAs: "followUp" });
		} else {
			ui?.notify(text, "info");
		}
	};

	// The profile's history: every change is a commit naming the session.
	const isProfile = agentDir !== "" && fs.existsSync(path.join(agentDir, ".dreamer", "base.json"));

	const commit = async (message: string | undefined, check: boolean) => {
		const args = ["agents", "profile", "commit", "--dir", agentDir];
		if (sessionId) args.push("--session", sessionId);
		if (message) args.push("-m", message);
		if (!check) args.push("--no-check");
		const r = await pi.exec(dream, args, { timeout: 120000 });
		return { ok: r.code === 0, text: `${r.stdout}${r.stderr}`.trim() };
	};

	if (isProfile) {
		tool({
			name: "dreamer_profile_commit",
			label: "Commit profile change",
			description: `Record a change you made to your own pi profile (${agentDir}: extensions, skills, prompts, settings) as a commit with a message saying why, and check that pi still loads with it. Set reload to load the change into this session once your reply ends.`,
			parameters: Type.Object({
				message: Type.String({ description: "What changed and why, in one line." }),
				reload: Type.Optional(Type.Boolean()),
			}),
			run: async (p) => {
				const r = await commit(p.message, true);
				if (!r.ok) throw new Error(r.text || "the commit failed");
				if (p.reload && r.text.includes("check: ok")) reloadRequested = true;
				return { result: r.text, reload: reloadRequested ? "after this reply" : "no" };
			},
		});

		pi.registerCommand("dreamer-reload", {
			description: "Commit profile changes, then reload extensions, skills, and prompts",
			handler: async (_args: string, ctx: any) => {
				await commit(undefined, false);
				await ctx.reload();
				return;
			},
		});

		pi.on("agent_settled", async () => {
			if (checking) return;
			await commit(undefined, false).catch(() => {});
			if (reloadRequested) {
				reloadRequested = false;
				pi.sendUserMessage("/dreamer-reload", { expandPromptTemplates: true });
			}
		});
	}

	pi.registerCommand("dreamer", {
		description: "Dreamer: the daemon connection, the agent's level, and the profile",
		handler: async (_args: string, ctx: any) => {
			let daemon: string;
			try {
				const panes = await call("terminal.list", {});
				const n = Array.isArray(panes?.terminals) ? panes.terminals.length : 0;
				daemon = `connected to ${session.socket} at level ${session.level}; ${n} panes; collectors ${session.collectors ? "available" : "not offered"}`;
			} catch (e) {
				daemon = `not connected: ${e instanceof Error ? e.message : String(e)}`;
			}
			const profile = isProfile ? `profile ${agentDir} (versioned)` : `agent directory ${agentDir || "(pi's default)"} (not a Dreamer profile)`;
			ctx.ui.notify(`Dreamer: ${daemon}\n${profile}`, "info");
		},
	});

	pi.on("session_start", async (_event: any, ctx: any) => {
		if (ctx?.hasUI) ui = ctx.ui;
		sessionId = ctx?.sessionManager?.getSessionId?.() ?? "";
		// Memory's session scope is pi's session, unless the daemon runs
		// this one as its own agent session.
		if (!session.daemonSession) session.memory.session = sessionId;
		if (typeof ctx?.cwd === "string" && ctx.cwd !== "") session.memory.cwd = ctx.cwd;
		// A session that already had its first prompt (resumed) is indexed.
		const entries: any[] = ctx?.sessionManager?.getEntries?.() ?? [];
		indexed = entries.some((e) => e?.customType === "dreamer-memory" || e?.message?.role === "user");
		if (checking) return;
		// Collector tools appear when the daemon offers them; a daemon that
		// is not running yet is tried again on the first tool call.
		session
			.connection()
			.then(() => {
				if (session.collectors) registerCollectors();
			})
			.catch(() => {});
	});

	// Dreamer pins pi to one subscription provider (`DREAMER_PI_PROVIDER`,
	// set by `dream agents pi`). Without a pinned model pi picks one itself,
	// possibly from a provider that bills or that the user never chose, so
	// before a prompt starts a turn the model is moved to the pinned
	// provider's, or the prompt is held back.
	const pinned = process.env.DREAMER_PI_PROVIDER ?? "";
	if (pinned && !checking) {
		pi.on("input", async (_event: any, ctx: any) => {
			if (!ctx?.model || ctx.model.provider === pinned) return { action: "continue" };
			const available = await Promise.resolve(ctx.modelRegistry?.getAvailable?.()).catch(() => []);
			const model = (Array.isArray(available) ? available : []).find((m: any) => m?.provider === pinned);
			if (model && (await pi.setModel(model))) return { action: "continue" };
			ctx.ui?.notify?.(
				`Dreamer holds pi to provider ${pinned}, which is not logged in here, so the prompt was not sent. Run /login ${pinned}, or pick one of its models with /model.`,
				"warning",
			);
			return { action: "handled" };
		});
	}

	pi.on("session_shutdown", async () => {
		if (isProfile && !checking) await commit(undefined, false).catch(() => {});
		session.close();
	});
}
