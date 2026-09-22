# COMMITS — commit and push discipline

The model is Orca-grade: many focused commits, each one explaining itself.
A live example:
https://github.com/stablyai/orca/commit/9ece2730561375979405825462c812167d6bbbc1

## Format

Conventional Commits: `type(scope): summary`.

- English, imperative, lowercase subject, no trailing period.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `ci`, `style`, `chore`.
- Scopes used in this repo: `ci`, `cargo`, `workspace`, `readme`, `agents`,
  `commits`, `conveyor`, `skills`, `brain`, `audits`, `qa`, plus crate names
  (`cli`, `engine`, `tui`, …).
- No `Co-Authored-By` or other agent trailers.

```
fix(workspace): include titi-secrets as a member so its tests run
docs(readme): fix the broken "continue the work" links
ci: run fmt, clippy, and tests on GitHub Actions
```

## One concern, one commit

Each commit changes exactly one thing. If the description needs "and",
"plus", or a second unrelated file, it is two commits. Keep a refactor apart
from a feature, and a rename apart from a logic change.

## The body is prose, not a checklist

A few dense sentences, wrapped at about 78 columns. The body answers:

- **Why**: the problem or goal, not a retelling of the diff.
- **Invariant**: what is now guaranteed or protected.
- **Tradeoff**: why this way and not the alternative.

No filler, no "as requested", no "updated code".

## Push

- Straight to `master`: `git push origin master`.
- Never force-push (`-f`, `--force`, and `--force-with-lease` are all
  banned).
- If origin moved ahead (Beka pushed), run `git fetch origin && git rebase
  origin/master`, then push normally. If the rebase conflicts, stop and
  report instead of rewriting history.

## Template

`.gitmessage` is set up as this repo's commit template (`git config
commit.template .gitmessage`). It holds the subject convention and prompts
for the body.
