//! End-to-end tests: they run the real binary against throwaway projects in
//! temporary directories, and always clean up after themselves.

use serde_json::Value;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agproc"))
}

/// A throwaway project whose services are stopped when the test ends.
struct Project {
    dir: tempfile::TempDir,
    _stop_on_drop: (),
}

impl Project {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("agproc.toml"), config).expect("write config");
        Self {
            dir,
            _stop_on_drop: (),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        std::fs::write(self.file(relative), contents).expect("write file");
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.file(relative))
            .unwrap_or_else(|err| panic!("cannot read {relative}: {err}"))
    }

    fn agproc(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .current_dir(self.dir.path())
            .args(args)
            .output()
            .expect("run agproc")
    }

    /// `agproc` with stdout and stderr merged, which is how an agent harness
    /// usually sees it.
    fn combined(&self, args: &[&str]) -> (i32, String) {
        let output = self.agproc(args);
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.code().unwrap_or(-1), text)
    }

    fn state(&self, service: &str) -> Value {
        let raw = std::fs::read_to_string(self.file(&format!(".agproc/state/{service}.json")))
            .unwrap_or_else(|err| panic!("cannot read state of {service}: {err}"));
        serde_json::from_str(&raw).expect("valid state json")
    }

    fn ps_json(&self) -> Value {
        let (code, text) = self.combined(&["ps", "--json"]);
        assert_eq!(code, 0, "ps --json failed: {text}");
        serde_json::from_str(&text).expect("valid ps json")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.agproc(&["stop"]);
        // Belt and braces: kill anything the state files still point at.
        for entry in std::fs::read_dir(self.file(".agproc/state"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            for key in ["runner-pid", "child-pid"] {
                if let Some(pid) = state.get(key).and_then(Value::as_u64) {
                    let _ = Command::new("kill")
                        .args(["-9", &pid.to_string()])
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
    }
}

/// A port that was free a moment ago. Tests allocate their own so they can run
/// in parallel.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn has(text: &str, needle: &str) -> bool {
    assert!(
        text.contains(needle),
        "expected {needle:?} in output:\n{text}"
    );
    true
}

/// Wait until a predicate holds, failing the test on timeout.
fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {label}");
}

// ---------------------------------------------------------------------------
// happy path
// ---------------------------------------------------------------------------

#[test]
fn starts_all_services_with_prefixes_and_stops_them() {
    let project = Project::new(
        r#"
[settings]
stop-timeout-seconds = 2

[[service]]
name = "backend"
build-cmd = ["echo", "building-backend"]
run-cmd = ["sh", "-c", "echo serving-backend; sleep 120"]

[[service]]
name = "frontend"
build-cmd = ["echo", "building-frontend"]
run-cmd = ["sh", "-c", "echo serving-frontend; sleep 120"]
"#,
    );

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "start failed:\n{text}");
    for marker in [
        "===== BUILDING =====",
        "===== BUILD SUCCEED =====",
        "===== RUNNING =====",
        "===== PROBE PASSED (NO PROBE CONFIGURED) =====",
    ] {
        assert!(has(&text, marker), "missing {marker}");
    }
    // Multi-service invocations label every line, with the name padded to the
    // longest one (`frontend`) so the `|` columns line up.
    has(&text, "backend  | ===== BUILDING =====");
    has(&text, "frontend | ===== RUNNING =====");
    has(&text, "backend  | serving-backend");
    has(&text, "frontend | serving-frontend");

    let ps = project.ps_json();
    for name in ["backend", "frontend"] {
        let service = ps["services"]
            .as_array()
            .expect("services array")
            .iter()
            .find(|service| service["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed in {ps}"));
        assert_eq!(service["phase"], "running", "{name} not running: {ps}");
        assert!(service["child_pid"].as_u64().unwrap_or(0) > 0);
    }

    let (code, text) = project.combined(&["stop"]);
    assert_eq!(code, 0, "stop failed:\n{text}");
    has(&text, "backend  | ===== STOPPED =====");
    has(&text, "frontend | ===== STOPPED =====");

    let ps = project.ps_json();
    for service in ps["services"].as_array().expect("services array") {
        assert_eq!(service["phase"], "stopped", "{service}");
    }
    // Idempotent: stopping again is not an error.
    let (code, text) = project.combined(&["stop"]);
    assert_eq!(code, 0);
    has(&text, "===== NOT RUNNING =====");
}

