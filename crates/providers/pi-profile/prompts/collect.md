---
description: Gather something from terminal output over time
argument-hint: "<what to gather>"
---
Set up gathering for: $ARGUMENTS

If the collector_* tools are available, create a collector with collector_create (a clear name, the panes it reads, the trigger, what it captures, and how much to keep), then show me what it has gathered so far with collector_query. Reuse a collector from collector_list when one already fits.

If they are not available, search the panes' recent output with terminal_search instead, and tell me that standing collection needs a Dreamer that offers collectors.
