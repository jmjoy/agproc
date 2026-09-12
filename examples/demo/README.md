# agproc demo

A two-service project: a Rust backend (`cargo build` then run) and a static
frontend served by `python3 -m http.server` — deliberately a **non-watching**
run command, so every `agproc restart` is meaningful.

```bash
cd examples/demo

agproc start                 # both services, prefixed output, waits for the probe
agproc ps                    # running / pid / uptime
curl http://127.0.0.1:38080/healthz
agproc start backend         # -> ALREADY RUNNING, exit 0, no rebuild
agproc logs frontend -f      # follow; returns when the service stops
agproc restart backend       # stop -> cargo build -> run -> probe
agproc stop                  # nothing left running
```

Watch the exit codes: `start` returns 4 for a build failure, 5 when the process
dies before the probe passed, 6 when the probe fails.

The backend service loads `env-file = ".env"`, a path relative to the project
root — hence `examples/demo/.env`, not `examples/demo/backend/.env`. Its
`DEMO_GREETING` shows up in the startup log as `demo-backend greeting: ...`.
Edit that file and `agproc ps` reports `config changed since start`; run
`agproc restart backend` to apply it.

> This demo lives in the Git repository only. It contains a nested crate
> (`backend/`), which cargo never packages, so the published `agproc` crate
> excludes `examples/` entirely rather than shipping a demo without its backend.
