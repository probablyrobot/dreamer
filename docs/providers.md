# Agent providers

Agent features reach a model through one provider interface, the
`providers` crate. It drives the CLIs you already have installed and
logged in to:

| Provider | Runs | Billing |
| --- | --- | --- |
| `claude` | Claude Code: `claude -p ... --output-format stream-json`, as a child process | Your Claude Code login, normally a subscription |
| `codex` | Codex: `codex exec --json ...`, as a child process | Your Codex login, normally ChatGPT |

The default order is `claude`, then `codex`: the first that is ready is
used.

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
  Codex runs with `--sandbox read-only`.
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

## On the command line

```sh
dream agents providers [--json]
dream agents ask [--provider claude|codex] [--method M]... [--pane %N]... [--all-panes]
                 [--no-tools] [--max-turns N] [--timeout SECONDS] [--allow-metered] -- PROMPT
```

`providers` probes each CLI: installed, a version this adapter reads, and
logged in. `ask` runs one prompt.
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
| Codex | not run | `exec --json` events (`thread.started`, `item.*` with `agent_message` and `mcp_tool_call`, `turn.completed`, `turn.failed`, `error`), `-c mcp_servers.*`, `--sandbox`, `login status` |

A Claude Code with another major version shows as `incompatible`. The
Codex adapter follows Codex's documented `exec --json` format and is
tested against a stand-in only; no real Codex has been run with it yet.

Before release, each vendor's terms for driving its subscription CLI from
another application have to be checked and recorded in an ADR.