#[test]
fn multi_service_prefixes_are_padded_and_shared_by_both_streams() {
    let project = Project::new(
        r#"
[settings]
stop-timeout-seconds = 2

[[service]]
name = "backend"
run-cmd = ["sh", "-c", "echo out-backend; echo err-backend >&2; sleep 120"]

[[service]]
name = "frontend"
run-cmd = ["sh", "-c", "echo out-frontend; sleep 120"]
"#,
    );

    let output = project.agproc(&["start"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The shorter name is padded to the width of the longest one.
    has(&stdout, "backend  | out-backend");
    has(&stdout, "frontend | out-frontend");
    assert!(
        !stdout.contains("backend | "),
        "a short service name must be padded:\n{stdout}"
    );

    // stderr carries exactly the stdout prefix: no textual stream marker.
    has(&stderr, "backend  | err-backend");
    assert!(
        !stderr.contains("stderr | "),
        "stderr lines must not be labelled:\n{stderr}"
    );

    let (code, text) = project.combined(&["stop"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "backend  | ===== STOPPED =====");
    has(&text, "frontend | ===== STOPPED =====");
}

#[test]
fn start_is_idempotent_and_restart_rebuilds() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
build-cmd = ["echo", "build-run"]
run-cmd = ["sh", "-c", "echo serve-run; sleep 120"]
"#,
    );

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");
    let first_pid = project.state("api")["child-pid"].as_u64().expect("child pid");

    // A second start must not rebuild, restart or complain.
    let (code, text) = project.combined(&["start", "api"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "===== ALREADY RUNNING");
    assert!(!text.contains("BUILDING"), "start rebuilt an running service:\n{text}");
    assert_eq!(
        project.state("api")["child-pid"].as_u64(),
        Some(first_pid),
        "start replaced the running process"
    );

    // Restart stops it and runs the whole sequence again.
    let (code, text) = project.combined(&["restart", "api"]);
    assert_eq!(code, 0, "{text}");
    let stopped = text.find("===== STOPPED =====").expect("STOPPED marker");
    let building = text.find("===== BUILDING =====").expect("BUILDING marker");
    assert!(stopped < building, "restart did not stop first:\n{text}");
    has(&text, "===== BUILD SUCCEED =====");
    has(&text, "===== PROBE PASSED");
    let second_pid = project.state("api")["child-pid"].as_u64().expect("child pid");
    assert_ne!(first_pid, second_pid, "restart kept the old process");
}

// ---------------------------------------------------------------------------
// streams
// ---------------------------------------------------------------------------

#[test]
fn stdout_and_stderr_stay_separate() {
    let project = Project::new(
        r#"
[[service]]
name = "chatty"
run-cmd = ["sh", "-c", "echo only-on-stdout; echo only-on-stderr >&2; sleep 120"]
"#,
    );

    let output = project.agproc(&["start"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    has(&stdout, "only-on-stdout");
    has(&stderr, "only-on-stderr");
    assert!(
        !stdout.contains("only-on-stderr"),
        "stderr leaked into stdout:\n{stdout}"
    );
    assert!(
        !stderr.contains("only-on-stdout"),
        "stdout leaked into stderr:\n{stderr}"
    );
    // agproc's own narrative stays on stdout.
    has(&stdout, "===== PROBE PASSED");
}

// ---------------------------------------------------------------------------
// failures
// ---------------------------------------------------------------------------

#[test]
fn build_failure_reports_exit_code_4() {
    let project = Project::new(
        r#"
[[service]]
name = "badbuild"
build-cmd = ["sh", "-c", "echo compile-error; exit 101"]
run-cmd = ["sleep", "120"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 4, "{text}");
    has(&text, "===== BUILD FAILED (exit code 101) =====");
    has(&text, "===== START FAILED: badbuild (build failed) =====");
    assert_eq!(project.state("badbuild")["phase"], "build-failed");
}

#[test]
fn run_failure_reports_exit_code_5() {
    let project = Project::new(
        r#"
[[service]]
name = "instant"
build-cmd = ["true"]
run-cmd = ["sh", "-c", "echo crashing; exit 3"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 5, "{text}");
    has(&text, "===== RUNNING FAILED (exit code 3) =====");
    assert_eq!(project.state("instant")["phase"], "run-failed");
}

#[test]
fn probe_failure_reports_exit_code_6_and_stops_the_child() {
    let port = free_port();
    let project = Project::new(&format!(
        r#"
[[service]]
name = "hopeless"
build-cmd = ["true"]
run-cmd = ["sh", "-c", "echo alive-but-not-listening; sleep 120"]
probe = {{ tcp-connect = {{ port = {port} }}, initial-delay-seconds = 1, period-seconds = 1, timeout-seconds = 1, failure-threshold = 2 }}
"#
    ));
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 6, "{text}");
    has(&text, "===== PROBE ATTEMPT 1/2 FAILED: connection refused");
    has(&text, "===== PROBE FAILED: 2 consecutive failures");
    assert_eq!(project.state("hopeless")["phase"], "probe-failed");

    // The half-ready child must not be left behind.
    let child = project.state("hopeless")["child-pid"].as_u64();
    wait_for("the failed child to disappear", || {
        child.map(|pid| !Path::new(&format!("/proc/{pid}")).exists()) == Some(true)
    });
}

#[test]
#[allow(clippy::zombie_processes)] // the test process holds the port on purpose
fn a_foreign_listener_on_the_probe_port_is_never_reported_as_ready() {
    // Someone else already owns the port and answers the probe: the classic way
    // a probe can lie.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 512];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        }
    });

    let project = Project::new(&format!(
        r#"
[[service]]
name = "victim"
run-cmd = ["sh", "-c", "echo pretending-to-serve; sleep 120"]
probe = {{ http-get = {{ port = {port}, path = "/healthz" }}, initial-delay-seconds = 1, period-seconds = 1, timeout-seconds = 2, failure-threshold = 2 }}
"#
    ));

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 6, "a foreign listener must not count as ready:\n{text}");
    has(&text, "===== WARNING: PORT");
    has(&text, "ALREADY IN USE BY");
    has(&text, "is owned by");
    assert!(!text.contains("PROBE PASSED"), "{text}");
    assert_eq!(project.state("victim")["phase"], "probe-failed");
}

// ---------------------------------------------------------------------------
// locking, staleness, lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_second_start_reports_the_running_one_instead_of_racing() {
    let project = Project::new(
        r#"
[[service]]
name = "slow"
build-cmd = ["sh", "-c", "echo building; sleep 4; echo built"]
run-cmd = ["sh", "-c", "echo serving; sleep 120"]
"#,
    );

    let dir = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        Command::new(bin())
            .current_dir(&dir)
            .args(["start", "slow"])
            .output()
            .expect("first start")
    });

    // Wait until the build is genuinely in flight, then race a second start.
    let state_path = project.file(".agproc/state/slow.json");
    wait_for("the build to start", || {
        std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .map(|state| state["phase"] == "building")
            .unwrap_or(false)
    });

    let (code, text) = project.combined(&["start", "slow"]);
    assert_eq!(code, 7, "concurrent start must be rejected:\n{text}");
    has(&text, "===== START IN PROGRESS");

    let first = handle.join().expect("first start thread");
    assert_eq!(first.status.code(), Some(0));
}

