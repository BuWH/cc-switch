#![cfg(unix)]

use super::*;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::OnceLock;
use tempfile::TempDir;

fn control() -> CheckControl {
    CheckControl {
        epoch: Arc::new(AtomicU64::new(0)),
        generation: 0,
        deadline: Instant::now() + Duration::from_secs(10),
    }
}

#[test]
fn unit_tests_never_start_the_real_remote_monitor() {
    let monitor = Arc::new(CodexRemoteMonitor::default());
    let service = crate::services::proxy::ProxyService::new(Arc::new(
        crate::database::Database::memory().unwrap(),
    ));
    monitor.start(service);
    assert!(!monitor.started.load(Ordering::Acquire));
    assert_eq!(monitor.status().state, RemoteState::Inactive);
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    dir: TempDir,
    execution: Execution,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

fn relay_binary() -> &'static Path {
    static RELAY: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &RELAY
        .get_or_init(|| {
            let dir = TempDir::new().unwrap();
            let binary = dir.path().join("relay");
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/services/codex_remote_control/relay_fixture.rs");
            let mut command = Command::new("rustc");
            command.arg(source).arg("-o").arg(&binary);
            let output = process::run(command, Duration::from_secs(10), &control()).unwrap();
            assert!(output.status.success(), "{}", output_text(&output));
            (dir, binary)
        })
        .1
}

fn serve_remote(listener: TcpListener, home: PathBuf, stop: Arc<AtomicBool>) {
    use serde_json::json;
    listener.set_nonblocking(true).unwrap();
    while !stop.load(Ordering::Acquire) {
        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut socket = match tungstenite::accept(stream) {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("fixture handshake: {error}");
                continue;
            }
        };
        while let Ok(message) = socket.read() {
            let tungstenite::Message::Text(text) = message else {
                break;
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            let method = request["method"].as_str().unwrap();
            if method == "initialized" {
                continue;
            }
            let result = if method == "initialize" {
                let reported_home = if home.join("wrong-home").exists() {
                    PathBuf::from("/wrong-home")
                } else {
                    home.clone()
                };
                json!({"codexHome": reported_home, "userAgent": "codex/0.153.4"})
            } else {
                let action = match method {
                    "remoteControl/status/read" => "read",
                    "remoteControl/disable" => "disable",
                    "remoteControl/enable" => "enable",
                    _ => panic!("Unexpected method {method}"),
                };
                use std::io::Write;
                let mut calls = fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(home.join("rpc-calls"))
                    .unwrap();
                writeln!(calls, "{action}").unwrap();
                if action != "read" {
                    assert_eq!(request["params"], json!({"ephemeral": true}));
                    if home.join("reject-ephemeral").exists() {
                        socket.send(tungstenite::Message::Text(json!({
                            "id": request["id"], "error": {"code": -32602, "message": "Invalid params"}
                        }).to_string().into())).unwrap();
                        continue;
                    }
                    let next = if action == "disable" {
                        "disabled".into()
                    } else {
                        fs::read_to_string(home.join("enable-state")).unwrap()
                    };
                    fs::write(home.join("remote"), next).unwrap();
                }
                json!({"status": fs::read_to_string(home.join("remote")).unwrap()})
            };
            // Notifications must not be mistaken for the RPC response.
            socket
                .send(tungstenite::Message::Text(
                    json!({
                        "method": "remoteControl/status/changed", "params": {"status": "connecting"}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            if socket
                .send(tungstenite::Message::Text(
                    json!({
                        "id": request["id"], "result": result
                    })
                    .to_string()
                    .into(),
                ))
                .is_err()
            {
                break;
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_path_buf();
        let candidate = home.join("codex");
        script(
            &candidate,
            &format!(
                "export CODEX_TEST_RELAY='{}'\n{}",
                relay_binary().display(),
                r#"
printf '%s\n' "$*" >> "$CODEX_HOME/calls"
case "$*" in
  "app-server daemon version")
    state=$(cat "$CODEX_HOME/lifecycle")
    case "$state" in
      missing) printf '%s\n' 'Error: failed to connect to app-server-control.sock: No such file or directory (os error 2)' >&2; exit 1 ;;
      stale) printf '%s\n' 'Error: failed to connect to app-server-control.sock: Connection refused (os error 61)' >&2; exit 1 ;;
      invalid) printf '%s\n' 'invalid config.toml' >&2; exit 1 ;;
      external) printf '%s\n' '{"status":"running"}'; exit 0 ;;
    esac
    printf '{"status":"%s","backend":"pid"}\n' "$state"
    ;;
  "app-server daemon start --help") printf '%s\n' 'Start the daemon' ;;
  "app-server daemon start") printf '%s' running > "$CODEX_HOME/lifecycle"; printf '%s\n' '{"status":"started","backend":"pid"}' ;;
  "app-server proxy")
    exec "$CODEX_TEST_RELAY" "$CODEX_HOME"
    ;;
  *) exit 2 ;;
esac
"#
            ),
        );
        fs::write(home.join("lifecycle"), "running").unwrap();
        fs::write(home.join("remote"), "connected").unwrap();
        fs::write(home.join("enable-state"), "connected").unwrap();
        fs::write(home.join("auth.json"), "unchanged login").unwrap();
        fs::write(home.join("config.toml"), "unchanged config").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(
            home.join("test-port"),
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_home = home.clone();
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || serve_remote(listener, server_home, server_stop));
        Self {
            execution: Execution {
                home,
                candidates: vec![candidate],
                control: control(),
            },
            dir,
            stop,
            server: Some(server),
        }
    }

    fn set(&self, key: &str, value: &str) {
        fs::write(self.dir.path().join(key), value).unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }

    fn rpc_calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("rpc-calls")).unwrap_or_default()
    }

    fn mark_socket(&self) {
        fs::create_dir(self.dir.path().join("app-server-control")).unwrap();
        self.set("app-server-control/app-server-control.sock", "");
    }

    fn check(&self, desktop: bool) -> Result<CheckResult, String> {
        reconcile(&self.execution, desktop, false, &mut |_| Ok(desktop))
    }
}

