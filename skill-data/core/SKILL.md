# agproc — dev-time process manager

agproc owns the long-running processes of this project (backend, frontend, workers...).
It builds them, runs them, waits until they are actually ready, and keeps every
process's state and logs under `.agproc/`, so calling it repeatedly is safe.

## The five rules

1. **Never start a service by hand.** `cargo run`, `pnpm dev`, `python3 main.py` bypass
   agproc and produce processes agproc cannot stop, log or report on. Use `agproc start`.
2. **`agproc start <service>` is idempotent.** If the service is already running it prints
   `ALREADY RUNNING` and exits 0 *without* rebuilding or restarting anything.
3. **After changing code, restart the service**: `agproc restart <service>`. That is the
   intended loop — edit, restart, read logs. `start` never re-runs an already-running service.
4. **Trust the exit code, not the prose.** Every failure has a distinct exit code (below).
5. **`agproc logs` is the service's output and nothing else.** It replays the last run-cmd's
   stdout/stderr (`.agproc/logs/<service>.stdout.log` and `.stderr.log`) — no agproc markers, no
   build output, no earlier runs. What you saw on the console of `start` is not repeated there.

## The intended agent loop

```bash
# after editing backend code
agproc restart backend            # stop + build + run + wait for the probe
echo $?                           # 0 = ready, 4 = build failed, 5 = run failed, 6 = probe failed
agproc logs backend --tail 60     # only when the exit code says something went wrong

# frontend: same, restart after edits
agproc restart frontend

# what is running right now?
agproc ps
agproc ps --json                  # stable machine-readable form

# live tail while a service starts or misbehaves
agproc logs backend -f            # returns by itself when the service stops

# when you are done
agproc stop                       # or: agproc stop backend
```

`agproc start` (no service name) starts **every** service in parallel and prefixes each
line with the service name, padded to the longest one so the `|` columns line up
(`backend  | ...` next to `frontend | ...`); with a single service there is no prefix.

## Commands

| Command | What it does |
| --- | --- |
| `agproc start [service...]` | build (if any) + run + wait for the probe. No-op when already running |
| `agproc restart [service...]` | stop, then build + run + wait for the probe |
| `agproc stop [service...]` | stop a running service or cancel a build that is in progress |
| `agproc ps [service...] [--json]` | what is running, with phase, pid, uptime and reason |
| `agproc logs [service...] [--tail N] [-f] [--stream both\|stdout\|stderr]` | replay or follow the last run-cmd's output |
| `agproc skills` | this document, specialised for this project |
| `agproc init [--force]` | write an `agproc.toml` template |

Flags worth knowing:

- `agproc start --timeout-seconds N` bounds how long the *command* waits. On timeout it
  prints `STILL STARTING` and exits 1, but the service keeps building/running in the
  background — check `agproc ps` instead of starting a second one.
- `agproc logs` replays the **last run-cmd's** output only, with `--tail N` keeping the last N lines
  of each stream. `-f` returns as soon as the service stops, so it never hangs.
- Build output is shown live by `start`/`restart` but is not kept in the service logs. After a failed
  build, read it back from `.agproc/tmp/<service>.console.stdout` (the transient console stream).
- stdout goes to agproc's stdout and stderr to agproc's stderr; with several services every line
  gets a `name |` prefix (name padded to the longest one). Both streams share that prefix, so read
  the stream — not the text — to tell them apart.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | success: ready / already running / stopped / ps / logs / skills |
| 1 | generic error, including "the CLI gave up waiting" |
| 2 | usage error |
| 3 | configuration error (missing or invalid `agproc.toml`, unknown service name) |
| 4 | build failed (non-zero exit or timeout) |
| 5 | run failed (the process exited before becoming ready) |
| 6 | probe failed |
| 7 | another start/restart is in progress for this service |
| 8 | this start was superseded (the service was stopped by another agproc call) |

## Reading the log markers

agproc's own lines always look like `===== LIKE THIS =====`:

```text
===== BUILDING =====              build-cmd started
===== BUILD SUCCEED =====         build-cmd exited 0
===== BUILD FAILED (exit code N) =====
===== RUNNING =====               run-cmd started, probing
===== PROBE ATTEMPT 2/3 FAILED: connection refused (http://127.0.0.1:3000/healthz) =====
===== PROBE PASSED (attempt 2) =====
===== PROBE FAILED: 3 consecutive failures, last: ... =====
===== RUNNING FAILED (exit code N) =====    run-cmd died before the probe passed
===== SERVICE EXITED (exit code N, ready for 12s) =====   it ran, then stopped
===== STOPPED =====
===== ALREADY RUNNING (pid N, uptime 2m3s, ready) =====
===== START IN PROGRESS (pid N, phase building) =====
===== WARNING: PORT 3000 ALREADY IN USE BY pid 1234 (node) =====
===== START FAILED: frontend (probe failed) =====
```

## Service phases (`agproc ps`)

`building`, `starting` (running, not yet ready), `running`, `build failed`, `run failed`,
`running failed` (it was ready and then exited), `probe failed`, `stopped`,
`stale` (the supervisor was killed; the next `start` reaps the leftovers and rebuilds).

## Troubleshooting

- **`probe failed`** — the log names the last probe error. `connection refused`
  means nothing was listening yet or the process died; `HTTP 503` means the service
  answered "not ready". If the process is still alive the probe simply gave up after
  `failure-threshold` attempts; fix the code and `agproc restart <service>`.
- **`PORT nnnn ALREADY IN USE BY pid N` followed by `PROBE FAILED: port ... is
  owned by ...`** — a foreign process holds the probe port. agproc refuses to report
  success against someone else's process. Free the port (or change it in `agproc.toml`).
- **exit 7** — a start/restart is already running for that service. Wait for it, or
  `agproc stop <service>` first; do not fire a second `start` in a loop.
- **`stale`** — the supervisor process was killed. Run `agproc start <service>`: it kills
  the leftover process group and builds again.
- **Build output missing?** Most build tools write progress to stderr, which agproc keeps
  on its own stderr; run `agproc logs <service> --stream stderr` if you filtered it out.

## Anti-patterns

- Calling `agproc start` to force a rebuild — use `agproc restart`.
- Starting services yourself and then wondering why `agproc ps` does not list them.
- Editing `agproc.toml` and expecting a running service to pick it up: `ps` marks
  `[config changed since start]`; run `agproc restart <service>`.
- Busy-looping on `agproc ps` right after `start`: `start` already waits for the probe.

<!-- agproc:project -->
