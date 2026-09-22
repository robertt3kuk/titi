---
name: skill-authoring
description: How to turn a recurring multi-step procedure in titi into a new project skill under .agents/skills, and wire it for every agent. Use when a 3+ step routine repeats, or the user says "make this a skill".
---

# skill-authoring

## When
- The same 3+ step procedure has happened at least twice, or the user asks.
  Never for a one-off or a hypothetical future need.
- Check `.agents/skills/` first: extend an existing skill rather than add a
  near-duplicate.

## Where
- `.agents/skills/<name>/SKILL.md` in this repo. Never `~/.agents/skills` or
  any user/global directory, even if the session has them loaded.
- Discovery: omp, Cline, grok, Antigravity, and Gemini CLI read
  `.agents/skills/` natively. Claude Code reads `.claude/skills/` only —
  add a symlink: `ln -s ../../.agents/skills/<name> .claude/skills/<name>`,
  and commit it together with the SKILL.md.

## Shape
- Frontmatter: `name` (kebab-case, = folder) and `description` — what it
  does plus "Use when …" phrased like a natural request.
- English. 30–80 lines. Specific to titi: real paths, commands from
  `ci.yml`, crate names. No generic templates.
- Point to canon (`AGENTS.md`, `docs/COMMITS.md`, `docs/CONVEYOR.md`) instead
  of restating it.

## Finish
1. Dogfood: give only the SKILL.md to a fresh subagent with a natural
   request that should trigger it; fix the description or steps if it
   misfires.
2. Commit as `docs(skills): add <name> …` (skill `commit`), push.
3. Mention the new skill in your final report to the user.
