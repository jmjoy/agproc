---
name: agproc
description: Dev-time process manager for a project's services. Use whenever the project root contains an agproc.toml or local services need attention. Before acting, run `agproc skills`; prefer agproc over invoking development servers by hand.
allowed-tools: Bash(agproc:*)
---

# agproc

This is a discovery stub, not the operating manual. The installed `agproc` binary
serves the complete, version-matched guide and the current project's service table.

## Required first step

Before any service action, load the guide:

```bash
agproc skills
```

Use `agproc skills --json` only when structured project data is needed.

## Safety boundary

When an `agproc.toml` is present, do not start the project's development servers
directly (for example, with `cargo run` or `pnpm dev`). Load the guide first, then
follow its instructions for all service operations.
