# SYNTHESIS_PROMPT — fold an audit into BRAIN.md

Input: one audit report from `docs/audits/`. Output: an edited
`docs/BRAIN.md` and nothing else.

Rules:

1. Reconcile every finding against the current code before accepting it.
   Reject findings the code disproves, and note the rejection in the audit
   history row.
2. Severity tags: `critical` (data loss, secret leak, security hole),
   `high` (wrong behaviour on a main path), `medium` (edge case, debt that
   slows work), `low` (cosmetic, docs).
3. Update the scorecard, per-module status, tech debt, and risks. Keep each
   fact in one place; link instead of repeating.
4. Resolved items are not deleted: strike through with the resolution date
   (`~~item~~ fixed 2026-10-01 in abc1234`) and keep them for at least one
   more audit cycle.
5. Never invent numbers, traction, or completion claims. Uncertain → mark as
   an estimate.
6. Append one row to Audit history: date, auditor, one-line summary.
7. Task-level follow-ups go to `docs/research/STATE.md` as `todo`, not here.
8. Commit as `docs(brain): …` — one commit per synthesis.
