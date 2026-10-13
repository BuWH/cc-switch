//! Non-disruptive Remote Control recovery. Desktop owns the connection while
//! present; an existing managed daemon yields only its ephemeral remote worker.

mod desktop;
mod process;
#[cfg(test)]
mod tests;

use process::{output_text, CheckControl};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Notify;

const OWNER_POLL: Duration = Duration::from_secs(5);
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);
const CHECK_TIMEOUT: Duration = Duration::from_secs(45);
const STALLED_CONNECTION: Duration = Duration::from_secs(120);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteState {
    #[default]
    Inactive,
    Checking,
    Desktop,
    External,
    Connected,
    Connecting,
    Degraded,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRemoteStatus {
    pub state: RemoteState,
    pub last_checked_at: Option<String>,
    pub last_connected_at: Option<String>,
    pub last_error: Option<String>,
    pub next_check_at: Option<String>,
    pub cli_path: Option<String>,
}

#[derive(Default)]
pub struct CodexRemoteMonitor {
    started: AtomicBool,
    epoch: Arc<AtomicU64>,
    notify: Notify,
    status: Mutex<CodexRemoteStatus>,
}

impl CodexRemoteMonitor {
    pub fn status(&self) -> CodexRemoteStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn request_check(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        if self.started.load(Ordering::Acquire) {
            self.status.lock().unwrap_or_else(|e| e.into_inner()).state = RemoteState::Checking;
        }
        self.notify.notify_one();
    }

    pub fn start(self: &Arc<Self>, service: super::proxy::ProxyService) {
        // Integration tests compile the library without cfg(test). Never
        // inspect or mutate the real user's daemon from a test HOME.
        if cfg!(test) || std::env::var_os("CC_SWITCH_TEST_HOME").is_some() {
            return;
        }
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let monitor = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut worker = Worker::default();
            let mut forced = false;
            loop {
                forced |= tokio::select! {
                    _ = monitor.notify.notified() => true,
                    _ = tokio::time::sleep(OWNER_POLL) => false,
                };
                let generation = monitor.epoch.load(Ordering::Acquire);
                // Config writers invalidate the epoch; no provider switch lock
                // is held across a process or network operation.
                if service.is_switch_in_progress_for_app("codex").await {
                    continue;
                }
                match service.codex_remote_takeover_enabled().await {
                    Ok(false) => {
                        worker = Worker::default();
                        *monitor.status.lock().unwrap_or_else(|e| e.into_inner()) =
                            CodexRemoteStatus::default();
                        continue;
                    }
                    Err(error) => {
                        monitor.publish(Err(error), OWNER_POLL, None);
                        continue;
                    }
                    Ok(true) => {}
                }
                let control = CheckControl {
                    epoch: monitor.epoch.clone(),
                    generation,
                    deadline: Instant::now() + CHECK_TIMEOUT,
                };
                let mut next_worker = worker.clone();
                let force_check = forced;
                forced = false;
                let result = tauri::async_runtime::spawn_blocking(move || {
                    let outcome =
                        Execution::for_active_proxy(control).and_then(
                            |execution| match execution {
                                Some(execution) => {
                                    next_worker.tick(&execution, force_check, desktop::running)
                                }
                                None => Ok(Some(CheckResult::new(RemoteState::Inactive, None))),
                            },
                        );
                    (next_worker, outcome)
                })
                .await;
                if monitor.epoch.load(Ordering::Acquire) != generation {
                    continue;
                }
                match result {
                    Ok((next_worker, outcome)) => {
                        worker = next_worker;
                        match outcome {
                            Ok(None) => {}
                            result => monitor.publish(
                                result.and_then(|value| {
                                    value.ok_or("Missing remote check result".into())
                                }),
                                worker.next_check.saturating_duration_since(Instant::now()),
                                worker.selected.as_deref(),
                            ),
                        }
                    }
                    Err(error) => monitor.publish(Err(error.to_string()), OWNER_POLL, None),
                }
            }
        });
        self.notify.notify_one();
    }

    fn publish(&self, result: Result<CheckResult, String>, delay: Duration, cli: Option<&Path>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = chrono::Utc::now();
        status.last_checked_at = Some(now.to_rfc3339());
        status.next_check_at =
            Some((now + chrono::Duration::seconds(delay.as_secs().max(1) as i64)).to_rfc3339());
        status.cli_path = cli.map(|path| path.display().to_string());
        match result {
            Ok(result) => {
                if status.state != result.state || status.last_error.is_some() {
                    log::info!("Codex Remote health: {:?}", result.state);
                }
                status.state = result.state;
                status.last_error = None;
                if result.state == RemoteState::Connected {
                    status.last_connected_at = status.last_checked_at.clone();
                }
                if result.state == RemoteState::Inactive {
                    status.next_check_at = None;
                }
            }
            Err(error) => {
                if status.last_error.as_ref() != Some(&error) {
                    log::warn!("Codex Remote recovery deferred: {error}");
                }
                status.state = RemoteState::Degraded;
                status.last_error = Some(error);
            }
        }
    }
}