#[test]
fn stop_cancels_a_build_in_progress() {
    let project = Project::new(
        r#"
[settings]
stop-timeout-seconds = 2

[[service]]
name = "slowbuild"
build-cmd = ["sh", "-c", "echo building-slowly; sleep 60; echo never"]
run-cmd = ["sleep", "60"]
"#,
    );

    let dir = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        Command::new(bin())
            .current_dir(&dir)
            .args(["start", "slowbuild"])
            .output()
            .expect("start")
    });

    let state_path = project.file(".agproc/state/slowbuild.json");
    wait_for("the build to start", || {
        std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .map(|state| state["phase"] == "building")
            .unwrap_or(false)
    });

    let (code, text) = project.combined(&["stop", "slowbuild"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "===== STOPPED");

    // The waiting start reports that it was superseded, not that it succeeded.
    let waiting = handle.join().expect("start thread");
    assert_eq!(waiting.status.code(), Some(8));

    let build_pid = project.state("slowbuild")["child-pid"].as_u64();
    if let Some(pid) = build_pid {
        wait_for("the cancelled build to exit", || {
            !Path::new(&format!("/proc/{pid}")).exists()
        });
    }
}

#[test]
fn a_killed_runner_is_reported_as_stale_and_start_recovers() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
run-cmd = ["sh", "-c", "echo serving; sleep 120"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");

    let runner = project.state("api")["runner-pid"].as_u64().expect("runner pid");
    let killed = Command::new("kill")
        .args(["-9", &runner.to_string()])
        .status()
        .expect("kill runner");
    assert!(killed.success());

    wait_for("the state to look stale", || {
        project.ps_json()["services"][0]["phase"] == "stale"
    });

    // Recovery: start must reap the leftover process group and succeed.
    let (code, text) = project.combined(&["start", "api"]);
    assert_eq!(code, 0, "start did not recover from a killed runner:\n{text}");
    assert_eq!(project.ps_json()["services"][0]["phase"], "running");
}

