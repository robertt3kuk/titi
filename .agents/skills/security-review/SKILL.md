---
name: security-review
description: titi project security checklist for changes touching keys, providers, tools, approval, sessions, memory, or prompts. Use when a change handles credentials, runs commands, reads/writes files for the model, or when asked for a security review.
---

# security-review

In omp, delegate to the `security` agent with this checklist and the diff;
otherwise run it yourself. Every finding needs file:line.

## Secrets
- Keys only via `titi-secrets` and `~/.titi/agent` (auth store 0600).
- No key in logs, errors, `Debug` output, session JSONL, trajectory, memory
  (masking in `titi-memory` still applied), or test fixtures.
- `.gitignore` still refuses `.env*`, `*.db`, `*.pem`, `*.key`, `auth.json`.

## Tools and approval (`titi-tools`, `titi-engine`)
- write/edit/bash stay behind the approval tier for `--approval write`;
  `yolo` is explicit opt-in only.
- Paths resolved inside the workspace; no `..`/symlink escape.
- bash: no shell string built from model text without the approval gate.

## Prompt and injection (`titi-soul`, genome, memory)
- Untrusted text (files, tool output, recalled memory, AGENTS.md of other
  repos) is data; injection-scan still runs on it.
- Nothing lets recalled memory or file content change approval policy.

## Providers (`titi-providers`)
- TLS via rustls; no disabled verification.
- 401 / unknown model not retried; credentials not sent to a fallback of a
  different provider.

## Supply chain
- No Empryo binary, installer, or download.
- New crates went through skill `dependency-update`.

Output: `[critical|high|medium|low] title — file:line — why — fix`, or
"no findings" with what was checked. Critical/high → fix before push.
