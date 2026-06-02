---
name: glyphtrail
description: >-
  Use when understanding or changing code in this repo: locating where a symbol
  is defined, what calls it, how a request flows across the API boundary, or the
  blast radius of a change (which symbols, tests, endpoints — and which other
  indexed repos — a change affects). Prefer this over ls/grep sweeps.
---

# Glyphtrail: query the code graph instead of grepping

This repository is indexed by [glyphtrail](https://github.com/glyphtrail/glyphtrail)
as a semantic graph (definitions, calls, imports, inheritance, API endpoints and
their clients). Query that graph for code understanding and impact analysis —
it's faster and more precise than scanning files, and it crosses the web and
package boundaries.

## How to ask

Glyphtrail exposes an MCP server (`glyphtrail mcp`, also `POST /mcp` under
`glyphtrail serve`) and a CLI. Use whichever is wired up.

**Locate / explore**
- `search <query>` — full-text over symbol names and docs.
- `definition <name>` / `callers <name>` / `callees <name>` / `neighbors <name>`.
- `endpoints` / `clients` / `serves <path>` / `who_calls <path>` / `api_impact <path>` — API surface and cross-boundary consumers.

**Blast radius (do this before changing a symbol)**
- `impact <name>` — classified, confidence-aware dependents: tests to run, API
  surface touched, internal dependents. Seed from a symbol, a `--file`, or a git
  change set (`--since`, `--staged`, `--diff`).
- `impact <name> --downstream` (or `--group <name>`) — extends the blast radius
  into *other* indexed repos that depend on this one, reporting which break and
  where.
- `list_repos` — the repos glyphtrail has indexed (with health + forge ids).

CLI equivalents: `glyphtrail query <verb> <name>`, `glyphtrail impact …`.

## Rule of thumb

Reaching for `grep`/`ls` to trace callers, find a definition, or guess what a
change affects? Ask glyphtrail instead — and run `impact` before editing a
widely-used symbol so you know the tests and consumers up front.