#[test]
fn cli_timeout_leaves_the_runner_working() {
    let project = Project::new(
        r#"
[[service]]
name = "slowpoke"
build-cmd = ["sh", "-c", "sleep 5; echo built"]
run-cmd = ["sh", "-c", "echo serving; sleep 120"]
"#,
    );
    let (code, text) = project.combined(&["start", "--timeout-seconds", "1"]);
    assert_eq!(code, 1, "{text}");
    has(&text, "===== STILL STARTING");

    // The runner keeps going, so the service eventually comes up on its own.
    wait_for("the service to finish starting", || {
        let ps = project.ps_json();
        ps["services"][0]["phase"] == "running"
    });
}

// ---------------------------------------------------------------------------
// logs, ps, skills, init
// ---------------------------------------------------------------------------

#[test]
fn stop_signals_every_service_concurrently() {
    // Both services need 2s to drain on SIGTERM. A sequential `stop` would
    // signal them ~2s apart; in parallel the signals land together.
    let project = Project::new(
        r#"
[settings]
stop-timeout-seconds = 10

[[service]]
name = "a"
run-cmd = ["sh", "slow-stop.sh", "a"]

[[service]]
name = "b"
run-cmd = ["sh", "slow-stop.sh", "b"]
"#,
    );
    project.write(
        "slow-stop.sh",
        "name=\"$1\"\n\
         trap 'date +%s.%N > drain-'\"$name\"'.stamp; sleep 2; exit 0' TERM\n\
         while true; do sleep 0.2; done\n",
    );

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");

    let started = Instant::now();
    let (code, text) = project.combined(&["stop"]);
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "{text}");

    let a: f64 = project
        .read("drain-a.stamp")
        .trim()
        .parse()
        .expect("service a recorded when it was signalled");
    let b: f64 = project
        .read("drain-b.stamp")
        .trim()
        .parse()
        .expect("service b recorded when it was signalled");
    let apart = (a - b).abs();
    assert!(
        apart < 1.0,
        "services were signalled {apart:.2}s apart, so stop is not parallel (took {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "stop took {elapsed:?}; two 2s drains must not add up"
    );
}

#[test]
fn the_start_console_still_narrates_every_phase() {
    let project = Project::new(
        r#"
[[service]]
name = "quiet"
build-cmd = ["echo", "building-quiet"]
run-cmd = ["sh", "-c", "echo serving-quiet; sleep 120"]

[[service]]
name = "other"
run-cmd = ["sh", "-c", "echo serving-other; sleep 120"]
"#,
    );

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");
    for marker in [
        "===== BUILDING =====",
        "===== BUILD SUCCEED =====",
        "===== RUNNING =====",
        "===== PROBE PASSED (NO PROBE CONFIGURED) =====",
    ] {
        assert!(has(&text, marker), "missing {marker}");
    }
    // Multi-service runs prefix every line, markers included.
    has(&text, "quiet | ===== BUILDING =====");
    has(&text, "quiet | building-quiet");
    has(&text, "other | serving-other");
    let _ = project.agproc(&["stop"]);
}