#[derive(Clone)]
struct Worker {
    selected: Option<PathBuf>,
    desktop: Option<bool>,
    next_check: Instant,
    failures: u32,
    connecting_since: Option<Instant>,
    last_tick: Option<SystemTime>,
    socket_modified: Option<SystemTime>,
}

impl Default for Worker {
    fn default() -> Self {
        Self {
            selected: None,
            desktop: None,
            next_check: Instant::now(),
            failures: 0,
            connecting_since: None,
            last_tick: None,
            socket_modified: None,
        }
    }
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(5 * (1u64 << failures.saturating_sub(1).min(5)))
        .min(Duration::from_secs(120))
}

impl Worker {
    fn tick(
        &mut self,
        execution: &Execution,
        forced: bool,
        mut desktop_probe: impl FnMut(&CheckControl) -> Result<bool, String>,
    ) -> Result<Option<CheckResult>, String> {
        let resumed = self
            .last_tick
            .is_some_and(|last| last.elapsed().unwrap_or(HEALTH_INTERVAL) > OWNER_POLL * 3);
        self.last_tick = Some(SystemTime::now());
        let desktop = desktop_probe(&execution.control)?;
        let owner_changed = self.desktop != Some(desktop);
        let socket_modified = std::fs::metadata(
            execution
                .home
                .join("app-server-control/app-server-control.sock"),
        )
        .ok()
        .and_then(|metadata| metadata.modified().ok());
        let socket_changed = self.socket_modified != socket_modified;
        self.socket_modified = socket_modified;
        if !forced
            && !resumed
            && !owner_changed
            && !socket_changed
            && Instant::now() < self.next_check
        {
            return Ok(None);
        }
        self.desktop = Some(desktop);
        let stalled = self
            .connecting_since
            .is_some_and(|since| since.elapsed() >= STALLED_CONNECTION);
        let result = reconcile(execution, desktop, stalled, &mut desktop_probe);
        let delay = match &result {
            Ok(result) => {
                self.selected = result.cli.clone();
                if result.state == RemoteState::Connecting {
                    if stalled || self.connecting_since.is_none() {
                        self.connecting_since = Some(Instant::now());
                    }
                    OWNER_POLL
                } else {
                    self.connecting_since = None;
                    HEALTH_INTERVAL
                }
            }
            Err(_) => {
                self.failures = self.failures.saturating_add(1);
                retry_delay(self.failures)
            }
        };
        if result.is_ok() {
            self.failures = 0;
        }
        self.next_check = Instant::now() + delay;
        result.map(Some)
    }
}

#[derive(Debug)]
struct CheckResult {
    state: RemoteState,
    cli: Option<PathBuf>,
}

impl CheckResult {
    fn new(state: RemoteState, cli: Option<&Path>) -> Self {
        Self {
            state,
            cli: cli.map(Path::to_path_buf),
        }
    }
}

struct Execution {
    home: PathBuf,
    candidates: Vec<PathBuf>,
    control: CheckControl,
}

impl Execution {
    fn for_active_proxy(control: CheckControl) -> Result<Option<Self>, String> {
        let home = crate::codex_config::get_codex_config_dir();
        let auth: Value = crate::config::read_json_file(&home.join("auth.json"))
            .map_err(|error| format!("Cannot read Codex login: {error}"))?;
        if !crate::codex_config::codex_auth_has_chatgpt_login_material(&auth) {
            return Ok(None);
        }
        let config =
            std::fs::read_to_string(home.join("config.toml")).map_err(|error| error.to_string())?;
        if !crate::codex_config::codex_config_active_provider_requires_openai_auth(&config) {
            return Ok(None);
        }
        let config: toml::Value = toml::from_str(&config).map_err(|error| error.to_string())?;
        if config
            .get("features")
            .and_then(|f| f.get("remote_control"))
            .and_then(toml::Value::as_bool)
            != Some(true)
        {
            return Ok(None);
        }
        Ok(Some(Self {
            candidates: candidates(&home),
            home,
            control,
        }))
    }

    fn command(&self, candidate: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(candidate);
        command.args(args).env("CODEX_HOME", &self.home);
        let mut runtimes = crate::codex_config::codex_cli_candidates();
        runtimes.extend(node_candidates(&crate::config::get_home_dir()));
        runtimes.extend(self.candidates.iter().cloned());
        command.env("PATH", cli_path(candidate, &runtimes));
        command
    }

    fn run(&self, candidate: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
        process::run(self.command(candidate, args), timeout, &self.control)
            .map_err(|error| format!("{} {}: {error}", candidate.display(), args.join(" ")))
    }

