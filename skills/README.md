# Repo skills

Agent-facing skills committed with the repo. Each `skills/<id>/SKILL.md` carries its
own frontmatter (`name`, `description`) — that is where discovery descriptions live.
[`registry.json`](registry.json) is the machine-readable index holding only what the
frontmatter does not: `kind` (contract vs runbook), `audience`, `platform`, `requires`.
Schema: [`registry.schema.json`](registry.schema.json).

| id | kind | audience |
|---|---|---|
| [`mega-bench-data`](mega-bench-data/SKILL.md) | contract — drive the reporter, consume its data, compose cards | consumer-agent |
| [`provision-instructions-lane`](provision-instructions-lane/SKILL.md) | runbook — bring a Linux host up for the instructions lane | operator-agent |

## Adding or changing a skill

1. Create/edit `skills/<id>/SKILL.md`; frontmatter `name` must equal `<id>`, and the
   `description` must start with `Use when` (single line, ≤ 1024 chars).
2. Register `<id>` in `registry.json` (schema-checked).
3. Run `python3 scripts/validate_skills.py` — CI (`validate-skills.yml`) runs the same
   script on every PR touching `skills/` and blocks on registry drift, frontmatter
   violations, or broken relative links.
