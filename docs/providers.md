# Agent providers

A provider runs one prompt on a model and streams the answer back.
`dream agents ask` uses one, and so does a task pi delegates to Claude
Code or Codex. Providers drive the CLIs you already have installed and
logged in to:

| Provider | Runs | Billing |
| --- | --- | --- |
| `claude` | Claude Code: `claude -p ... --output-format stream-json`, as a child process | Your Claude Code login, normally a subscription |
| `codex` | Codex: `codex exec --json ...`, as a child process | Your Codex login, normally ChatGPT |

The default order is `claude`, then `codex`: the first that is ready is
used.

```sh
dream agents providers                         # which CLIs are ready, and on which login
dream agents ask -- "which pane is running the tests?"
```

Providers are separate from the agent you chat with. That is a harness
(pi by default), chosen by `agents:` in `dreamer.yaml`; see
[agents.md](agents.md).

## Rules

- **No metered fallback.** The child process never gets
  `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, or
  `CODEX_API_KEY`, even when they are set for Dreamer. So a CLI can only use
  its own login. A Claude Code run that still reports an API key (from its
  own settings) is stopped before the model answers, unless you allow
  metered use (`--allow-metered`). A Codex logged in with an API key shows
  as `metered`. A missing, logged-out, or rate-limited CLI is reported as
  such; Dreamer never switches to an API key on its own.
- **Your credentials stay where they are.** Login state comes from the
  CLI's own status command (`claude auth status`, `codex login status`).
  Only whether it is logged in and how are kept. Dreamer never reads,
  copies, or forwards a CLI's credential store.
- **Tools through MCP.** Dreamer's tools reach the CLI as one MCP server,
  [`dream mcp`](mcp.md), scoped by the grant you give it. Claude Code gets
  only that server (`--strict-mcp-config`) and no built-in tools
  (`--tools ""`): it cannot run shell commands or edit files on its own.
  A delegated task ([agents.md](agents.md#delegation-to-claude-code-and-codex))
  also gets the read-only `Read`, `Grep`, and `Glob`. Codex runs with
  `--sandbox read-only`. Its Dreamer tools are approved in advance
  (`default_tools_approval_mode`), since the grant already narrows them,
  and `dream mcp` gets the run's `DREAMER_*` variables, because Codex
  passes an MCP server only a few of its own.
- **One contract.** Every provider streams the same events: started, text,
  tool call, tool result, limit, usage, and then exactly one of done or
  failed. Each can be cancelled. Cancelling, or a timeout, ends the CLI's
  whole process group, including the MCP server it started. Each provider
  has a capability record:

  | | `claude` | `codex` |
  | --- | --- | --- |
  | Streams text as it is written | yes | no (whole messages) |
  | Tools (MCP) | yes | yes |
  | Cancel | yes | yes |
  | Usage | yes | yes |
  | Cost | yes | no |

## API adapters

The Claude Messages API (`claude-api`) and the OpenAI Chat Completions API
(`openai-api`) keep the same contract.
- **When they are used:** only when configured in
  [`dreamer.yaml`](settings.md#agent-providers) with a model, and named
  (or set as the default). They are metered and never a fallback.
- **Keys:** read from the environment or the OS keychain at run time,
  never from a file Dreamer reads, a manifest, a skin, or the action
  ledger.
- **Requests:** each model turn is one `curl` request, in its own process
  group. The key and the body reach `curl` on its standard input
  (`--config -`), never on its command line or on disk.
- **Tools:** an adapter starts `dream mcp` itself, takes the tool
  definitions from it, and runs the tool calls in a loop until the model
  answers, for at most `--max-turns` turns (16 by default).
- **Errors:** a refused key (HTTP 401 or 403) is reported as logged out,
  and HTTP 429 as rate limited.
- **Requires:** `curl` installed.

## On the command line

```sh
dream agents providers [--json]
dream agents ask [--provider claude|codex|claude-api|openai-api] [--method M]... [--pane %N]... [--all-panes]
                 [--no-tools] [--max-turns N] [--timeout SECONDS] [--allow-metered] -- PROMPT
```

`providers` probes each CLI (installed, a version this adapter reads, and
logged in) and each configured API adapter (its key can be read). `ask` runs one prompt.
- **Output:** the answer is printed on standard output as it arrives.
  Tool calls, their results, and usage go to standard error.
- **Tools:** `--method`, `--pane`, and `--all-panes` narrow the grant of
  the `dream mcp` server the CLI gets. Without them, it acts as you.
- **Daemon:** it is started if it is not running.
- **Cancel:** Ctrl-C cancels the run.

## Versions

| CLI | Tested | Adapter reads |
| --- | --- | --- |
| Claude Code | 2.1.325 | major version 2: `-p`, `stream-json` with `--verbose` and `--include-partial-messages`, `--tools`, `--strict-mcp-config`, `--mcp-config`, `--allowedTools`, `--max-turns`, `auth status --json` |
| Codex | 0.170.0 | `exec --json` events (`thread.started`, `item.*` with `agent_message` and `mcp_tool_call`, `turn.completed`, `turn.failed`, `error`), `-c mcp_servers.*` (with `default_tools_approval_mode` and `env`), `--sandbox`, `login status` |

A Claude Code with another major version shows as `incompatible`. The
Codex adapter was run against Codex 0.170.0 on a ChatGPT login: a
streamed answer, a Dreamer tool call through the grant, cancel, and the
logged-out state.

Dreamer runs your own, unmodified CLI on your own login, for requests you
make, and never handles its credentials. Every run says who is billed
(`billed to your own Claude Code login`). Your plan's limits assume
ordinary individual use: for heavy or unattended automation, configure an
API adapter instead.