#[test]
fn logs_shows_only_the_last_run_cmd_output() {
    let project = Project::new(
        r#"
[[service]]
name = "ticker"
run-cmd = ["sh", "-c", "i=0; while true; do echo out-$$-$i; echo err-$$-$i >&2; i=$((i+1)); sleep 0.3; done"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");
    // The console narrates the start…
    has(&text, "===== RUNNING =====");
    has(&text, "===== PROBE PASSED");

    // …while `logs` replays the run-cmd output only.
    let (code, text) = project.combined(&["logs", "ticker"]);
    assert_eq!(code, 0, "{text}");
    let first_run = text
        .lines()
        .find_map(|line| line.strip_prefix("out-"))
        .map(|rest| rest.split('-').next().unwrap_or_default().to_string())
        .expect("run-cmd output present");
    has(&text, "out-");
    has(&text, "err-");
    assert!(
        !text.contains("====="),
        "agproc markers leaked into `logs`:\n{text}"
    );

    let (_, tailed) = project.combined(&["logs", "ticker", "--stream", "stdout", "--tail", "1"]);
    assert_eq!(tailed.lines().filter(|l| !l.is_empty()).count(), 1, "{tailed}");

    // Stream filtering still works, and the streams stay separate.
    let output = project.agproc(&["logs", "ticker", "--stream", "stdout"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    has(&stdout, "out-");
    assert!(!stdout.contains("err-"), "stderr leaked into stdout:\n{stdout}");
    let output = project.agproc(&["logs", "ticker", "--stream", "stderr"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    has(&stderr, "err-");
    assert!(!stderr.contains("out-"), "stdout leaked into stderr:\n{stderr}");

    // A restart starts a new run-cmd: the previous one's output is gone.
    let (code, text) = project.combined(&["restart", "ticker"]);
    assert_eq!(code, 0, "{text}");
    let (_, after) = project.combined(&["logs", "ticker"]);
    has(&after, "out-");
    assert!(
        !after.contains(&format!("out-{first_run}-")),
        "output of the previous run-cmd survived a restart:\n{after}"
    );

    // `-f` comes back by itself once the service is gone, still replaying the
    // run-cmd output only.
    let dir = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        Command::new(bin())
            .current_dir(&dir)
            .args(["logs", "-f"])
            .output()
            .expect("logs -f")
    });
    std::thread::sleep(Duration::from_millis(600));
    let _ = project.agproc(&["stop"]);
    let followed = handle.join().expect("follow thread");
    assert_eq!(followed.status.code(), Some(0));
    let text = String::from_utf8_lossy(&followed.stdout).into_owned();
    has(&text, "out-");
    assert!(
        !text.contains("====="),
        "markers leaked into `logs -f`:\n{text}"
    );
}

#[test]
fn logs_all_is_gone() {
    let project = Project::new(
        r#"
[[service]]
name = "svc"
run-cmd = ["sleep", "120"]
"#,
    );
    let (code, _) = project.combined(&["logs", "--all"]);
    assert_eq!(code, 2, "--all must be rejected as an unknown flag");
}

#[test]
fn build_output_is_shown_live_and_kept_out_of_the_logs() {
    let project = Project::new(
        r#"
[[service]]
name = "badbuild"
build-cmd = ["sh", "-c", "echo compile-error; echo compile-error-on-stderr >&2; exit 101"]
run-cmd = ["sleep", "120"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 4, "{text}");
    has(&text, "===== BUILDING =====");
    has(&text, "compile-error");
    has(&text, "compile-error-on-stderr");
    has(&text, "===== BUILD FAILED (exit code 101) =====");

    // Build output never reaches the service logs…
    let (code, text) = project.combined(&["logs", "badbuild"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "NO LOGS YET");
    assert!(!text.contains("compile-error"), "{text}");

    // …but the transient console stream keeps it (failure paths are not cleaned
    // up), so a failed build stays diagnosable.
    let console = project.read(".agproc/tmp/badbuild.console.stdout");
    has(&console, "compile-error");
    let console_err = project.read(".agproc/tmp/badbuild.console.stderr");
    has(&console_err, "compile-error-on-stderr");
}

#[test]
fn the_console_stream_stops_growing_once_readiness_settled() {
    let project = Project::new(
        r#"
[[service]]
name = "ticker"
run-cmd = ["sh", "-c", "i=0; while true; do echo tick-$i; i=$((i+1)); sleep 0.05; done"]
"#,
    );
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");

    // Let the runner notice that readiness settled.
    std::thread::sleep(Duration::from_millis(400));
    let console = project.file(".agproc/tmp/ticker.console.stdout");
    let log = project.file(".agproc/logs/ticker.stdout.log");
    let size = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let console_before = size(&console);
    let log_before = size(&log);
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        size(&log) > log_before,
        "run logs must keep growing while the service runs"
    );
    assert_eq!(
        size(&console),
        console_before,
        "the console stream must stop duplicating run output after readiness"
    );
    let _ = project.agproc(&["stop"]);
}

#[test]
fn every_probe_attempt_is_reported() {
    let port = free_port();
    let project = Project::new(&format!(
        r#"
[[service]]
name = "hopeless"
run-cmd = ["sh", "-c", "echo waiting; sleep 60"]
probe = {{ tcp-connect = {{ port = {port} }}, initial-delay-seconds = 1, period-seconds = 1, timeout-seconds = 1, failure-threshold = 3 }}
"#
    ));
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 6, "{text}");
    for attempt in 1..=3 {
        has(&text, &format!("===== PROBE ATTEMPT {attempt}/3 FAILED"));
    }
    assert!(!text.contains("PROBE ATTEMPT 4/3"), "{text}");
    has(&text, "===== PROBE FAILED: 3 consecutive failures");
}

#[test]
fn ps_reports_states_and_json_stays_parseable() {
    let port = free_port();
    let project = Project::new(&format!(
        r#"
[[service]]
name = "up"
run-cmd = ["sh", "-c", "echo up; sleep 120"]

[[service]]
name = "down"
run-cmd = ["sleep", "120"]
probe = {{ tcp-connect = {{ port = {port} }}, initial-delay-seconds = 1, period-seconds = 1, timeout-seconds = 1, failure-threshold = 1 }}
"#
    ));

    let (code, text) = project.combined(&["start", "up"]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = project.combined(&["start", "down"]);
    assert_eq!(code, 6, "{text}");

    let (code, text) = project.combined(&["ps"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "running");
    has(&text, "probe failed");

    let ps = project.ps_json();
    let up = ps["services"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["name"] == "up")
        .expect("up listed");
    let down = ps["services"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["name"] == "down")
        .expect("down listed");
    assert_eq!(up["phase"], "running");
    assert_eq!(up["ready"], true);
    assert!(up["uptime_seconds"].as_i64().unwrap_or(-1) >= 0);
    assert_eq!(down["phase"], "probe-failed");
    assert!(down["probe"]["last_error"].as_str().is_some());

    let _ = project.agproc(&["stop"]);
}

#[test]
fn a_listener_owned_by_the_service_counts_as_ready() {
    if !python3_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }
    let port = free_port();
    let project = Project::new(&format!(
        r#"
[[service]]
name = "server"
build-cmd = ["echo", "preparing"]
run-cmd = ["python3", "-m", "http.server", "{port}", "--bind", "127.0.0.1"]
probe = {{ tcp-connect = {{ host = "127.0.0.1", port = {port} }}, initial-delay-seconds = 1, period-seconds = 1, timeout-seconds = 2, failure-threshold = 3 }}
"#
    ));
    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "a service listening on its own port must be ready:\n{text}");
    has(&text, "===== PROBE PASSED");
    assert_eq!(project.ps_json()["services"][0]["phase"], "running");
    let _ = project.agproc(&["stop"]);
}

#[test]
fn skills_describe_the_real_services_and_work_anywhere() {
    let project = Project::new(
        r#"
[[service]]
name = "backend"
build-cmd = ["cargo", "build"]
run-cmd = ["./target/debug/api"]
probe = { http-get = { port = 3000, path = "/healthz" } }
"#,
    );
    let (code, text) = project.combined(&["skills"]);
    assert_eq!(code, 0);
    has(&text, "## This project");
    has(&text, "[\"cargo\", \"build\"]");
    has(&text, "[\"./target/debug/api\"]");
    has(&text, "http://127.0.0.1:3000/healthz");
    has(&text, "agproc restart backend");
    has(&text, "===== ALREADY RUNNING");

    let (code, text) = project.combined(&["skills", "--json"]);
    assert_eq!(code, 0);
    let json: Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(json["services"][0]["name"], "backend");
    assert_eq!(json["services"][0]["probe_kind"], "http-get");

    // Outside a project it still teaches the agent how to set one up.
    let empty = tempfile::tempdir().expect("temp dir");
    let output = Command::new(bin())
        .current_dir(empty.path())
        .args(["skills"])
        .output()
        .expect("skills");
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    has(&text, "generic guide");
    has(&text, "agproc init");
}

#[test]
fn init_writes_a_template_and_gitignore_entry() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(bin())
        .current_dir(dir.path())
        .args(["init"])
        .output()
        .expect("init");
    assert_eq!(output.status.code(), Some(0));

    let config = std::fs::read_to_string(dir.path().join("agproc.toml")).expect("config written");
    assert!(config.contains("[[service]]") || config.contains("# [[service]]"));
    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).expect("gitignore");
    assert!(gitignore.contains(".agproc/"));

    let again = Command::new(bin())
        .current_dir(dir.path())
        .args(["init"])
        .output()
        .expect("init again");
    assert_ne!(again.status.code(), Some(0), "init overwrote without --force");
}

#[test]
fn configuration_errors_exit_with_3() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
run-cmd = ["sleep", "1"]
"#,
    );
    let (code, text) = project.combined(&["start", "nope"]);
    assert_eq!(code, 3, "{text}");
    has(&text, "===== CONFIG ERROR =====");
    has(&text, "unknown service");

    let broken = Project::new(
        r#"
[[service]]
name = "api"
run_cmd = ["sleep", "1"]
"#,
    );
    let (code, text) = broken.combined(&["ps"]);
    assert_eq!(code, 3, "{text}");
    has(&text, "run_cmd");
}

