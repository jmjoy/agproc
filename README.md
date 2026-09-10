# agproc

**English** | [简体中文](README.zh-CN.md)

> A **dev-time process manager** for AI agents and humans alike (Linux only, for now)

Declare your project's long-running services (backend / frontend / workers) in `agproc.toml`;
agproc takes care of **build → run → readiness probe** and keeps every process's state and logs
under `.agproc/`, so calling it repeatedly is safe for both humans and agents.

```bash
agproc start              # start every service in parallel, wait for readiness
agproc ps                 # what is running: pid, uptime, why it failed
agproc restart backend    # the standard move after editing backend code
agproc logs -f            # live logs, prefixed per service
agproc stop               # stop everything
```

## Why it exists

| Problem | What agproc does |
|---|---|
| An agent edits code constantly, retriggering builds and restarts — sometimes ending up with two dev servers | `start` is **idempotent**: when the service is already running it prints `ALREADY RUNNING` and exits 0 without building or restarting anything. Pair it with a **non-watching** `run-cmd` (e.g. `vite preview`) and restart explicitly after edits, so every step is predictable |
| Rust projects must compile before they run, and failures are scattered around | `build-cmd` and `run-cmd` are separate; a failed build gets its own marker and **exit code 4**, so an agent needs no text parsing |
| "The process is still alive" is a poor readiness signal | Built-in `http-get` / `tcp-connect` readiness probes; failure is only declared after `failure-threshold` retries (**exit code 6**) |
| A leftover process holds the port, so the probe "passes" against someone else's process | Port preflight warning + **ownership verification** at the moment readiness is reported + child-exit-first ordering. It never reports a false success |
| Piped output gets block buffered and the pre-readiness logs never arrive | One pty per stream keeps the child **line buffered**, so output shows up as it happens |

## Install

```bash
cargo install agproc          # from crates.io (needs Rust 1.89+)

# or from a checkout
cargo install --path .
# or just build the binary
cargo build --release && install -m755 target/release/agproc ~/.local/bin/
```

## Quick start

```bash
cd your-project
agproc init            # write an agproc.toml template (detects Cargo.toml / package.json)
$EDITOR agproc.toml    # fill in build-cmd / run-cmd / readiness-probe
agproc start           # start everything and wait for readiness
```

## Commands

```
agproc start   [service...] [--timeout-seconds N]   # build + run + wait for readiness; no-op when already running
agproc restart [service...] [--timeout-seconds N]   # stop first, then build + run + wait for readiness
agproc stop    [service...]                         # stop a running service or cancel a build in progress
agproc ps      [service...] [--json]                # what is running
agproc logs    [service...] [--tail N] [-f] [--stream both|stdout|stderr] [--all]
agproc skills  [--json]                             # the project-aware guide for agents
agproc init    [--force]                            # write a config template
```

- Omitting `[service...]` means **every** service; several names may be given at once.
- `-C/--config <PATH>` or `AGPROC_CONFIG` selects the config file. Otherwise agproc searches
  upwards from the current directory for `agproc.toml` (like git and cargo) and creates `.agproc/`
  next to it.
- `--timeout-seconds` bounds **the command's wait only**: on timeout it prints `STILL STARTING` and
  exits 1, but the **background runner keeps working** — keep tracking it with `ps` / `logs`.
- `agproc logs` shows only the **most recent session** unless `--all` is given, and `-f` **returns by
  itself** once the service stops, so it never hangs an agent.

## Configuration: `agproc.toml`

```toml
[settings]
shell = "sh"                      # string commands run as <shell> -c "<cmd>"
stop-timeout-seconds = 10         # SIGTERM -> SIGKILL grace period
log-max-bytes = 33554432          # rotate a log past 32 MiB into <name>.1 (0 = never)
port-check = true                 # port preflight + ownership verification

[[service]]
name = "backend"                  # required, unique, [A-Za-z0-9._-]
cwd = "."                         # optional, relative to the project root
env = { RUST_LOG = "debug" }      # optional, merged into the inherited environment
build-cmd = "cargo build"         # optional; without it the BUILD phase is skipped
build-timeout-seconds = 0         # optional; 0 = no limit
run-cmd = "./target/debug/api"    # required; string = <shell> -c, array = exec directly
stop-timeout-seconds = 10         # optional, overrides [settings]

readiness-probe = {               # optional; without it "still alive" means ready
  http-get = { scheme = "http", host = "127.0.0.1", port = 3000, path = "/healthz" },
  initial-delay-seconds = 1,      # wait before the first attempt
  period-seconds = 1,             # interval between attempts
  timeout-seconds = 2,            # per-attempt timeout
  failure-threshold = 3,          # consecutive failures before giving up
}
```

A plain TCP connectivity probe works just as well:

```toml
readiness-probe = { tcp-connect = { host = "127.0.0.1", port = 5173 },
                    initial-delay-seconds = 1, period-seconds = 1,
                    timeout-seconds = 2, failure-threshold = 3 }
```

Rules:

