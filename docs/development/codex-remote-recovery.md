# Codex Remote Recovery

The supervisor is attached to `ProxyService` and starts once after application
startup recovery. It only manages remote control while the local proxy and Codex
takeover are active, ChatGPT login material is present, the active provider uses
native OpenAI auth, and `features.remote_control = true`.

## Ownership and Safety

- Desktop has priority. Its presence is not presented as proof of a healthy
  remote connection.
- When Desktop is absent, a stopped managed daemon is started using
  `app-server daemon start`. CLI capability and daemon liveness are separate:
  missing/refused control sockets are recoverable, but arbitrary probe errors
  are not treated as a stopped daemon.
- An existing control socket must identify the expected `CODEX_HOME`, and the
  lifecycle probe must identify a PID-managed daemon before remote mutations.
  Unmanaged app-servers are left alone.
- A daemon yields using `remoteControl/disable` with `ephemeral: true`. After
  Desktop exits, it can resume using the corresponding ephemeral enable RPC.
  Unsupported ephemeral requests fail closed, without a parameterless fallback.
- Recovery never calls daemon `stop`, `restart`, or `bootstrap`, edits
  `auth.json`, or overwrites the persisted remote enable/disable preference.
  In-flight app-server tasks are not terminated.
- `app-server proxy` is a raw byte relay, not JSONL. The client uses WebSocket
  JSON-RPC through this relay and checks initialization before sending commands.

## Scheduling and Diagnostics

Ownership is checked every five seconds. Healthy connections are checked every
30 seconds; failures back off from five to 120 seconds. Desktop transitions,
control-socket replacement, manual checks, and a detected wake gap bypass the
old retry deadline. A connecting worker is given 120 seconds before an
ephemeral reconnect is attempted.

External commands have individual deadlines and share a 45-second check
budget. Output is bounded, and temporary command processes are terminated and
reaped on timeout or cancellation. Detached daemon processes are not killed.
Configuration writes invalidate the current check generation. The supervisor
does not hold the provider-switch lock while running commands.

CLI subprocesses receive the selected `CODEX_HOME` and a PATH augmented with
known Node installation directories. Node discovery does not depend on a
`codex` wrapper being installed in the same Node version, and no login shell
configuration is executed.

The local-route settings show remote ownership/state, last check, next retry,
and the last failure. The retry button schedules a new check; it does not
toggle takeover or restart either application.

Desktop's own remote network connection remains managed by Codex. CC Switch
does not independently certify or restart that connection. Process-ownership
inspection failures defer recovery rather than assuming no other owner exists.
Unix process-owner parsing accepts both unsigned UIDs and signed 32-bit `ps`
representations (such as macOS `-2`), while invalid or out-of-range values still
defer recovery.

## Verification

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib services::codex_remote_control
pnpm exec vitest run tests/components/CodexRemoteStatusRow.test.tsx
pnpm typecheck
```

The Rust tests exercise real child-process pipes and WebSocket framing against
isolated local fixtures. They never start a real daemon. An optional wire test
can use an installed CLI against a private test socket:

```sh
CODEX_REMOTE_TEST_CLI="/Applications/ChatGPT.app/Contents/Resources/codex" \
  cargo test --manifest-path src-tauri/Cargo.toml --lib \
  official_cli_proxy_speaks_the_expected_websocket_protocol -- --ignored
```

This test sends only initialization and a status read to the test server.
Compatibility was verified locally with CLI 0.147.0 and 0.153.4 on macOS.

To check the actual host process list without changing any running service:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib \
  live_desktop_process_probe_is_read_only -- --ignored --nocapture
```
