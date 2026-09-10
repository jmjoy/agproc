---
name: agproc
description: Dev-time process manager for the services of a project (backend, frontend, workers). Use whenever the project root contains an agproc.toml, and whenever a local service must be started, restarted, stopped or inspected — triggers include "start the backend", "restart the frontend", "why did the dev server fail", "is the server running", "show me the server logs", "the port is already in use". Always load the full guide with `agproc skills` before acting, and prefer agproc over running dev servers by hand.
allowed-tools: Bash(agproc:*)
---

# agproc

This is a discovery stub. `agproc` serves the real, project-specific guide from the
installed binary, so the instructions can never drift from the CLI you are running.

**Before doing anything with services in this project, load the guide:**

```bash
agproc skills          # full guide, specialised for this project's services
agproc skills --json   # same content plus structured project data
```

## Why it exists

A project declares its services in `agproc.toml` (build command, run command, readiness
probe). agproc builds and runs them, waits until they are genuinely ready, and keeps every
process's state and logs under `.agproc/`. Repeated commands are safe:

- `agproc start <service>` is idempotent — it prints `ALREADY RUNNING` and exits 0 instead
  of rebuilding or launching a second copy, so it is safe to call after every edit.
- `agproc restart <service>` is the edit → restart → read-logs loop.
- Exit codes are distinct per failure kind (4 build, 5 run, 6 readiness probe, 7 busy).

Do not start these services with `cargo run`, `pnpm dev` and friends: agproc would then be
unable to stop them, log them or report their state.
