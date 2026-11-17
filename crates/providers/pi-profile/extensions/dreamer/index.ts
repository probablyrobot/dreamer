// Dreamer's extension for pi: the terminals, layout, and collectors of the
// Dreamer daemon as native pi tools, delegation to Claude Code and Codex,
// and the profile's history.
//
// Each tool is one call to the daemon over its JSON-RPC socket, on a
// connection that has narrowed itself to an agent's grant
// (`DREAMER_AGENT_LEVEL`, default `elsewhere`). The daemon checks every
// call; acting on a pane this session did not create waits for the user's
// approval. This extension shows those approvals and never answers them.
//
// Shipped with Dreamer as part of its pi profile. Edits here are kept when
// Dreamer updates, but Dreamer's newer version of this file is then not
// applied; see `dream agents profile status`.

import * as fs from "node:fs";
import * as path from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { COLLECTOR_PRESETS, DELEGATES, Session, collectorQuery, collectorSpec, delegates, inputEvents, readResult } from "./daemon.ts";

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
