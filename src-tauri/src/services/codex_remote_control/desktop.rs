use super::process::{self, CheckControl};
use std::process::Command;
use std::time::Duration;

fn is_desktop_executable(path: &str) -> bool {
    let path = path.replace('\\', "/");
    if path.ends_with(".app/Contents/MacOS/ChatGPT") || path.ends_with(".app/Contents/MacOS/Codex")
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    if path.ends_with("/Codex") || path.ends_with("/codex-desktop") {
        return true;
    }
    #[cfg(windows)]
    {
        let path = path.to_ascii_lowercase();
        if (path.ends_with("/codex.exe") || path.ends_with("/chatgpt.exe"))
            && ![
                "/resources/",
                "/vendor/",
                "/bin/",
                "/.codex/",
                "/node_modules/",
            ]
            .iter()
            .any(|part| path.contains(part))
        {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn from_process_list(text: &str, uid: u32) -> Result<bool, String> {
    let mut parsed = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let line = line.trim();
        let (_, rest) = line
            .split_once(char::is_whitespace)
            .ok_or("Invalid process list")?;
        let rest = rest.trim_start();
        let (owner, executable) = rest
            .split_once(char::is_whitespace)
            .ok_or("Invalid process list")?;
        // macOS ps can render a 32-bit uid_t as signed, e.g. nobody as -2.
        let owner = owner
            .parse::<u32>()
            .or_else(|_| owner.parse::<i32>().map(|uid| uid as u32))
            .map_err(|_| "Invalid process owner")?;
        parsed += 1;
        if owner == uid && is_desktop_executable(executable.trim()) {
            return Ok(true);
        }
    }
    if parsed == 0 {
        return Err("Process inspection returned no processes".into());
    }
    Ok(false)
}

pub(super) fn running(control: &CheckControl) -> Result<bool, String> {
    #[cfg(unix)]
    {
        let mut command = Command::new("/bin/ps");
        command.args(["-axo", "pid=,uid=,comm="]);
        let output = process::run(command, Duration::from_secs(3), control)?;
        if !output.status.success() {
            return Err("Cannot inspect Codex Desktop processes; remote recovery deferred".into());
        }
        from_process_list(&String::from_utf8_lossy(&output.stdout), unsafe {
            libc::geteuid()
        })
    }
    #[cfg(windows)]
    {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $session=(Get-Process -Id $PID).SessionId; @(Get-Process | Where-Object { $_.SessionId -eq $session -and $_.ProcessName -in @('Codex','ChatGPT') } | Select-Object -ExpandProperty Path) | ConvertTo-Json -Compress",
        ]);
        let output = process::run(command, Duration::from_secs(3), control)?;
        if !output.status.success() {
            return Err("Cannot inspect Codex Desktop processes; remote recovery deferred".into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        if text.trim().is_empty() {
            return Ok(false);
        }
        let value: serde_json::Value =
            serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        let paths = match value {
            serde_json::Value::Array(paths) => paths,
            path => vec![path],
        };
        for path in paths {
            let path = path
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or("Cannot determine a Codex process executable; remote recovery deferred")?;
            if is_desktop_executable(path) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    #[cfg(not(any(unix, windows)))]
    Err("Codex Desktop ownership detection is unavailable on this platform".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn detects_desktop_with_spaces_but_not_helpers_or_other_users() {
        assert!(from_process_list(
            "17 501 /Users/me/My Apps/Codex.app/Contents/MacOS/Codex\n",
            501
        )
        .unwrap());
        assert!(!from_process_list(
            "17 502 /Applications/ChatGPT.app/Contents/MacOS/ChatGPT\n\
             18 501 /Applications/ChatGPT.app/Contents/Resources/codex\n\
             19 501 /Applications/ChatGPT.app/Contents/MacOS/Codex (Renderer)\n",
            501
        )
        .unwrap());
    }

    #[test]
    fn unknown_ownership_fails_closed() {
        assert!(from_process_list("", 501).is_err());
        assert!(from_process_list("unexpected output", 501).is_err());
    }

    #[test]
    fn signed_system_uid_does_not_hide_a_later_desktop_process() {
        let processes = "8493 -2 /usr/libexec/dhcp6d\n\
                         23375 501 /Applications/ChatGPT.app/Contents/MacOS/ChatGPT\n";
        assert!(from_process_list(processes, 501).unwrap());
        assert!(!from_process_list("8493 -2 /usr/libexec/dhcp6d\n", 501).unwrap());
    }

    #[test]
    fn signed_and_unsigned_uid_representations_match_the_same_owner() {
        for (text, uid) in [
            ("-2", u32::MAX - 1),
            ("4294967294", u32::MAX - 1),
            ("-2147483648", 2147483648),
            ("2147483648", 2147483648),
            ("501", 501),
        ] {
            let processes = format!("17 {text} /Applications/Codex.app/Contents/MacOS/Codex\n");
            assert!(from_process_list(&processes, uid).unwrap(), "{text}");
            assert!(!from_process_list(&processes, 502).unwrap(), "{text}");
        }
    }

    #[test]
    fn invalid_or_out_of_range_uids_still_fail_closed() {
        for uid in ["nobody", "-2147483649", "4294967296", "501x"] {
            let processes = format!("17 {uid} /Applications/Codex.app/Contents/MacOS/Codex\n");
            assert!(from_process_list(&processes, 501).is_err(), "{uid}");
        }
    }

    #[test]
    #[ignore = "read-only smoke test against the local host's real process list"]
    fn live_desktop_process_probe_is_read_only() {
        let control = CheckControl {
            epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            generation: 0,
            deadline: std::time::Instant::now() + Duration::from_secs(10),
        };
        let present = running(&control).expect("real process list must be readable");
        println!("desktop_present={present}");
    }
}