    fn rpc(&self, candidate: &Path, method: &str) -> Result<Value, String> {
        process::rpc(
            self.command(candidate, &["app-server", "proxy"]),
            &self.home,
            method,
            &self.control,
        )
    }
}

fn candidates(home: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: PathBuf| {
        if path.components().count() > 1 && !path.is_file() {
            return;
        }
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            result.push(path);
        }
    };
    // Prefer Desktop's current protocol client. The daemon command itself
    // still selects Codex's managed standalone binary.
    #[cfg(target_os = "macos")]
    for path in [
        "/Applications/ChatGPT.app/Contents/Resources/codex",
        "/Applications/Codex.app/Contents/Resources/codex",
    ] {
        add(PathBuf::from(path));
    }
    let executable = if cfg!(windows) { "codex.exe" } else { "codex" };
    add(home
        .join("packages/standalone/current/bin")
        .join(executable));
    add(home.join("packages/standalone/current").join(executable));
    for path in crate::codex_config::codex_cli_candidates() {
        add(path);
    }
    result
}

fn cli_path(candidate: &Path, candidates: &[PathBuf]) -> std::ffi::OsString {
    cli_path_with_inherited(candidate, candidates, std::env::var_os("PATH").as_deref())
}

fn node_candidates(home: &Path) -> Vec<PathBuf> {
    let node = if cfg!(windows) { "node.exe" } else { "node" };
    let mut result = Vec::new();
    for relative in [
        ".local/bin",
        ".volta/bin",
        ".asdf/shims",
        ".local/share/mise/shims",
        ".config/mise/shims",
    ] {
        result.push(home.join(relative).join(node));
    }
    let mut roots = vec![
        (home.join(".nvm/versions/node"), "bin"),
        (
            home.join(".local/share/fnm/node-versions"),
            "installation/bin",
        ),
        (
            home.join("Library/Application Support/fnm/node-versions"),
            "installation/bin",
        ),
    ];
    if let Some(nvm) = std::env::var_os("NVM_DIR") {
        roots.push((PathBuf::from(nvm).join("versions/node"), "bin"));
    }
    if let Some(fnm) = std::env::var_os("FNM_DIR") {
        roots.push((PathBuf::from(fnm).join("node-versions"), "installation/bin"));
    }
    for (root, suffix) in roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut nodes = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join(suffix).join(node))
                .filter(|path| path.is_file())
                .collect::<Vec<_>>();
            nodes.sort_by(|a, b| b.cmp(a));
            result.extend(nodes);
        }
    }
    #[cfg(windows)]
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        result.push(PathBuf::from(program_files).join("nodejs").join(node));
    }
    result
}

fn cli_path_with_inherited(
    candidate: &Path,
    candidates: &[PathBuf],
    inherited: Option<&std::ffi::OsStr>,
) -> std::ffi::OsString {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: PathBuf| {
        if path.is_absolute() && seen.insert(path.clone()) {
            paths.push(path);
        }
    };
    if let Some(parent) = candidate.parent() {
        add(parent.to_path_buf());
    }
    let node = if cfg!(windows) { "node.exe" } else { "node" };
    for path in candidates {
        if let Some(parent) = path.parent().filter(|parent| parent.join(node).is_file()) {
            add(parent.to_path_buf());
        }
    }
    if let Some(inherited) = inherited {
        for path in std::env::split_paths(inherited) {
            add(path);
        }
    }
    // GUI PATH may omit Node. Reuse nvm/fnm/mise/npm discovery without
    // executing the user's login shell.
    for path in candidates {
        if let Some(parent) = path.parent() {
            add(parent.to_path_buf());
        }
    }
    #[cfg(unix)]
    for path in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        add(path.into());
    }
    std::env::join_paths(paths).unwrap_or_default()
}

fn status(value: &Value) -> Result<&str, String> {
    value
        .get("status")
        .and_then(Value::as_str)
        .ok_or("Codex returned no status".into())
}

fn unavailable_socket(output: &Output) -> bool {
    let text = output_text(output).to_ascii_lowercase();
    !output.status.success()
        && text.contains("failed to connect")
        && text.contains("app-server-control")
        && [
            "no such file",
            "connection refused",
            "os error 2",
            "os error 61",
            "os error 111",
            "os error 10061",
        ]
        .iter()
        .any(|needle| text.contains(needle))
}

fn remote_state(value: &Value) -> Result<RemoteState, String> {
    match status(value)? {
        "connected" => Ok(RemoteState::Connected),
        "connecting" => Ok(RemoteState::Connecting),
        other => Err(format!("Codex Remote is {other}")),
    }
}

