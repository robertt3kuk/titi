#!/bin/sh
# Commit by pathspec, inside the workspace commit lock, in one step.
#
# Several agents share this checkout's index, and `git add` followed by a
# separate `git commit` is not atomic: between them another agent can stage its
# own file and the commit carries it. Doing both under
# `$(git rev-parse --git-dir)/titi-commit.lock`, with a check that the cached
# set is exactly the paths asked for, is the whole point of this script.
#
# Usage: scripts/commit.sh <message-file> <path>...
#
# The lock is taken with a bounded retry, the cached set is printed and
# refused if it holds anything outside <paths>, the commit is made with a
# pathspec, the paths are checked clean afterwards, and the lock is released
# on every exit path. No `amend`, no reset, no push.
set -eu

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <message-file> <path>..." >&2
    exit 2
fi

message_file=$1
shift
if [ ! -f "$message_file" ]; then
    echo "commit: no message file $message_file" >&2
    exit 2
fi

git_dir=$(git rev-parse --git-dir)
lock="$git_dir/titi-commit.lock"

# A stale lock is the holder's, never ours to delete: report and stop.
n=0
while ! mkdir "$lock" 2>/dev/null; do
    n=$((n + 1))
    if [ "$n" -gt 300 ]; then
        echo "commit: gave up waiting for $lock after ${n}s" >&2
        exit 1
    fi
    sleep 1
done

released=0
release() {
    if [ "$released" -eq 0 ]; then
        released=1
        rmdir "$lock" 2>/dev/null || true
    fi
}
trap release EXIT HUP INT TERM

git add -- "$@"

echo "commit: cached:"
git diff --cached --name-only

# Anything cached that was not asked for is somebody else's work in flight.
wanted=$(printf '%s\n' "$@" | sort)
staged=$(git diff --cached --name-only | sort)
extra=$(printf '%s\n' "$staged" | while IFS= read -r f; do
    printf '%s\n' "$wanted" | grep -qxF -- "$f" || printf '%s\n' "$f"
done)
if [ -n "$extra" ]; then
    echo "commit: refusing — the cached set holds paths outside the argument list:" >&2
    printf '%s\n' "$extra" >&2
    exit 1
fi

git commit -F "$message_file" -- "$@"

dirty=$(git status --short -- "$@")
if [ -n "$dirty" ]; then
    echo "commit: these paths are still dirty after the commit:" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
fi
