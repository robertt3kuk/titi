---
name: brain-audit
description: Run the titi audit cycle — produce an audit report with docs/AUDIT_PROMPT.md, then fold it into docs/BRAIN.md with docs/SYNTHESIS_PROMPT.md. Use when asked to audit the project, update the brain, or refresh the project health scorecard.
---

# brain-audit

BRAIN.md changes only through this cycle.

## 1. Audit
- Hand `docs/AUDIT_PROMPT.md` (the part below the rule) to an auditor with
  repo access. In omp: `reviewer` (@review) for the general pass, plus
  `security` for section 3–4 findings; `second-opinion` for a cross-check.
- Other harnesses: one fresh read-only subagent per role (general,
  security). Create `docs/audits/` if it does not exist yet.
- Auditors read code, not reports. Evidence is file:line.
- Save the raw output as `docs/audits/<YYYY-MM-DD>-<auditor>.md`.
  Commit: `docs(audits): add <date> audit by <auditor>`.

## 2. Synthesis
- Follow `docs/SYNTHESIS_PROMPT.md` exactly: verify each finding against
  current code, tag severity, strike through resolved items with a date,
  never invent numbers, append the audit-history row.
- Task follow-ups go to `docs/research/STATE.md` as `todo`.
- Commit: `docs(brain): synthesize <date> audit`.

## 3. Report
Tell the user: top findings by severity, what changed in the scorecard,
what was rejected as disproved by code.