fn reconcile(
    execution: &Execution,
    desktop: bool,
    stalled: bool,
    desktop_probe: &mut impl FnMut(&CheckControl) -> Result<bool, String>,
) -> Result<CheckResult, String> {
    let socket = execution
        .home
        .join("app-server-control/app-server-control.sock");
    if desktop && !socket.exists() {
        return Ok(CheckResult::new(RemoteState::Desktop, None));
    }
    let mut diagnostics = Vec::new();
    for candidate in &execution.candidates {
        execution.control.check()?;
        let version = match execution.run(
            candidate,
            &["app-server", "daemon", "version"],
            Duration::from_secs(4),
        ) {
            Ok(output) => output,
            Err(error) => {
                diagnostics.push(error);
                continue;
            }
        };
        let version_json = serde_json::from_slice::<Value>(&version.stdout).ok();
        let lifecycle = version_json.as_ref().and_then(|value| status(value).ok());
        let stopped = unavailable_socket(&version)
            || (version.status.success() && matches!(lifecycle, Some("stopped" | "notRunning")));
        let running = version.status.success() && lifecycle == Some("running");
        if !stopped && !running {
            diagnostics.push(format!(
                "{}: {}",
                candidate.display(),
                output_text(&version)
            ));
            continue;
        }
        if running
            && version_json
                .as_ref()
                .and_then(|v| v.get("backend"))
                .and_then(Value::as_str)
                != Some("pid")
        {
            return Ok(CheckResult::new(RemoteState::External, Some(candidate)));
        }
        if stopped {
            if desktop || desktop_probe(&execution.control)? {
                return Ok(CheckResult::new(RemoteState::Desktop, Some(candidate)));
            }
            // Capability and liveness are separate. Bootstrap is deliberately
            // avoided: it can stop tasks and restart an existing updater.
            let help = match execution.run(
                candidate,
                &["app-server", "daemon", "start", "--help"],
                Duration::from_secs(4),
            ) {
                Ok(help) => help,
                Err(error) => {
                    diagnostics.push(error);
                    continue;
                }
            };
            if !help.status.success() {
                diagnostics.push(format!(
                    "{}: daemon start is unsupported",
                    candidate.display()
                ));
                continue;
            }
            if desktop_probe(&execution.control)? {
                return Ok(CheckResult::new(RemoteState::Desktop, Some(candidate)));
            }
            let started = execution.run(
                candidate,
                &["app-server", "daemon", "start"],
                Duration::from_secs(20),
            )?;
            if !started.status.success() {
                return Err(format!(
                    "Cannot start the managed Codex daemon: {}",
                    output_text(&started)
                ));
            }
            let verified = execution.run(
                candidate,
                &["app-server", "daemon", "version"],
                Duration::from_secs(4),
            )?;
            let verified: Value = serde_json::from_slice(&verified.stdout)
                .ok()
                .filter(|_| verified.status.success())
                .ok_or("Could not verify the started Codex daemon")?;
            if verified.get("backend").and_then(Value::as_str) != Some("pid") {
                return Ok(CheckResult::new(RemoteState::External, Some(candidate)));
            }
        }
        let mut remote = match execution.rpc(candidate, "remoteControl/status/read") {
            Ok(remote) => remote,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", candidate.display()));
                continue;
            }
        };
        if desktop || desktop_probe(&execution.control)? {
            if status(&remote)? != "disabled" {
                remote = execution.rpc(candidate, "remoteControl/disable")?;
                if status(&remote)? != "disabled" {
                    return Err(
                        "Managed daemon did not yield its remote connection to Desktop".into(),
                    );
                }
            }
            return Ok(CheckResult::new(RemoteState::Desktop, Some(candidate)));
        }
        if status(&remote)? == "errored" || (stalled && status(&remote)? == "connecting") {
            let disabled = execution.rpc(candidate, "remoteControl/disable")?;
            if status(&disabled)? != "disabled" {
                return Err("Codex Remote worker did not stop; reconnect deferred".into());
            }
            if desktop_probe(&execution.control)? {
                return Ok(CheckResult::new(RemoteState::Desktop, Some(candidate)));
            }
            remote = execution.rpc(candidate, "remoteControl/enable")?;
        } else if status(&remote)? == "disabled" {
            remote = execution.rpc(candidate, "remoteControl/enable")?;
        }
        // Desktop can appear during a command. Yield without killing either
        // app-server or changing persisted login/enrollment preferences.
        if desktop_probe(&execution.control)? {
            let yielded = execution.rpc(candidate, "remoteControl/disable")?;
            if status(&yielded)? != "disabled" {
                return Err("Managed daemon did not yield its remote connection to Desktop".into());
            }
            return Ok(CheckResult::new(RemoteState::Desktop, Some(candidate)));
        }
        return Ok(CheckResult::new(remote_state(&remote)?, Some(candidate)));
    }
    Err(format!(
        "Codex Remote check failed: {}",
        diagnostics
            .join(" | ")
            .chars()
            .take(4000)
            .collect::<String>()
    ))
}
