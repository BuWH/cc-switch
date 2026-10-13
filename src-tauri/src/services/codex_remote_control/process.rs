use serde_json::{json, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub(super) struct CheckControl {
    pub epoch: Arc<AtomicU64>,
    pub generation: u64,
    pub deadline: Instant,
}

impl CheckControl {
    pub fn check(&self) -> Result<(), String> {
        if self.epoch.load(Ordering::Acquire) != self.generation {
            return Err("Remote check superseded by a configuration change".into());
        }
        if Instant::now() >= self.deadline {
            return Err("Codex Remote check timed out".into());
        }
        Ok(())
    }
}

// Capture in regular files: a detached grandchild inheriting stdout must not
// keep Command::output() (or a reader thread) waiting forever for pipe EOF.
struct BoundedChild {
    child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
    deadline: Instant,
    control: CheckControl,
    exited: bool,
}

impl BoundedChild {
    fn spawn(
        mut command: Command,
        timeout: Duration,
        control: &CheckControl,
        interactive: bool,
    ) -> Result<Self, String> {
        control.check()?;
        let stdout = NamedTempFile::new().map_err(|error| error.to_string())?;
        let stderr = NamedTempFile::new().map_err(|error| error.to_string())?;
        command
            .stdin(if interactive {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(stdout.reopen().map_err(|error| error.to_string())?)
            .stderr(stderr.reopen().map_err(|error| error.to_string())?);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let child = command.spawn().map_err(|error| error.to_string())?;
        Ok(Self {
            child,
            stdout,
            stderr,
            deadline: control.deadline.min(Instant::now() + timeout),
            control: control.clone(),
            exited: false,
        })
    }

    fn check(&self) -> Result<(), String> {
        self.control.check()?;
        if Instant::now() >= self.deadline {
            return Err("Codex command timed out".into());
        }
        for file in [&self.stdout, &self.stderr] {
            if file.as_file().metadata().map_err(|e| e.to_string())?.len() > MAX_OUTPUT_BYTES {
                return Err("Codex command exceeded the output limit".into());
            }
        }
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        let status = self.child.try_wait().map_err(|error| error.to_string())?;
        self.exited = status.is_some();
        Ok(status)
    }

    fn read_output(file: &NamedTempFile) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        file.reopen()
            .map_err(|error| error.to_string())?
            .take(MAX_OUTPUT_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    }
}

struct ProxyStream {
    child: BoundedChild,
    reader: File,
}

impl Read for ProxyStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.child.check().map_err(std::io::Error::other)?;
            let count = self.reader.read(buffer)?;
            if count != 0 {
                return Ok(count);
            }
            if self
                .child
                .try_wait()
                .map_err(std::io::Error::other)?
                .is_some()
            {
                // The last bytes may have arrived between read and try_wait.
                let count = self.reader.read(buffer)?;
                if count != 0 {
                    return Ok(count);
                }
                return Err(std::io::Error::other(format!(
                    "Codex control proxy exited: {}",
                    diagnostic(&BoundedChild::read_output(&self.child.stderr).unwrap_or_default())
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Write for ProxyStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.child.check().map_err(std::io::Error::other)?;
        self.child
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Codex control pipe closed"))?
            .write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.child
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Codex control pipe closed"))?
            .flush()
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if self.exited {
            return;
        }
        #[cfg(unix)]
        unsafe {
            // Only this command's process group, never a pre-existing daemon.
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // npm's node wrapper can own a native CLI child.
            if let Ok(mut killer) = Command::new("taskkill")
                .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                .creation_flags(0x08000000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let until = Instant::now() + Duration::from_secs(2);
                while matches!(killer.try_wait(), Ok(None)) && Instant::now() < until {
                    std::thread::sleep(POLL_INTERVAL);
                }
                let _ = killer.kill();
                let _ = killer.wait();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(1600)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(super) fn output_text(output: &Output) -> String {
    let stderr = diagnostic(&output.stderr);
    if stderr.is_empty() {
        diagnostic(&output.stdout)
    } else {
        stderr
    }
}

pub(super) fn run(
    command: Command,
    timeout: Duration,
    control: &CheckControl,
) -> Result<Output, String> {
    let mut child = BoundedChild::spawn(command, timeout, control, false)?;
    loop {
        if let Err(error) = child.check() {
            return Err(format!(
                "{error}: {}",
                diagnostic(&BoundedChild::read_output(&child.stderr)?)
            ));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(Output {
                status,
                stdout: BoundedChild::read_output(&child.stdout)?,
                stderr: BoundedChild::read_output(&child.stderr)?,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

pub(super) fn rpc(
    command: Command,
    home: &Path,
    method: &str,
    control: &CheckControl,
) -> Result<Value, String> {
    let child = BoundedChild::spawn(command, Duration::from_secs(5), control, true)?;
    let reader = child.stdout.reopen().map_err(|error| error.to_string())?;
    // `app-server proxy` relays raw bytes, not JSONL. The control socket speaks
    // WebSocket JSON-RPC; use the protocol library for masking and framing.
    let (mut socket, _) = tungstenite::client::client_with_config(
        "ws://localhost/",
        ProxyStream { child, reader },
        Some(
            tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(64 * 1024))
                .max_frame_size(Some(64 * 1024)),
        ),
    )
    .map_err(|error| error.to_string())?;
    send(
        &mut socket,
        json!({
            "id": 1, "method": "initialize",
            "params": {
                "clientInfo": {"name": "cc_switch_remote_health", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }
        }),
    )?;
    let initialized = response(&mut socket, 1)?;
    if let Some(actual) = initialized.get("codexHome").and_then(Value::as_str) {
        let expected = home.canonicalize().map_err(|error| error.to_string())?;
        if Path::new(actual).canonicalize().ok().as_ref() != Some(&expected) {
            return Err("Codex control socket belongs to another CODEX_HOME".into());
        }
    } else {
        return Err("Codex control socket did not identify its CODEX_HOME".into());
    }
    send(&mut socket, json!({"method": "initialized"}))?;
    let mut request = json!({"id": 2, "method": method});
    if method != "remoteControl/status/read" {
        // Never fall back to legacy parameterless mutation: it can change the
        // Desktop's shared, persisted remote-control preference.
        request["params"] = json!({"ephemeral": true});
    }
    send(&mut socket, request)?;
    let result = response(&mut socket, 2)?;
    let _ = socket.close(None);
    Ok(result)
}

fn send(socket: &mut tungstenite::WebSocket<ProxyStream>, value: Value) -> Result<(), String> {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("notification");
    socket
        .send(tungstenite::Message::Text(value.to_string().into()))
        .map_err(|error| {
            format!(
                "{method}: {error}; {}",
                diagnostic(
                    &BoundedChild::read_output(&socket.get_ref().child.stderr).unwrap_or_default()
                )
            )
        })
}

fn response(socket: &mut tungstenite::WebSocket<ProxyStream>, id: u64) -> Result<Value, String> {
    loop {
        let message = socket.read().map_err(|error| error.to_string())?;
        let tungstenite::Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!(
                "Codex control request rejected: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ));
        }
        return value
            .get("result")
            .cloned()
            .ok_or("Codex control response is missing its result".into());
    }
}