#[test]
fn the_string_form_of_a_command_is_rejected_with_a_fix() {
    // build-cmd / run-cmd are argv arrays executed without a shell; the old
    // string form must fail loudly instead of silently running `<shell> -c`.
    let project = Project::new(
        r#"
[[service]]
name = "api"
run-cmd = "cargo run"
"#,
    );
    let (code, text) = project.combined(&["ps"]);
    assert_eq!(code, 3, "{text}");
    has(&text, "===== CONFIG ERROR =====");
    has(&text, "argv array");
    has(&text, "[\"sh\", \"-c\",");

    // The same service written as an array loads fine.
    let project = Project::new(
        r#"
[[service]]
name = "api"
run-cmd = ["cargo", "run"]
"#,
    );
    let (code, text) = project.combined(&["ps"]);
    assert_eq!(code, 0, "{text}");
}

#[test]
fn unknown_flags_and_missing_config_are_usage_or_config_errors() {
    let project = Project::new("[[service]]\nname = \"a\"\nrun-cmd = [\"true\"]\n");
    let (code, _) = project.combined(&["--definitely-not-a-flag"]);
    assert_eq!(code, 2, "clap usage errors exit 2");

    let empty = tempfile::tempdir().expect("temp dir");
    let output = Command::new(bin())
        .current_dir(empty.path())
        .args(["ps"])
        .output()
        .expect("ps");
    assert_eq!(output.status.code(), Some(3));
    let text = String::from_utf8_lossy(&output.stderr);
    has(&text, "agproc.toml");
}

