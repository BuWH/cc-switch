#!/bin/zsh

set -euo pipefail

DEFAULT_APP_PATH="/Applications/CC Switch.app"
DEFAULT_PORT="15721"
DEFAULT_DELAY="30"

usage() {
  cat <<'EOF'
Usage:
  scripts/replace-cc-switch-app.sh --dmg <path> [options]

Options:
  --expected-version <version>  Require the DMG app to match this version.
  --app-path <path>             Installed app path (default: /Applications/CC Switch.app).
  --port <port>                 Proxy health-check port (default: 15721).
  --delay <seconds>             Delay before stopping the provider (default: 30).
  --check-only                  Validate, stage, sign, and clean up without replacing.
  -h, --help                    Show this help.

The replacement is delegated to a one-shot user launchd job. After scheduling,
the invoking Codex session must not run more commands: it should immediately
tell the user that the provider will briefly restart. The launchd worker will:

  1. quit the running CC Switch;
  2. atomically swap the pre-staged app into /Applications;
  3. start the new app and wait for /health;
  4. roll back and restart the old app if validation fails.

Per-run status and logs are written under:
  /private/tmp/cc-switch-replace-<run-id>/

The latest paths are also available at:
  /private/tmp/cc-switch-replace-latest.status
  /private/tmp/cc-switch-replace-latest.log
EOF
}

timestamp() {
  TZ=Asia/Shanghai date "+%Y-%m-%d %H:%M:%S %z"
}

app_version() {
  local app_path="$1"
  plutil -extract CFBundleShortVersionString raw \
    "$app_path/Contents/Info.plist" 2>/dev/null
}

validate_app() {
  local app_path="$1"
  local expected_version="$2"
  local host_arch="$3"
  local actual_version

  [[ -d "$app_path" ]] || {
    print -u2 "App bundle not found: $app_path"
    return 1
  }

  actual_version="$(app_version "$app_path")" || {
    print -u2 "Unable to read app version: $app_path"
    return 1
  }
  if [[ -n "$expected_version" && "$actual_version" != "$expected_version" ]]; then
    print -u2 "Version mismatch: expected=$expected_version actual=$actual_version"
    return 1
  fi

  lipo -archs "$app_path/Contents/MacOS/cc-switch" | tr " " "\n" |
    grep -Fxq "$host_arch" || {
    print -u2 "Architecture $host_arch not found in app binary"
    return 1
  }

  codesign --verify --deep --strict --verbose=2 "$app_path"
}

write_status() {
  local state="$1"
  local message="$2"
  local tmp_path="${STATUS_PATH}.tmp"

  {
    printf "run_id=%s\n" "$RUN_ID"
    printf "state=%s\n" "$state"
    printf "expected_version=%s\n" "$EXPECTED_VERSION"
    printf "app_path=%s\n" "$APP_PATH"
    printf "port=%s\n" "$PORT"
    printf "updated_at=%s\n" "$(timestamp)"
    printf "message=%s\n" "$message"
  } >"$tmp_path"
  mv -f "$tmp_path" "$STATUS_PATH"
}

notify_result() {
  local message="$1"
  /usr/bin/osascript -e \
    "display notification \"${message}\" with title \"CC Switch replacement\"" \
    >/dev/null 2>&1 || true
}