#[test]
fn running_connected_daemon_is_only_probed() {
    let fixture = Fixture::new();
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
    assert_eq!(fixture.rpc_calls(), "read\n");
    assert!(!fixture.calls().contains("daemon start"));
}

#[test]
fn missing_control_socket_recovers_the_daemon() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "missing");
    fixture.set("remote", "disabled");
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
    assert!(fixture.calls().contains("app-server daemon start\n"));
    assert!(!fixture.calls().contains("bootstrap"));
    assert_eq!(fixture.rpc_calls(), "read\nenable\n");
}

#[test]
fn stale_control_socket_recovers_the_daemon() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "stale");
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
    assert!(fixture.calls().contains("app-server daemon start\n"));
}

#[test]
fn stopped_json_also_recovers() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "stopped");
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
}

#[test]
fn unsupported_cli_falls_back_without_starting_it() {
    let mut fixture = Fixture::new();
    let unsupported = fixture.dir.path().join("old-codex");
    script(&unsupported, "exit 2");
    fixture.execution.candidates.insert(0, unsupported);
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
}

#[test]
fn invalid_config_is_not_mistaken_for_a_stopped_daemon() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "invalid");
    assert!(fixture.check(false).is_err());
    assert!(!fixture.calls().contains("daemon start"));
}

#[test]
fn desktop_without_a_managed_socket_does_not_start_a_second_server() {
    let fixture = Fixture::new();
    assert_eq!(fixture.check(true).unwrap().state, RemoteState::Desktop);
    assert!(fixture.calls().is_empty());
}

#[test]
fn desktop_pauses_only_the_managed_remote_worker() {
    let fixture = Fixture::new();
    fixture.mark_socket();
    assert_eq!(fixture.check(true).unwrap().state, RemoteState::Desktop);
    assert_eq!(fixture.rpc_calls(), "read\ndisable\n");
    assert!(!fixture.calls().contains("stop"));
    assert!(!fixture.calls().contains("restart"));
    assert!(!fixture.calls().contains("bootstrap"));
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("auth.json")).unwrap(),
        "unchanged login"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("config.toml")).unwrap(),
        "unchanged config"
    );
}

#[test]
fn desktop_exit_resumes_the_daemon_without_restarting_it() {
    let fixture = Fixture::new();
    fixture.mark_socket();
    fixture.check(true).unwrap();
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
    assert_eq!(fixture.rpc_calls(), "read\ndisable\nread\nenable\n");
    assert!(!fixture.calls().contains("daemon start"));
}

#[test]
fn unmanaged_control_socket_is_not_mutated() {
    let fixture = Fixture::new();
    fixture.mark_socket();
    fixture.set("lifecycle", "external");
    assert_eq!(fixture.check(true).unwrap().state, RemoteState::External);
    assert!(fixture.rpc_calls().is_empty());
}

#[test]
fn desktop_appearing_during_recovery_prevents_start() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "missing");
    let mut probes = [false, true].into_iter();
    let result = reconcile(&fixture.execution, false, false, &mut |_| {
        Ok(probes.next().unwrap())
    });
    assert_eq!(result.unwrap().state, RemoteState::Desktop);
    assert!(!fixture.calls().contains("app-server daemon start\n"));
}

