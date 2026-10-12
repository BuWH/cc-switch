#!/bin/zsh

set -euo pipefail

[[ "$#" -eq 2 ]] || {
  print -u2 "Usage: check-release-source.sh <repository> <source-commit>"
  exit 64
}
repo="$1"
source_commit="$2"

git() {
  if [[ -x /Library/Developer/CommandLineTools/usr/bin/git ]]; then
    /Library/Developer/CommandLineTools/usr/bin/git "$@"
  else
    command git "$@"
  fi
}

fail() {
  print -u2 "Release source check failed: $1"
  exit 65
}

[[ "$source_commit" =~ '^[0-9a-f]{40}$' ]] ||
  fail "missing or invalid CCSwitchSourceCommit; rebuild from committed main"
[[ "$(git -C "$repo" branch --show-current)" == main ]] ||
  fail "run replacement from the main checkout"
[[ -z "$(git -C "$repo" status --porcelain)" ]] ||
  fail "main checkout has uncommitted changes"
[[ "$(git -C "$repo" rev-parse HEAD)" == "$source_commit" ]] ||
  fail "app was not built from current main"
[[ "$(git -C "$repo" rev-parse refs/remotes/origin/main)" == "$source_commit" ]] ||
  fail "main has not been synchronized with origin/main"

# Read the server too: a stale tracking ref must not authorize an old artifact.
remote_refs="$(git -C "$repo" -c http.lowSpeedLimit=1 -c http.lowSpeedTime=15 \
  ls-remote --exit-code origin refs/heads/main)" ||
  fail "cannot verify origin/main on the server"
remote_commit="$(printf '%s\n' "$remote_refs" | awk '$2 == "refs/heads/main" {print $1}')"
[[ "$remote_commit" == "$source_commit" ]] ||
  fail "server main differs from the app source commit"

# Recheck local state after the network request.
[[ "$(git -C "$repo" branch --show-current)" == main &&
   "$(git -C "$repo" rev-parse HEAD)" == "$source_commit" &&
   "$(git -C "$repo" rev-parse refs/remotes/origin/main)" == "$source_commit" &&
   -z "$(git -C "$repo" status --porcelain)" ]] ||
  fail "main changed during source verification"
print -r -- "SOURCE_OK commit=${source_commit}"
