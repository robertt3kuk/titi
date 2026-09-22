---
name: commit
description: Commit and push a change in titi the project way — one concern per commit, Conventional Commits with a prose body, straight to origin master, never force. Use when asked to commit, push, save, or ship a change.
---

# commit

Rules live in `docs/COMMITS.md`; this is the procedure.

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