#[test]
fn desktop_appearing_during_enable_immediately_receives_ownership() {
    let fixture = Fixture::new();
    fixture.set("remote", "disabled");
    let mut probes = [false, true].into_iter();
    let result = reconcile(&fixture.execution, false, false, &mut |_| {
        Ok(probes.next().unwrap())
    });
    assert_eq!(result.unwrap().state, RemoteState::Desktop);
    assert_eq!(fixture.rpc_calls(), "read\nenable\ndisable\n");
}

#[test]
fn unsupported_ephemeral_disable_never_falls_back_to_a_persistent_write() {
    let fixture = Fixture::new();
    fixture.mark_socket();
    fixture.set("reject-ephemeral", "");
    assert!(fixture.check(true).unwrap_err().contains("Invalid params"));
    assert_eq!(fixture.rpc_calls(), "read\ndisable\n");
}

#[test]
fn a_different_codex_home_is_never_mutated() {
    let fixture = Fixture::new();
    fixture.set("remote", "disabled");
    fixture.set("wrong-home", "");
    assert!(fixture
        .check(false)
        .unwrap_err()
        .contains("another CODEX_HOME"));
    assert!(fixture.rpc_calls().is_empty());
}

#[test]
fn connecting_is_not_reported_as_connected_or_restarted_immediately() {
    let fixture = Fixture::new();
    fixture.set("remote", "connecting");
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connecting);
    assert_eq!(fixture.rpc_calls(), "read\n");
}

#[test]
fn errored_or_stalled_workers_are_reconnected_without_restarting_tasks() {
    for state in ["errored", "connecting"] {
        let fixture = Fixture::new();
        fixture.set("remote", state);
        let result = reconcile(&fixture.execution, false, true, &mut |_| Ok(false)).unwrap();
        assert_eq!(result.state, RemoteState::Connected);
        assert_eq!(fixture.rpc_calls(), "read\ndisable\nenable\n");
        assert!(!fixture.calls().contains("restart"));
    }
}

#[test]
fn timeout_kills_the_command_and_reaps_it() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]);
    let start = Instant::now();
    let error = process::run(command, Duration::from_millis(80), &control()).unwrap_err();
    assert!(error.contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn config_change_cancels_in_flight_commands() {
    let control = control();
    let epoch = control.epoch.clone();
    let changer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        epoch.fetch_add(1, Ordering::AcqRel);
    });
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]);
    let error = process::run(command, Duration::from_secs(5), &control).unwrap_err();
    changer.join().unwrap();
    assert!(error.contains("superseded"));
}

#[test]
fn child_inheriting_stdout_does_not_hold_the_result_open() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 0.3 & printf 'ok'; exit 0"]);
    let start = Instant::now();
    let output = process::run(command, Duration::from_millis(200), &control()).unwrap();
    assert_eq!(output.stdout, b"ok");
    assert!(start.elapsed() < Duration::from_millis(250));
    std::thread::sleep(Duration::from_millis(310));
}

#[test]
fn discovered_node_directory_is_available_without_a_login_shell() {
    let fixture = Fixture::new();
    let node_dir = fixture.dir.path().join(".nvm/versions/node/v22/bin");
    fs::create_dir_all(&node_dir).unwrap();
    script(&node_dir.join("node"), "printf '%s' node-found");
    let candidate = fixture.dir.path().join("npm-codex");
    fs::write(&candidate, "#!/usr/bin/env node\n").unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let paths = node_candidates(fixture.dir.path());
    let mut command = Command::new(&candidate);
    let path = cli_path_with_inherited(&candidate, &paths, Some("/usr/bin:/bin".as_ref()));
    command.env("PATH", path);
    let output = process::run(command, Duration::from_secs(3), &control()).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"node-found");
}

#[test]
fn backoff_is_bounded_and_ownership_changes_bypass_it() {
    assert_eq!(
        [1, 2, 3, 4, 5, 6, 99].map(|n| retry_delay(n).as_secs()),
        [5, 10, 20, 40, 80, 120, 120]
    );
    let fixture = Fixture::new();
    let mut worker = Worker {
        desktop: Some(false),
        next_check: Instant::now() + Duration::from_secs(120),
        ..Worker::default()
    };
    assert!(worker
        .tick(&fixture.execution, false, |_| Ok(false))
        .unwrap()
        .is_none());
    assert_eq!(
        worker
            .tick(&fixture.execution, false, |_| Ok(true))
            .unwrap()
            .unwrap()
            .state,
        RemoteState::Desktop
    );
}

