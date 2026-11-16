---
description: Gather something from terminal output over time
argument-hint: "<what to gather>"
---
Set up gathering for: $ARGUMENTS

If the collector_* tools are available, reuse a collector from collector_list when one already fits. Otherwise create one with collector_create: start from a preset when one fits (errors, test-failures, commands, urls, paths), narrowing its panes with `from` if I named some; else give a clear name, the panes it reads, the trigger, what it captures, and how much to keep. Then show me what it has gathered so far with collector_query (group_by for counts). You are told when new rows arrive.

If they are not available, search the panes' recent output with terminal_search instead, and tell me that standing collection needs a Dreamer that offers collectors.