wait_for_stop() {
  local attempts="${1:-30}"
  local i

  for ((i = 0; i < attempts; i += 1)); do
    if ! pgrep -x cc-switch >/dev/null 2>&1 &&
      ! lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

stop_running_app() {
  if pgrep -x cc-switch >/dev/null 2>&1; then
    /usr/bin/osascript -e 'tell application "CC Switch" to quit' \
      >/dev/null 2>&1 || true
  fi

  if wait_for_stop 30; then
    return 0
  fi

  pkill -TERM -x cc-switch >/dev/null 2>&1 || true
  wait_for_stop 15
}

health_is_ready() {
  local body
  body="$(curl -fsS --connect-timeout 2 --max-time 5 \
    "http://127.0.0.1:${PORT}/health" 2>/dev/null)" || return 1
  print -r -- "$body" |
    grep -Eq '"status"[[:space:]]*:[[:space:]]*"healthy"'
}

wait_for_healthy_version() {
  local attempts="${1:-60}"
  local i

  for ((i = 0; i < attempts; i += 1)); do
    if [[ "$(app_version "$APP_PATH" 2>/dev/null || true)" == "$EXPECTED_VERSION" ]] &&
      pgrep -x cc-switch >/dev/null 2>&1 &&
      health_is_ready; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_any_healthy_app() {
  local attempts="${1:-60}"
  local i

  for ((i = 0; i < attempts; i += 1)); do
    if pgrep -x cc-switch >/dev/null 2>&1 && health_is_ready; then
      return 0
    fi
    sleep 1
  done
  return 1
}

rollback_install() {
  local reason="$1"
  local failed_path="${APP_PATH}.failed-${RUN_ID}"

  print -r -- "[$(timestamp)] replacement failed: $reason"
  stop_running_app || true

  if [[ -d "$APP_PATH" ]]; then
    mv "$APP_PATH" "$failed_path" || true
  fi

  if [[ -d "$BACKUP_PATH" ]]; then
    mv "$BACKUP_PATH" "$APP_PATH"
    open "$APP_PATH" >/dev/null 2>&1 || true
    if wait_for_any_healthy_app 60; then
      rm -rf "$failed_path"
      write_status "rolled_back" "$reason; previous version restored and healthy"
      notify_result "Update failed; previous version restored"
      return 0
    fi
  fi

  write_status "rollback_failed" "$reason; automatic rollback did not become healthy"
  notify_result "Update and automatic rollback failed; inspect the replacement log"
  return 1
}

worker_main() {
  RUN_ID="$1"
  EXPECTED_VERSION="$2"
  DELAY="$3"
  CANDIDATE_PATH="$4"
  APP_PATH="$5"
  PORT="$6"
  STATUS_PATH="$7"
  LOG_PATH="$8"
  BACKUP_PATH="${APP_PATH}.backup-${RUN_ID}"
  HOST_ARCH="$(uname -m)"

  exec >>"$LOG_PATH" 2>&1
  print -r -- "[$(timestamp)] worker started; delay=${DELAY}s"
  write_status "waiting" "launchd worker started; waiting before provider restart"
  sleep "$DELAY"

  write_status "stopping" "stopping current CC Switch"
  if ! validate_app "$CANDIDATE_PATH" "$EXPECTED_VERSION" "$HOST_ARCH"; then
    write_status "failed" "pre-staged app validation failed"
    notify_result "Update aborted: staged app validation failed"
    return 1
  fi
  if ! stop_running_app; then
    write_status "failed" "current CC Switch or proxy port did not stop"
    notify_result "Update aborted: current CC Switch did not stop"
    return 1
  fi

  write_status "replacing" "provider stopped; atomically replacing app"
  if [[ -d "$APP_PATH" ]]; then
    mv "$APP_PATH" "$BACKUP_PATH" || {
      write_status "failed" "unable to move current app to backup"
      return 1
    }
  fi

  if ! mv "$CANDIDATE_PATH" "$APP_PATH"; then
    [[ -d "$BACKUP_PATH" ]] && mv "$BACKUP_PATH" "$APP_PATH"
    open "$APP_PATH" >/dev/null 2>&1 || true
    write_status "rolled_back" "unable to move staged app into place"
    notify_result "Update failed; previous version restored"
    return 1
  fi

  if ! validate_app "$APP_PATH" "$EXPECTED_VERSION" "$HOST_ARCH"; then
    rollback_install "installed app validation failed"
    return $?
  fi

  write_status "starting" "new app installed; starting provider"
  if ! open "$APP_PATH" >/dev/null 2>&1; then
    rollback_install "unable to launch new app"
    return $?
  fi

  if ! wait_for_healthy_version 60; then
    rollback_install "new app did not restore a healthy provider on port ${PORT}"
    return $?
  fi

  rm -rf "$BACKUP_PATH"
  write_status "success" "CC Switch ${EXPECTED_VERSION} installed; provider healthy"
  notify_result "CC Switch ${EXPECTED_VERSION} installed; provider restored"
  print -r -- "[$(timestamp)] replacement completed successfully"
}

if [[ "${1:-}" == "--worker" ]]; then
  shift
  [[ "$#" -eq 8 ]] || {
    print -u2 "Invalid worker invocation"
    exit 64
  }
  worker_main "$@"
  exit $?
fi

DMG_PATH=""
EXPECTED_VERSION=""
APP_PATH="$DEFAULT_APP_PATH"
PORT="$DEFAULT_PORT"
DELAY="$DEFAULT_DELAY"
CHECK_ONLY=0

while [[ "$#" -gt 0 ]]; do
  case "$1" in
  --dmg)
    DMG_PATH="${2:-}"
    shift 2
    ;;
  --expected-version)
    EXPECTED_VERSION="${2:-}"
    shift 2
    ;;
  --app-path)
    APP_PATH="${2:-}"
    shift 2
    ;;
  --port)
    PORT="${2:-}"
    shift 2
    ;;
  --delay)
    DELAY="${2:-}"
    shift 2
    ;;
  --check-only)
    CHECK_ONLY=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    print -u2 "Unknown argument: $1"
    usage >&2
    exit 64
    ;;
  esac
done

[[ -n "$DMG_PATH" ]] || {
  print -u2 "--dmg is required"
  usage >&2
  exit 64
}
if [[ "$PORT" != <-> ]] || ((PORT < 1 || PORT > 65535)); then
  print -u2 "Invalid port: $PORT"
  exit 64
