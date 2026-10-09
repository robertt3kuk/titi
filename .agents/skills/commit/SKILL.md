---
name: commit
description: Commit and push a change in titi the project way — one concern per commit, Conventional Commits with a prose body, straight to origin master, never force. Use when asked to commit, push, save, or ship a change.
---

# commit

Rules live in `docs/COMMITS.md`; this is the procedure.

## Several agents, one index

The index is a single resource shared by every agent in the checkout. `git add`
and the `commit` that follows are not atomic: between them another agent can
stage its own file, and your commit will carry someone else's work. Real
symptoms seen today: a commit whose subject names one crate and whose stat
lists another; a docs fix swept into a peer's commit three commits in a row so
its message never matched its content.

Serialize:

- Take the workspace commit lock **before** staging, release it **after** the
  commit — and put the `git add` **inside** the lock, never before it. A
  `git add` that runs before `mkdir` succeeds is staged for the whole checkout
  to sweep:

  ```sh
  while ! mkdir "$(git rev-parse --git-dir)/titi-commit.lock" 2>/dev/null; do
    sleep 1
  done
  git add <paths>                 # inside the lock, by explicit path
  git diff --cached --name-only   # must print exactly what you staged
  git commit -F - -- <paths>      # commit by pathspec, so nothing else in the
                                  # index can ride along
  git status --short -- <paths>   # must be empty afterwards
  rmdir "$(git rev-parse --git-dir)/titi-commit.lock"
  ```
  The lock directory belongs to whoever created it; a stale one must be
  reported (to the holder or the user), never deleted.
- Stage by explicit path. Never `git add -A`, `git add .`, or `git commit
  -am` — that is how sibling work gets swept in.
- Before committing: `git diff --cached --name-only` and `git status --short`
  — anything staged that you did not stage is a stop sign.
- Never `git commit --amend`, and never `reset` history another agent may be
  building on.
- Right after committing: `git show --stat --format='%h %s' HEAD`. If it shows
  a path you did not stage, say so in your report — do not rewrite the commit
  to hide it.
- Never delete `.git/index.lock`: it means another agent is mid-commit. Wait
  and retry.

Two commits in this session are misattributed by exactly this: `f3b11ef` swept
the `agent_tool.rs`, `agents.rs`, `runtime.rs` and `tests/agent_tool.rs` that
were staged for `c1b2260` — which then carried nothing but its changelog line —
and several changelog lines landed in sibling commits beside the features they
describe.

1. `git status` and `git diff` — see exactly what changed. Unrelated hunks
   or files → split into separate commits (`git add -p` is interactive and
   unavailable; stage whole files or write patches).
2. Never stage secrets, `.env*`, `*.db`, `target/`, or anything under
   `~/.titi/agent`.
3. Subject: `type(scope): summary` — English, imperative, lowercase, no
   trailing dot, ~50 cols. Types: feat fix docs refactor test ci style chore.
   Scope: crate short name (`cli`, `engine`, `tui`, `genome`, …) or `ci`,
   `cargo`, `workspace`, `readme`, `agents`, `commits`, `conveyor`, `skills`,
   `brain`.
4. Body, wrapped ~78 cols, prose: why, what invariant now holds, why this
   way and not the alternative. No filler. No `Co-Authored-By` trailer.
5. Commit with a heredoc (`git commit -F -`) so the body keeps its wrapping.
6. Push right away: `git push origin master`.
   - Rejected (non-fast-forward) → `git fetch origin && git rebase
     origin/master`, then push again.
   - Rebase conflict → `git rebase --abort`, stop, report to the user.
   - Never `--force`, `--force-with-lease`, `reset --hard`, `clean -fd`.
7. After push: `gh run list --repo robertt3kuk/titi --branch master --limit 1`
   and, for code changes, follow skill `ci-and-tests` until green.