- Every key is kebab-case and **unknown keys are rejected** — configs are often written by agents,
  and a typo must fail immediately instead of silently doing nothing.
- `http-get` supports `http` only (local dev endpoints); `https` fails at load time. `2xx/3xx` count
  as ready.
- Multiple services are started **in parallel**.

## Log conventions

Everything agproc says itself looks like `===== LIKE THIS =====`:

```
===== BUILDING =====
===== BUILD SUCCEED =====            # on failure: ===== BUILD FAILED (exit code 101) =====
===== RUNNING =====
===== PROBE ATTEMPT 2/3 FAILED: connection refused (http://127.0.0.1:3000/healthz) =====
===== READINESS PROBE PASSED (attempt 2) =====
===== READINESS PROBE FAILED: 3 consecutive failures, last: ... =====
===== RUNNING FAILED (exit code 1) =====
===== SERVICE EXITED (exit code 0, ready for 12s) =====
===== STOPPED ===== / ===== ALREADY RUNNING (pid 1234, uptime 2m3s, ready) =====
===== START IN PROGRESS (pid 1234, phase building) =====
===== WARNING: PORT 3000 ALREADY IN USE BY pid 614089 (node) =====
```

**Streams stay separate**: a child's stdout goes to agproc's stdout and its stderr to agproc's
stderr, unchanged. A single service gets no prefix (so it can be piped); with several services the
prefixes are `backend | ` and `backend stderr | `.

> How it manages to be both live *and* split: stdout and stderr each get their own pty, so the child
> believes it is on a terminal and stays **line buffered**. Redirect straight to a file or pipe and
> runtimes such as Python switch to block buffering — measured: the file was still 0 bytes after
> 1.2 seconds.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success: ready / already running / stopped / ps / logs / skills |
| 1 | generic error, including "the CLI gave up waiting" |
| 2 | usage error |
| 3 | configuration error (missing or invalid `agproc.toml`, unknown service name) |
| 4 | build failed (non-zero exit or timeout) |
| 5 | run failed (the process exited before readiness) |
| 6 | readiness probe failed |
| 7 | another start/restart is already in progress for this service |
| 8 | this start was superseded (e.g. interrupted by `stop`) |

## States (`agproc ps`)

`building`, `starting` (running, not yet ready), `running`, `build failed`, `run failed`,
`running failed` (exited after being ready), `readiness probe failed`, `stopped`, and
`stale` (the runner was `kill -9`ed; the next `start` reaps the leftover process group and rebuilds).

`agproc ps --json` emits a stable shape (`phase` / `pid` / `runner_pid` / `child_pid` /
`uptime_seconds` / `build_exit_code` / `run_exit_code` / `probe{kind,target,attempts,last_error}` /
`config_changed` / `log_stdout` / `log_stderr`).

## The `.agproc/` directory

```
.agproc/
├── logs/<service>.stdout.log      # phase markers + child stdout (split by session)
├── logs/<service>.stderr.log      # child stderr
├── state/<service>.json           # runtime state (atomic writes; the source of truth for ps/logs)
├── lock/<service>.lock            # orchestration lock (flock)
└── tmp/                           # temp files for atomic writes
```

`agproc init` appends `.agproc/` to `.gitignore`.

## For AI agents

The repository ships a [skills.sh](https://www.skills.sh/)-compatible skill:

```bash
npx skills add jmjoy/agproc      # installs skills/agproc/SKILL.md (a discovery stub)
agproc skills                    # full guide + this project's real service table
agproc skills --json             # same content plus structured data
```

The skill's core discipline: **when the project root has an `agproc.toml`, do not start services with
`pnpm dev` / `cargo run`**. Run `agproc skills` first, manage processes through agproc, `restart`
after edits, and on failure read the exit code before reading the logs.

## Design notes

- **One detached runner per service** (`agproc __runner`, `setsid`): no central daemon, no socket.
  State and logs are ordinary files under `.agproc/`, so an agent can `cat`/`grep` them directly, and
  killing the CLI cannot disturb a build or probe that is already running.
- **The runner is the single authority on the child's lifecycle**: an independent `waitpid` runs
  alongside the probe loop and records `run-failed` the moment the child exits, outranking any probe
  result — a probe can never report a dead service as ready.
- **Port ownership in three layers**: before `run-cmd` starts, `/proc/net/tcp{,6}` is scanned to
  record (and warn about) an existing listener; the instant a probe succeeds, ownership is verified
  (still the same foreign process → fail; a container runtime publishing on our behalf → warn only);
  and a child exit always takes precedence.
- **PID-reuse safe**: state records the runner's `/proc/<pid>/stat` start time, which `ps` checks.
- **Orchestration lock + generation**: only one start/restart per service at a time, while `stop`
  never takes the lock so it can always interrupt.

## Explicitly out of scope (v0.1)

`depends-on` ordering (everything starts in parallel today), automatic restart on crash,
watch/HMR awareness, `https` probes, macOS/Windows.

## License

Licensed under the [Mulan Permissive Software License, Version 2](LICENSE) (`MulanPSL-2.0`).
