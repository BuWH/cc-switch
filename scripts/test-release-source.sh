#!/bin/zsh

set -euo pipefail
script_dir="$(cd "$(dirname "$0")" && pwd)"
temp="$(mktemp -d)"
trap 'rm -rf "$temp"' EXIT
remote="$temp/origin.git"
repo="$temp/repo"
git init --bare --quiet "$remote"
git init --quiet -b main "$repo"
git -C "$repo" config user.name "Release test"
git -C "$repo" config user.email "release-test@example.invalid"
git -C "$repo" config commit.gpgsign false
git -C "$repo" remote add origin "$remote"
print -r -- one > "$repo/source"
git -C "$repo" add source
git -C "$repo" commit --quiet -m one
git -C "$repo" push --quiet -u origin main
first="$(git -C "$repo" rev-parse HEAD)"

check() {
  /bin/zsh "$script_dir/check-release-source.sh" "$repo" "$1" > "$temp/result" 2>&1
}
reject() {
  if check "$1"; then
    print -u2 "Unexpectedly accepted: $2"
    exit 1
  fi
  grep -Fq "$3" "$temp/result"
  print -r -- "PASS rejected $2"
}

check "$first"
print -r -- "PASS synchronized clean main"
reject "" "missing provenance" "missing or invalid"
reject "HEAD" "symbolic provenance" "missing or invalid"
print -r -- dirty > "$repo/source"
reject "$first" "modified tracked file" "uncommitted changes"
print -r -- one > "$repo/source"
print -r -- new > "$repo/untracked"
reject "$first" "untracked source" "uncommitted changes"
rm "$repo/untracked"
git -C "$repo" switch --quiet -c fix/test
reject "$first" "feature checkout" "main checkout"
git -C "$repo" switch --quiet main
git -C "$repo" checkout --quiet --detach
reject "$first" "detached checkout" "main checkout"
git -C "$repo" switch --quiet main
git -C "$repo" commit --quiet --allow-empty -m two
second="$(git -C "$repo" rev-parse HEAD)"
reject "$first" "old same-version artifact" "current main"
reject "$second" "unpushed fix" "synchronized with origin/main"
git -C "$repo" push --quiet origin main
check "$second"
print -r -- "PASS merged and pushed fix"

# Move only the server ref, leaving the local tracking ref deliberately stale.
git -C "$repo" commit --quiet --allow-empty -m three
third="$(git -C "$repo" rev-parse HEAD)"
git -C "$repo" push --quiet origin "${third}:refs/heads/test-candidate"
git --git-dir="$remote" update-ref refs/heads/main "$third"
git -C "$repo" update-ref refs/heads/main "$second" "$third"
reject "$second" "stale origin/main" "server main differs"
git -C "$repo" remote set-url origin "$temp/missing.git"
reject "$second" "unreachable origin" "cannot verify"
