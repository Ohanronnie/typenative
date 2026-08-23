#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/typenative-worktree-state-test.XXXXXX")
trap 'rm -r -- "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.email verification@typenative.invalid
git -C "$fixture" config user.name 'TypeNative Verification'
printf '%s\n' baseline >"$fixture/tracked.txt"
git -C "$fixture" add tracked.txt
git -C "$fixture" commit -qm baseline

printf '%s\n' dirty >"$fixture/tracked.txt"
printf '%s\n' retained >"$fixture/untracked.txt"
snapshot="$fixture/.git/worktree-state.json"
(
  cd "$fixture"
  "$root/scripts/worktree-state.py" capture "$snapshot"
  "$root/scripts/worktree-state.py" compare "$snapshot"
)

printf '%s\n' changed-again >"$fixture/tracked.txt"
if (cd "$fixture" && "$root/scripts/worktree-state.py" compare "$snapshot") >/dev/null 2>&1; then
  printf '%s\n' 'worktree comparison missed an edit to an already-dirty file' >&2
  exit 1
fi

printf '%s\n' dirty >"$fixture/tracked.txt"
printf '%s\n' changed-untracked >"$fixture/untracked.txt"
if (cd "$fixture" && "$root/scripts/worktree-state.py" compare "$snapshot") >/dev/null 2>&1; then
  printf '%s\n' 'worktree comparison missed an edit to a retained untracked file' >&2
  exit 1
fi

printf '%s\n' 'worktree-preservation-regression=pass dirty-tracked=true untracked=true'
