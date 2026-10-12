# CC Switch Workspace Rules

## Branching Policy

- `main` is the only long-lived integration baseline.
- Start every feature or fix in a new branch/worktree created from the latest
  `main`; do not base new work on an older feature branch.
- A completed fix must be committed, tested, merged into local `main`, and pushed
  to `origin/main` without force before it is delivered or installed.
- Inspect every existing worktree's staged/unstaged changes as well as unmerged
  commits before preparing an installation. A branch HEAD alone does not show
  uncommitted fixes. Review and integrate completed fixes intended for release;
  explicitly report unfinished/unrelated work and leave its worktree untouched.
- Upstream release sync compares `main` with `upstream/main`.
- After an upstream sync passes all tests and artifact validation, fast-forward
  the verified result into local `main` and push `origin/main` without force.
- Never overwrite a moved local or remote `main`; stop and reconcile concurrent
  work first.

## Artifact Provenance

- Build only from clean, committed source. Record the full source commit before
  building and verify that HEAD and the source tree remain unchanged afterwards.
- Before signing the final staging app, set `CCSwitchSourceCommit` in its
  `Contents/Info.plist` to that actual build commit, then repackage and validate.
  Never relabel an old binary with a new source commit.
- An installation requires the exact artifact commit to equal clean local
  `main`, `origin/main`, and the server's `refs/heads/main`. The version number
  alone is not sufficient. Keep same-version rebuilds in separate directories.
- Use the latest replacement script in the main checkout, never an old feature
  worktree's copy. Missing provenance or a stale commit requires a rebuild.
- See `FORK_NOTES.md` for release checks and `scripts/test-release-source.sh`
  for the source gate regression tests.

## Runtime Safety

The installed `/Applications/CC Switch.app` is the active provider for Codex
and normally owns `127.0.0.1:15721`.

- Never stop the installed app and depend on a later tool call to copy, replace,
  launch, or validate the new app. Stopping the provider can disconnect the
  current agent before those later calls run.
- Build and validation tasks must not stop, replace, restart, or install the
  running app unless the user explicitly asks for installation or replacement.
- For an explicit replacement request, use
  `scripts/replace-cc-switch-app.sh`. It pre-stages the validated app and
  delegates stop, atomic replacement, restart, health validation, and rollback
  to a one-shot user launchd job.
- After the script returns `SCHEDULED`, do not run another tool call. Immediately
  tell the user that the provider will briefly restart and end the response.
- On the next turn, inspect:
  `/private/tmp/cc-switch-replace-latest.status` and
  `/private/tmp/cc-switch-replace-latest.log`.
