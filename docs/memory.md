# Memory

Durable memory is for facts worth keeping across agent sessions and
harnesses: what the user prefers, guidance they gave, decisions in a
project, where to find things. The daemon keeps it through a **provider**
and serves it with the `memory.*` API, `dream memory`, and the agent
grants. Instructions files (`AGENTS.md`, `CLAUDE.md`) and session
transcripts are separate; memory never changes them.

There are two providers:

- `files` (the default): one markdown file per memory, which a person can
  read, edit, diff, and commit like any other file.
- `kosmonaut`: one graph database for the user, Kosmonaut embedded in the
  daemon, where memories are nodes linked to their project, session,
  files, panes, and each other ([Kosmonaut](#kosmonaut)). It is in a
  `dreamerd` built with the `kosmonaut` feature.

`dream memory import` and `dream memory export` copy memories between the
two, so the readable form is never lost.

## Scopes and kinds

Every memory has one **scope**, which decides where it is kept and who
sees it:

| Scope | Applies to | Kept in (`files`) |
| --- | --- | --- |
| `user` | The user, everywhere | `<config dir>/memory/` (`~/.config/dreamer/memory/` on macOS and Linux) |
| `host` | This machine only | `<state dir>/memory/host/` |
| `project` | One project | `<project root>/.dreamer/memory/` |
| `session` | One agent session | `<state dir>/memory/sessions/<session id>/` |

The user scope sits in the configuration directory so it travels with the
user's dotfiles. Host and session memory sit in the state directory
(`~/.local/state/dreamer`, or `DREAMER_STATE_DIR`), which is private to
this machine.

The **project** is found from the caller's working directory: the nearest
directory at or above it that holds `.dreamer/memory` or `.git`, else the
working directory itself. A project's memory can be committed with the
project, or listed in its `.gitignore`.

Project and session scopes need context: every request carries an optional
`context` with `cwd` (an absolute directory) and `session` (an agent
session id: letters, digits, `.`, `_`, `-`). A request that names a scope
its context does not reach is refused with `INVALID_PARAMS`. A request
without a scope (`recall`, `list`) looks in every scope the context
reaches: user and host always, project with a `cwd`, session with a
`session`.

The **kind** says what a memory is about. The kinds are Claude Code's:

| Kind | For |
| --- | --- |
| `user` | The user: role, preferences, what they know |
| `feedback` | Guidance the user gave about how to work |
| `project` | Facts about the work: decisions, people, dates |
| `reference` | Where to find something outside the project |

## Files

Each memory is `<name>.md` in its scope's directory: YAML frontmatter, a
blank line, and the text.

```markdown
---
name: Small commits
description: One change per commit, subject under 60 characters
type: feedback
links:
  - docs/contributing.md
created: 2026-11-17T09:30:00Z
updated: 2026-11-17T09:30:00Z
---

Keep commits small: one change each, with a subject under 60 characters.
```

| Key | Meaning |
| --- | --- |
| `name` | The title. Without it, the file name |
| `description` | One line for the index |
| `type` | The kind; required. `kind` is read the same way |
| `links` | Paths, URLs, or other memories' ids |
| `created`, `updated` | UTC times. A memory's updated time is the later of `updated` and the file's modification time, so a hand edit counts as an update |

Other keys are kept as they are when Dreamer rewrites the file. The scope
is the directory's, not a key: moving a file to another scope's directory
moves the memory.

The memory's **id** is its scope and file name: `project/small-commits`.
New files are named from the title (lowercase letters and digits joined
by `-`); a name already taken gets `-2`, `-3`, and so on. The id stays
when the title changes.

`MEMORY.md` beside the files is the **index**, one line per memory, the
same layout Claude Code's memory uses:

```markdown
- [Small commits](small-commits.md) — One change per commit, subject under 60 characters
```

Dreamer adds a line when it remembers, rewrites a memory's line when it
updates it, and removes it when it forgets. Lines a person writes there
themselves stay. Listing and recall read the memory files, not the index,
so a file added by hand is a memory at once and shows in the index after
its next update.

**Hand edits.** Every call reads the files again; nothing is cached. An
edit, a new file, or a deleted file is what the next call sees. Writes go
through a temporary file and a rename, so a reader never sees half a file.

**Files that do not read.** A file without frontmatter, with frontmatter
that is not valid YAML, or without a known `type` is skipped and reported
in the reply's `problems` (path and reason); `dream memory` prints a note
for each. `memory.get`, `memory.update`, and `memory.forget` on such a
file fail with `PRECONDITION`, and Dreamer never changes or deletes it:
fix or remove it by hand.

## Recall

`memory.recall` finds memories by words, without a model, and the same
query over the same files always gives the same order. Text is split into
words (runs of letters and digits, lowercased). A query word hits a
memory word that equals it (2 points) or starts with it (1 point),
weighted by where the word is: title 4, description 2, text and links 1. A
memory that hits more distinct query words ranks above one that hits
fewer; then more points; then the newer; then the id. Each item carries
its `score`. An empty query returns the newest. `limit` defaults to 10 and
is at most 100.

## API

| Method | Parameters | Result |
| --- | --- | --- |
| `memory.recall` | `query`, `scope` (optional), `limit`, `context` | `MemoryItems`: matches, best first |
| `memory.remember` | `text`, `kind`, `scope`, `title`, `description`, `links`, `context` | The new `MemoryItem`, with its `id` and `path` |
| `memory.list` | `scope`, `kind` (both optional), `context` | `MemoryItems`, newest first |
| `memory.get` | `id`, `context` | `MemoryItem` |
| `memory.update` | `id`, and any of `text`, `kind`, `title`, `description`, `links` (replace), `clear_links`; `context` | The changed `MemoryItem` |
| `memory.forget` | `id`, `context` | The `MemoryItem` that was deleted |
| `memory.import` | `scope` (optional), `context` | `MemoryTransferred`: the `ids` copied from `files` into the provider in force, `problems`, `from`, `to` |
| `memory.export` | `scope` (optional), `context` | `MemoryTransferred`: the `ids` copied from the provider in force into `files` |

`remember` needs `text`, `kind`, and `scope`. Without a `title` it takes
the text's first line (up to 60 characters); without a `description`, the
title. `MemoryItems` holds `items`, `problems`, and the `provider` in
force. An id in the project or session scope needs the same `context` it
was made with. An unknown id fails with `NOT_FOUND`.

## Kosmonaut

With `memory.provider: kosmonaut` the daemon keeps memory in
[Kosmonaut](dependency-baseline.md#kosmonaut), a graph database it runs
in-process (`mode: embedded`). It needs a `dreamerd` built with the
`kosmonaut` feature (`cargo build -p dreamer-server --features kosmonaut`);
in a build without it, every memory call fails with `INVALID_PARAMS` and
says so.

**One database per user.** Every scope lives in one database,
`<data dir>/memory.kosmonaut/` (`~/.local/share/dreamer/memory.kosmonaut`
by default, or `memory.kosmonaut.path`), opened on the first memory call.
Each memory is tagged with its scope and owner: the project root for a
project memory, the session id for a session memory. A request sees only
the scopes its context reaches, so another project's memories never come
back. The API, ids, kinds, and fields are the same as with `files`;
`path` is empty.

**The graph.** A memory is a node, related to:

| Relation | To | When |
| --- | --- | --- |
| `IN_PROJECT` | the project (its root) | a project memory |
| `IN_SESSION` | the agent session | a session memory |
| `LINKS_TO` | another memory | a link that is a memory's id (`user/prefers-tabs`) |
| `ABOUT_PANE` | a pane | a link `pane:%3` or `%3` |
| `ABOUT_FILE` | a file | any other link but a URL; a relative path in a project memory is the project's |

Every relation has a valid time. When an update drops a link, its edge is
closed at that moment instead of deleted, so the database keeps what a
memory was linked to and until when. Forgetting a memory deletes it and
its edges.

**Recall** ranks by words exactly as `files` does ([Recall](#recall)), so
switching providers does not change which memories match or their order.
After the word matches, Kosmonaut adds memories one edge from the best
of them: ones that link to a match or that a match links to, and ones
about the same file or pane. These score below every word match (500,
plus 10 for each further edge). With an [embedder](#embedders), memories
whose vectors are near the query's come next (similarity of at least 0.5;
score below 500).

**Collector rows and session summaries** have tables in the same
database, linked to their pane, session, and project, so they can be
found next to the memories they concern.

### Embedders

Without an embedder there are no vectors: recall is by words and the
graph, and no model runs. `memory.kosmonaut.embedder` adds one:

| `kind` | Settings | What it does |
| --- | --- | --- |
| `static` | `path` | Reads word vectors from a file in the word2vec text format (one word and its numbers per line; a first line of count and dimension is skipped), as fastText and GloVe publish them. A text's vector is the mean of its words' vectors. Nothing runs and nothing leaves the machine |
| `local` | `command`, `dimension`, `timeout` (default 30s) | Runs a program for each batch: it reads a JSON array of strings on standard input and prints a JSON array of vectors, one per string, each `dimension` long. The program runs whatever model it likes, under the user's control |

A memory is embedded when it is remembered or changed; memories written
before the embedder was configured are embedded at the next recall that
reaches them. Vectors are kept beside the memories with the name of the
model that made them (`static:<file name>`, `local:<program>`), and only
vectors of the configured model are compared. When the embedder fails,
recall still answers by words and the graph, and its `problems` say why
there were no vectors.

Dreamer opens no network connections of its own, so it has no built-in
client for a hosted embedding API; a `local` program may call one with
the user's own key.

### Import and export

`memory.import` copies every memory the `files` provider holds in the
scopes the context reaches (or the one `scope` given) into the provider in
force; `memory.export` copies the other way. Ids, kinds, titles,
descriptions, text, links, and times are kept, and a memory with the same
id on the other side is replaced; nothing is deleted on either side. Both
are refused while `files` is the provider in force. A file that does not
read is left out and reported in `problems`.

Exported files are what `files` would have written: the same frontmatter,
the same `MEMORY.md` lines, and the file's modification time set to the
memory's updated time. Frontmatter keys Dreamer does not use are kept in
a file that is replaced, but are not carried through Kosmonaut.

```sh
dream memory import             # files -> kosmonaut, every scope this directory reaches
dream memory export --scope project
```

## Settings

```yaml
memory:
  provider: files        # default; or kosmonaut
  kosmonaut:             # used when the provider is kosmonaut
    mode: embedded       # default, and the only mode so far
    path: ~/.local/share/dreamer/memory.kosmonaut   # default: <data dir>/memory.kosmonaut
    embedder:            # optional
      kind: static
      path: ~/models/words.vec
```

A change applies to the next call, like other live settings
([settings.md](settings.md)). `memory.kosmonaut.mode: server` is refused:
only the embedded mode exists so far. So is `embedder.kind: openai` (see
[Embedders](#embedders)).

## Grants

Memory touches no pane, so the pane scope does not apply: a grant needs
only the method. Agent grants ([grants.md](grants.md#act-levels-and-approvals))
hold `memory.recall`, `memory.list`, `memory.get`, and `memory.remember`
at every level, since reading notes and adding one are not acting.
`memory.update` and `memory.forget` change or delete what may be a memory
the user wrote, so they come from `own` up. Neither asks for approval.
`memory.import` and `memory.export` are in no agent grant: they are the
owner's.

The daemon's socket belongs to the user, so `dream memory` run by the
user holds the owner grant. Run by an agent, it holds the agent's grant
instead: inside an agent session the daemon runs (`DREAMER_SESSION_ID`
and `DREAMER_SESSION_TICKET` set), it takes the session's grant; under a
harness started with `DREAMER_AGENT_LEVEL` (`dream agents pi` sets it),
it narrows to an agent's grant at that level. So an agent at `observe`
can recall and remember but not forget, whichever way it calls.

## Agents

Every harness uses memory the same way ([agents.md](agents.md#memory)):

- **Tools.** `memory_recall`, `memory_remember`, and `memory_forget`, as
  pi tools in Dreamer's pi extension and as MCP tools in `dream mcp`
  ([mcp.md](mcp.md#memory-tools)) for Claude Code, Codex, and other MCP
  clients. From a shell, `dream memory`. A tool call shows in the chat
  with what was recalled, remembered, or forgotten.
- **Context.** The project is the session's working directory; the
  session scope is the agent session's id.
- **In the chat.** pi's `/memory` lists, recalls, shows, edits, and
  forgets memories ([agents.md](agents.md#memory)).

### The index in the first prompt

A session's first prompt starts with the **index** of the scopes it
reaches (user, host, its project, and its session): a line saying what
the index is and which tools read and add memories, then one line per
memory, newest first:

```text
- project/small-commits: Small commits — One change per commit, subject under 60 characters
```

The line is the id, the title, and the description (left out when it
equals the title). It lists at most 40 memories in at most 4096 bytes,
each line at most 160 characters; a last line counts the memories left
out. With no memories there is no index. Later prompts go as they are.

For a session the daemon runs (`agent.sessions.send`) the daemon puts the
index before the first prompt; the transcript keeps the prompt as sent.
In the harness drawer, Dreamer's pi extension adds it to pi's first
prompt as a `dreamer-memory` message.

## CLI

```text
dream memory recall [QUERY...] [--scope SCOPE] [--limit N]
dream memory remember --kind KIND --scope SCOPE [--title T] [--description D] [--link L]... TEXT... | -
dream memory list [--scope SCOPE] [--kind KIND]
dream memory get ID
dream memory update ID [--text TEXT | --text -] [--kind KIND] [--title T] [--description D] [--link L]... [--clear-links]
dream memory forget ID
dream memory import [--scope SCOPE]
dream memory export [--scope SCOPE]
```

Every command takes `--json` (the API's reply), `--cwd DIR` (default: the
current directory, which picks the project), and `--session ID` (default:
`$DREAMER_SESSION_ID`, which the daemon sets for the harnesses it starts).
`-` reads the text from standard input; text after `--` is never read as
an option.

```sh
dream memory remember --kind feedback --scope project --title "Small commits" -- Keep commits small
dream memory recall commits
dream memory forget project/small-commits
```