#[test]
fn failure_is_retried_and_cleared_after_recovery() {
    let fixture = Fixture::new();
    fixture.set("lifecycle", "invalid");
    let mut worker = Worker::default();
    assert!(worker
        .tick(&fixture.execution, false, |_| Ok(false))
        .is_err());
    assert_eq!(worker.failures, 1);
    fixture.set("lifecycle", "running");
    assert!(worker
        .tick(&fixture.execution, false, |_| Ok(false))
        .unwrap()
        .is_none());
    assert_eq!(
        worker
            .tick(&fixture.execution, true, |_| Ok(false))
            .unwrap()
            .unwrap()
            .state,
        RemoteState::Connected
    );
    assert_eq!(worker.failures, 0);
    let monitor = CodexRemoteMonitor::default();
    monitor.publish(Err("offline".into()), OWNER_POLL, None);
    assert_eq!(monitor.status().state, RemoteState::Degraded);
    monitor.publish(
        Ok(CheckResult::new(RemoteState::Connected, None)),
        HEALTH_INTERVAL,
        None,
    );
    assert!(monitor.status().last_error.is_none());
    assert!(monitor.status().last_connected_at.is_some());
}

#[test]
fn wake_gap_bypasses_an_old_retry_deadline() {
    let fixture = Fixture::new();
    let mut worker = Worker {
        desktop: Some(false),
        next_check: Instant::now() + Duration::from_secs(120),
        last_tick: Some(SystemTime::now() - Duration::from_secs(60)),
        ..Worker::default()
    };
    assert_eq!(
        worker
            .tick(&fixture.execution, false, |_| Ok(false))
            .unwrap()
            .unwrap()
            .state,
        RemoteState::Connected
    );
}

#[test]
fn daemon_socket_appearing_while_desktop_is_open_is_checked_immediately() {
    let fixture = Fixture::new();
    let mut worker = Worker::default();
    worker
        .tick(&fixture.execution, false, |_| Ok(true))
        .unwrap();
    fixture.mark_socket();
    assert_eq!(
        worker
            .tick(&fixture.execution, false, |_| Ok(true))
            .unwrap()
            .unwrap()
            .state,
        RemoteState::Desktop
    );
    assert_eq!(fixture.rpc_calls(), "read\ndisable\n");
}

#[test]
fn an_unrecoverable_remote_error_is_not_success() {
    let fixture = Fixture::new();
    fixture.set("remote", "errored");
    fixture.set("enable-state", "errored");
    assert!(fixture.check(false).unwrap_err().contains("errored"));
}

#[test]
fn executable_disappearing_before_rpc_falls_back_to_another_client() {
    let mut fixture = Fixture::new();
    let first = fixture.dir.path().join("vanishing-codex");
    script(
        &first,
        r#"chmod 000 "$0"; printf '%s\n' '{"status":"running","backend":"pid"}'"#,
    );
    fixture.execution.candidates.insert(0, first);
    assert_eq!(fixture.check(false).unwrap().state, RemoteState::Connected);
}

#[test]
fn excessive_command_output_is_bounded() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "dd if=/dev/zero bs=65536 count=32 2>/dev/null"]);
    assert!(process::run(command, Duration::from_secs(2), &control())
        .unwrap_err()
        .contains("output limit"));
}

#[test]
#[ignore = "optional read-only wire test against a locally installed Codex CLI"]
fn official_cli_proxy_speaks_the_expected_websocket_protocol() {
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    let cli =
        PathBuf::from(std::env::var("CODEX_REMOTE_TEST_CLI").expect("set CODEX_REMOTE_TEST_CLI"));
    let dir = tempfile::Builder::new()
        .prefix("cc-remote-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = dir.path().canonicalize().unwrap();
    let socket_path = home.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server_home = home.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("CLI did not connect to the test socket: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        loop {
            let tungstenite::Message::Text(message) = socket.read().unwrap() else {
                continue;
            };
            let message: Value = serde_json::from_str(&message).unwrap();
            let method = message["method"].as_str().unwrap();
            if method == "initialized" {
                continue;
            }
            let result = if method == "initialize" {
                json!({"codexHome": server_home, "userAgent": "fixture/1"})
            } else {
                assert_eq!(method, "remoteControl/status/read");
                json!({"status": "connected"})
            };
            socket
                .send(tungstenite::Message::Text(
                    json!({
                        "id": message["id"], "result": result
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            if method == "remoteControl/status/read" {
                break;
            }
        }
    });
    let execution = Execution {
        home,
        candidates: vec![cli.clone()],
        control: control(),
    };
    let result = process::rpc(
        execution.command(
            &cli,
            &[
                "app-server",
                "proxy",
                "--sock",
                socket_path.to_str().unwrap(),
            ],
        ),
        &execution.home,
        "remoteControl/status/read",
        &execution.control,
    );
    server.join().unwrap();
    assert_eq!(result.unwrap()["status"], "connected");
}
