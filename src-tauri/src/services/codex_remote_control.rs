use serde_json::Value;
#[cfg(not(test))]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRemoteDaemonAction {
    AlreadyRunning,
    Bootstrapped,
}

#[cfg(not(test))]
fn push_candidate(candidates: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    // A single-component path is a PATH lookup such as `codex`; absolute and
    // multi-component candidates must exist before we try to execute them.
    if path.components().count() > 1 && !path.exists() {
        return;
    }
    let key = path.to_string_lossy().into_owned();
    if seen.insert(key) {
        candidates.push(path);
    }
}

#[cfg(not(test))]
fn codex_remote_daemon_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let codex_home = crate::codex_config::get_codex_config_dir();
    let home = crate::config::get_home_dir();

    #[cfg(target_os = "windows")]
    {
        push_candidate(
            &mut candidates,
            &mut seen,
            codex_home.join("packages/standalone/current/bin/codex.exe"),
        );
        push_candidate(
            &mut candidates,
            &mut seen,
            codex_home.join("packages/standalone/current/codex.exe"),
        );
        push_candidate(
            &mut candidates,
            &mut seen,
            home.join(".local/bin/codex.exe"),
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        push_candidate(
            &mut candidates,
            &mut seen,
            codex_home.join("packages/standalone/current/bin/codex"),
        );
        push_candidate(
            &mut candidates,
            &mut seen,
            codex_home.join("packages/standalone/current/codex"),
        );
        push_candidate(&mut candidates, &mut seen, home.join(".local/bin/codex"));
    }

    #[cfg(target_os = "macos")]
    push_candidate(
        &mut candidates,
        &mut seen,
        PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
    );

    // Keep the established CLI discovery list as a fallback for standalone
    // installs managed by Homebrew, npm, nvm, fnm, mise, or pnpm.
    for candidate in crate::codex_config::codex_cli_candidates() {
        push_candidate(&mut candidates, &mut seen, candidate);
    }

    candidates
}

fn daemon_command(candidate: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(candidate);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

fn output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}; {stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => "no output".to_string(),
    }
}