// ---------------------------------------------------------------------------
// env-file
// ---------------------------------------------------------------------------

#[test]
fn env_file_feeds_build_and_run() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
env-file = ".env"
build-cmd = ["sh", "-c", "echo build-sees-$FROM_FILE"]
run-cmd = ["sh", "-c", "echo run-sees-$FROM_FILE; sleep 120"]
"#,
    );
    project.write(".env", "# loaded by agproc\nFROM_FILE=hello\n");

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "start with an env-file failed:\n{text}");
    // The env-file reaches the build phase and the run phase alike.
    has(&text, "build-sees-hello");
    has(&text, "run-sees-hello");

    let (code, text) = project.combined(&["logs", "api"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "run-sees-hello");

    let _ = project.agproc(&["stop"]);
}

#[test]
fn env_file_is_relative_to_the_project_root_and_env_wins() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
cwd = "sub"
env-file = "sub/.env"
env = { WHO = "inline" }
run-cmd = ["sh", "-c", "echo who-$WHO; echo from-$FROM_FILE; sleep 120"]
"#,
    );
    std::fs::create_dir_all(project.file("sub")).expect("create cwd");
    // Root-relative, not `sub/.env` as seen from `cwd = "sub"`.
    project.write("sub/.env", "WHO=file\nFROM_FILE=sub-env\n");

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");
    // The `env` table overrides the file, which overrides the inherited environment.
    has(&text, "who-inline");
    has(&text, "from-sub-env");

    let _ = project.agproc(&["stop"]);
}