fi
if [[ "$DELAY" != <-> ]]; then
  print -u2 "Invalid delay: $DELAY"
  exit 64
fi
if [[ -n "$EXPECTED_VERSION" ]] &&
  ! print -r -- "$EXPECTED_VERSION" |
    grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
  print -u2 "Invalid expected version: $EXPECTED_VERSION"
  exit 64
fi

DMG_DIR="$(cd "$(dirname "$DMG_PATH")" && pwd)"
DMG_PATH="${DMG_DIR}/$(basename "$DMG_PATH")"
[[ -f "$DMG_PATH" ]] || {
  print -u2 "DMG not found: $DMG_PATH"
  exit 66
}

RUN_ID="$(TZ=Asia/Shanghai date "+%Y%m%d-%H%M%S")-$$"
RUN_DIR="/private/tmp/cc-switch-replace-${RUN_ID}"
MOUNT_PATH="${RUN_DIR}/mount"
STATUS_PATH="${RUN_DIR}/status"
LOG_PATH="${RUN_DIR}/worker.log"
HOST_ARCH="$(uname -m)"
SCRIPT_PATH="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
MOUNTED=0

mkdir -p "$MOUNT_PATH"
cleanup_mount() {
  if [[ "$MOUNTED" -eq 1 ]]; then
    hdiutil detach "$MOUNT_PATH" >/dev/null 2>&1 || true
    MOUNTED=0
  fi
}
trap cleanup_mount EXIT INT TERM

SHA_FILE="${DMG_PATH}.sha256"
if [[ -f "$SHA_FILE" ]]; then
  EXPECTED_SHA="$(awk 'NR == 1 {print $1}' "$SHA_FILE")"
  ACTUAL_SHA="$(shasum -a 256 "$DMG_PATH" | awk '{print $1}')"
  [[ -n "$EXPECTED_SHA" && "$ACTUAL_SHA" == "$EXPECTED_SHA" ]] || {
    print -u2 "DMG SHA-256 mismatch"
    exit 65
  }
fi

hdiutil verify "$DMG_PATH"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT_PATH" "$DMG_PATH"
MOUNTED=1

SOURCE_APP="${MOUNT_PATH}/CC Switch.app"
[[ -d "$SOURCE_APP" ]] || {
  print -u2 "CC Switch.app not found in DMG"
  exit 65
}

DMG_VERSION="$(app_version "$SOURCE_APP")"
if [[ -z "$EXPECTED_VERSION" ]]; then
  EXPECTED_VERSION="$DMG_VERSION"
elif [[ "$DMG_VERSION" != "$EXPECTED_VERSION" ]]; then
  print -u2 "DMG version mismatch: expected=$EXPECTED_VERSION actual=$DMG_VERSION"
  exit 65
fi

if [[ "$CHECK_ONLY" -eq 1 ]]; then
  CANDIDATE_PATH="${RUN_DIR}/CC Switch.app"
else
  CANDIDATE_PATH="/Applications/.CC Switch.app.installing-${RUN_ID}"
  [[ ! -e "$CANDIDATE_PATH" ]] || {
    print -u2 "Candidate path already exists: $CANDIDATE_PATH"
    exit 73
  }
fi

ditto "$SOURCE_APP" "$CANDIDATE_PATH"
codesign --force --deep --sign - --timestamp=none "$CANDIDATE_PATH"
validate_app "$CANDIDATE_PATH" "$EXPECTED_VERSION" "$HOST_ARCH"
cleanup_mount

if [[ "$CHECK_ONLY" -eq 1 ]]; then
  rm -rf "$RUN_DIR"
  print -r -- "CHECK_OK version=${EXPECTED_VERSION} arch=${HOST_ARCH}"
  exit 0
fi

write_status "scheduled" "replacement staged; launchd worker will start after ${DELAY}s"
ln -sfn "$STATUS_PATH" /private/tmp/cc-switch-replace-latest.status
ln -sfn "$LOG_PATH" /private/tmp/cc-switch-replace-latest.log

LABEL="com.ccswitch.replace.${RUN_ID}"
if ! launchctl submit -l "$LABEL" -- \
  /bin/zsh "$SCRIPT_PATH" --worker \
  "$RUN_ID" "$EXPECTED_VERSION" "$DELAY" "$CANDIDATE_PATH" \
  "$APP_PATH" "$PORT" "$STATUS_PATH" "$LOG_PATH"; then
  rm -rf "$CANDIDATE_PATH"
  write_status "failed" "unable to submit launchd replacement worker"
  print -u2 "Unable to submit launchd replacement worker"
  exit 70
fi

print -r -- "SCHEDULED"
print -r -- "run_id=${RUN_ID}"
print -r -- "version=${EXPECTED_VERSION}"
print -r -- "delay=${DELAY}"
print -r -- "status=${STATUS_PATH}"
print -r -- "log=${LOG_PATH}"