fn daemon_status(output: &Output) -> Option<String> {
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn remote_control_is_connected(output: &Output) -> bool {
    if !output.status.success() {
        return false;
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|status| status.eq_ignore_ascii_case("connected"))
}

fn start_remote_control(candidate: &Path) -> Result<Output, String> {
    daemon_command(candidate, &["remote-control", "start", "--json"])
        .output()
        .map_err(|error| format!("{}: {error}", candidate.display()))
}

fn ensure_with_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<CodexRemoteDaemonAction, String> {
    let mut diagnostics = Vec::new();

    for candidate in candidates {
        let version =
            match daemon_command(&candidate, &["app-server", "daemon", "version"]).output() {
                Ok(output) => output,
                Err(error) => {
                    diagnostics.push(format!("{}: {error}", candidate.display()));
                    continue;
                }
            };

        if let Some(status) = daemon_status(&version) {
            if status.eq_ignore_ascii_case("running") {
                let started = start_remote_control(&candidate)?;
                if remote_control_is_connected(&started) {
                    return Ok(CodexRemoteDaemonAction::AlreadyRunning);
                }
                diagnostics.push(format!(
                    "{} remote-control start: {}",
                    candidate.display(),
                    output_text(&started)
                ));
                continue;
            }

            let bootstrapped = daemon_command(
                &candidate,
                &["app-server", "daemon", "bootstrap", "--remote-control"],
            )
            .output()
            .map_err(|error| format!("{}: {error}", candidate.display()))?;
            if bootstrapped.status.success() {
                let started = start_remote_control(&candidate)?;
                if remote_control_is_connected(&started) {
                    return Ok(CodexRemoteDaemonAction::Bootstrapped);
                }
                diagnostics.push(format!(
                    "{} remote-control start after bootstrap: {}",
                    candidate.display(),
                    output_text(&started)
                ));
                continue;
            }
            diagnostics.push(format!(
                "{} bootstrap --remote-control: {}",
                candidate.display(),
                output_text(&bootstrapped)
            ));
            continue;
        }

        diagnostics.push(format!(
            "{}: {}",
            candidate.display(),
            output_text(&version)
        ));
    }

    Err(format!(
        "未找到支持 Remote daemon 的 Codex standalone CLI。请使用 Codex 官方安装器安装最新版 CLI。探测结果: {}",
        diagnostics.join(" | ")
    ))
}

#[cfg(not(test))]
fn active_proxy_uses_chatgpt_auth() -> bool {
    let auth_path = crate::codex_config::get_codex_auth_path();
    let config_path = crate::codex_config::get_codex_config_path();
    let Ok(auth) = crate::config::read_json_file::<Value>(&auth_path) else {
        return false;
    };
    if !crate::codex_config::codex_auth_has_chatgpt_login_material(&auth) {
        return false;
    }
    let Ok(config_text) = std::fs::read_to_string(config_path) else {
        return false;
    };
    crate::codex_config::codex_config_active_provider_requires_openai_auth(&config_text)
}

#[cfg(not(test))]
pub async fn ensure_for_active_proxy() -> Result<Option<CodexRemoteDaemonAction>, String> {
    if !active_proxy_uses_chatgpt_auth() {
        return Ok(None);
    }

    tauri::async_runtime::spawn_blocking(
        || ensure_with_candidates(codex_remote_daemon_candidates()),
    )
    .await
    .map_err(|error| format!("等待 Codex Remote daemon 启动失败: {error}"))?
    .map(Some)
}

#[cfg(test)]
pub async fn ensure_for_active_proxy() -> Result<Option<CodexRemoteDaemonAction>, String> {
    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn fake_codex(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake codex");
        let mut permissions = fs::metadata(&path).expect("stat fake codex").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake codex");
        path
    }

    #[test]
    #[serial]
    fn running_daemon_only_enables_remote_control() {
        let dir = TempDir::new().expect("temp dir");
        let calls = dir.path().join("calls.log");
        let candidate = fake_codex(
            &dir,
            "codex",
            &format!(
                r#"printf '%s\n' "$*" >> '{}'
if [ "$*" = "app-server daemon version" ]; then
  printf '%s\n' '{{"status":"running"}}'
  exit 0
fi
if [ "$*" = "remote-control start --json" ]; then
  printf '%s\n' '{{"status":"connected"}}'
  exit 0
fi
exit 1"#,
                calls.display()
            ),
        );

        assert_eq!(
            ensure_with_candidates([candidate]),
            Ok(CodexRemoteDaemonAction::AlreadyRunning)
        );
        assert_eq!(
            fs::read_to_string(calls).expect("read calls"),
            "app-server daemon version\nremote-control start --json\n"
        );
    }

    #[test]
    #[serial]
    fn stopped_daemon_is_bootstrapped_with_remote_control() {
        let dir = TempDir::new().expect("temp dir");
        let calls = dir.path().join("calls.log");
        let candidate = fake_codex(
            &dir,
            "codex",
            &format!(
                r#"printf '%s\n' "$*" >> '{}'
if [ "$*" = "app-server daemon version" ]; then
  printf '%s\n' '{{"status":"stopped"}}'
  exit 0
fi
if [ "$*" = "app-server daemon bootstrap --remote-control" ]; then
  exit 0
fi
if [ "$*" = "remote-control start --json" ]; then
  printf '%s\n' '{{"status":"connected"}}'
  exit 0
fi
exit 1"#,
                calls.display()
            ),
        );

        assert_eq!(
            ensure_with_candidates([candidate]),
            Ok(CodexRemoteDaemonAction::Bootstrapped)
        );
        assert_eq!(
            fs::read_to_string(calls).expect("read calls"),
            "app-server daemon version\napp-server daemon bootstrap --remote-control\nremote-control start --json\n"
        );
    }

    #[test]
    #[serial]
    fn unsupported_cli_falls_back_to_daemon_capable_candidate() {
        let dir = TempDir::new().expect("temp dir");
        let unsupported = fake_codex(&dir, "old-codex", "exit 2");
        let supported = fake_codex(
            &dir,
            "new-codex",
            r#"if [ "$*" = "app-server daemon version" ]; then
  printf '%s\n' '{"status":"running"}'
  exit 0
fi
if [ "$*" = "remote-control start --json" ]; then
  printf '%s\n' '{"status":"connected"}'
  exit 0
fi
exit 1"#,
        );

        assert_eq!(
            ensure_with_candidates([unsupported, supported]),
            Ok(CodexRemoteDaemonAction::AlreadyRunning)
        );
    }

    #[test]
    #[serial]
    fn enabled_but_disconnected_remote_control_is_not_reported_as_success() {
        let dir = TempDir::new().expect("temp dir");
        let candidate = fake_codex(
            &dir,
            "codex",
            r#"if [ "$*" = "app-server daemon version" ]; then
  printf '%s\n' '{"status":"running"}'
  exit 0
fi
if [ "$*" = "remote-control start --json" ]; then
  printf '%s\n' '{"status":"errored"}'
  exit 1
fi
exit 1"#,
        );

        let error = ensure_with_candidates([candidate]).expect_err("remote should be errored");
        assert!(error.contains("remote-control start"));
        assert!(error.contains("errored"));
    }
}
