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
