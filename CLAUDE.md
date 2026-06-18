# glyphtrail

Local code intelligence graph for AI coding agents. See `README.md` for the
full overview. The managed section below is exactly what `glyphtrail setup`
installs into a consuming repo — this repo dogfoods its own onboarding, and the
bundled copies under `crates/glyphtrail-cli/assets/` are derived from here (run
`task assets:sync` after editing the skill or this section).

<!-- glyphtrail:begin v=2 (managed section — edits are overwritten) -->
# Code graph (glyphtrail)

This repo is indexed by [glyphtrail](https://github.com/glyphtrail/glyphtrail).
For code understanding and change-impact analysis, query the graph via the
glyphtrail MCP server (`glyphtrail mcp`) or CLI rather than `ls`/`grep`:

- understand / search: `outline`, `search`, `definition`, `callers`, `callees`,
  `neighbors`
- API flow: `endpoints`, `clients`, `who_calls`, `api_impact`
- **blast radius before a change**: `impact <symbol>` (add `--downstream` to
  reach other indexed repos that depend on this one)
- **wire two repos** across an API boundary: resolve `clients --unmatched`
  against a producer's OpenAPI/proto/GraphQL spec, then `repo link add`

See `.claude/skills/glyphtrail/SKILL.md` for details.
<!-- glyphtrail:end -->
