---
name: dreamer-pi-profile
description: Change your own pi setup inside Dreamer - write a new skill, prompt template, or extension (a TypeScript tool or command), or edit settings - so it is versioned, checked, and loaded. Use when the user asks you to learn a workflow, add a command or tool, remember how to do something, or change how you behave.
---

# Improving your own profile

Your agent directory (`$PI_CODING_AGENT_DIR`) is Dreamer's pi profile: a
git repository. Dreamer ships the base layer (`extensions/dreamer/`,
the `dreamer-*` skills, `prompts/panes.md`, `prompts/collect.md`,
`prompts/apps.md`, `APPEND_SYSTEM.md`, `settings.json`). Everything else
is yours to add, and you may edit the shipped files too: your version is
kept when Dreamer updates.

| To add | Write |
| --- | --- |
| A skill | `skills/NAME/SKILL.md`, with `name` and `description` front matter; the description says what it does and when to use it |
| A slash command that expands to a prompt | `prompts/NAME.md`, with `description` front matter; `$1`, `$ARGUMENTS` take arguments |
| A tool or command in code | `extensions/NAME/index.ts`, exporting `default function (pi: ExtensionAPI)`; TypeScript, loaded by pi without a build step |
| A setting | `settings.json` (pi's settings; keep it valid JSON) |

Read pi's own documentation for the details before writing an extension:
`docs/extensions.md` beside the pi executable (`dirname $(readlink -f $(which pi))`
or Dreamer's install, `dream agents status` shows where).

## The loop

1. Write or edit the files in the profile.
2. Call `dreamer_profile_commit` with a one-line message saying what
   changed and why. It commits the change with this session's id and
   starts pi once to check that it still loads. Set `reload: true` to
   load the change into this session after your reply.
3. If the check fails, fix the file and commit again. A change that
   stops pi from loading is reverted automatically the next time Dreamer
   starts pi, so a broken change is lost.

The user can see and undo every change: `dream agents profile log`,
`dream agents profile diff REV`, `dream agents profile revert REV`.

## Rules

- Extensions run with the user's privileges. Write only what the user
  asked for, and say what a new tool can do.
- Never read, print, copy, or commit `auth.json` or any other
  credential. The repository ignores everything outside the profile's
  resources.
- Do not edit `.dreamer/` or the git history by hand.
- An extension must not open sockets, start processes, or set timers
  when it loads; start them in `session_start` or when a tool runs.