#[test]
fn a_missing_env_file_is_a_config_error() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
env-file = "config/dev.env"
run-cmd = ["sleep", "120"]
"#,
    );

    let (code, text) = project.combined(&["start"]);
    assert_eq!(
        code, 3,
        "a missing env-file must be a configuration error:\n{text}"
    );
    has(&text, "===== CONFIG FAILED");
    has(&text, "cannot read env-file");
    has(&text, "config/dev.env");
    has(&text, "===== START FAILED: api (configuration error");
    // Nothing was started, and the failure is visible in ps.
    assert_eq!(project.state("api")["phase"], "config-failed");
    assert!(project.state("api")["child-pid"].is_null(), "{text}");

    let (code, text) = project.combined(&["ps"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "config failed");
    let ps = project.ps_json();
    assert_eq!(ps["services"][0]["phase"], "config-failed", "{ps}");

    // The console stream keeps the reason, as it does for a failed build.
    let console = project.read(".agproc/tmp/api.console.stdout");
    has(&console, "cannot read env-file");
}

#[test]
fn an_unparsable_env_file_is_a_config_error() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
env-file = ".env"
run-cmd = ["sleep", "120"]
"#,
    );
    project.write(".env", "THIS IS NOT A KEY=VALUE LINE\n");

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 3, "{text}");
    has(&text, "===== CONFIG FAILED");
    has(&text, "cannot parse env-file");
    assert_eq!(project.state("api")["phase"], "config-failed");

    // A duplicate key is just as loud: one of the two lines would be a silent lie.
    project.write(".env", "PORT=1\nPORT=2\n");
    let (code, text) = project.combined(&["restart", "api"]);
    assert_eq!(code, 3, "{text}");
    has(&text, "declared more than once");
    has(&text, "PORT");
}

#[test]
fn editing_an_env_file_marks_the_config_changed() {
    let project = Project::new(
        r#"
[[service]]
name = "api"
env-file = ".env"
run-cmd = ["sh", "-c", "echo value-$FROM_FILE; sleep 120"]
"#,
    );
    project.write(".env", "FROM_FILE=first\n");

    let (code, text) = project.combined(&["start"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "value-first");
    assert_eq!(project.ps_json()["services"][0]["config_changed"], false);

    project.write(".env", "FROM_FILE=second\n");
    let ps = project.ps_json();
    assert_eq!(
        ps["services"][0]["config_changed"], true,
        "editing an env-file must count as a config change: {ps}"
    );

    // `start` stays idempotent, but it now points at the stale environment.
    let (code, text) = project.combined(&["start", "api"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "===== ALREADY RUNNING");
    has(&text, "CONFIG CHANGED SINCE START");

    // Restarting is what applies the new values.
    let (code, text) = project.combined(&["restart", "api"]);
    assert_eq!(code, 0, "{text}");
    has(&text, "value-second");
    assert_eq!(project.ps_json()["services"][0]["config_changed"], false);

    let _ = project.agproc(&["stop"]);
}
