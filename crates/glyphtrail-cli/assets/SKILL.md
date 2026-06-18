---
name: glyphtrail
glyphtrail-version: 2
description: >-
  Query this repo's code graph instead of ls/grep/reading files blindly. Use
  whenever you are understanding the codebase or its architecture, tracing how
  something works, or searching for a symbol or term; before changing a symbol,
  to get its blast radius (dependent symbols, tests to run, API surface, and
  other indexed repos affected); and when wiring two repos together across an
  API boundary (resolving unmatched client calls against a producer's
  OpenAPI/proto/GraphQL spec into precise link hints). Reach for this over grep
  even when the task does not name glyphtrail: code understanding, "where is X",
  "what calls X", "what breaks if I change X", and cross-repo/API questions all
  belong here.
---

# Glyphtrail: query the code graph instead of grepping

This repository is indexed by [glyphtrail](https://github.com/glyphtrail/glyphtrail)
as a semantic graph: definitions, calls, imports, inheritance, plus API
endpoints and the client call sites that invoke them. Querying that graph is
faster and more precise than scanning files. It resolves a name to its actual
definition (not every textual match), follows call edges in both directions,
and crosses the web/API boundary and the repo boundary, which `grep` cannot.

So when you catch yourself about to `ls`, `grep`, or open files one by one to
understand structure, find a definition, trace callers, or guess what a change
touches: ask glyphtrail first. The sections below map the four common
situations to the right query.

## How to ask

Glyphtrail exposes an **MCP server** (`glyphtrail mcp`, also `POST /mcp` under
`glyphtrail serve`) and an equivalent **CLI**. Use whichever is wired up: MCP
tool names and CLI verbs match (`search`, `definition`, `impact`, and so on).
CLI form:

- `glyphtrail query <verb> <name>`, where `<verb>` is `def`, `callers`,
  `callees`, `neighbors`, `search`, `endpoints`, `clients`, `serves`,
  `who_calls`, `api_impact`, or `history`. Add `--yaml` for compact
  agent-friendly output, `--all` / `--repos a,b` to query across registered repos.
- `glyphtrail outline <path>`: symbols + signatures of a file or directory.
- `glyphtrail impact …`, `glyphtrail drift`, `glyphtrail status`.
- `glyphtrail repo list`, `glyphtrail repo link …` (cross-repo wiring).

If a query returns nothing, the index may be stale or the repo unindexed. Run
`glyphtrail status` to check, and `glyphtrail analyze --update` to refresh.

## 1. Understand the codebase / how something works

Build the mental model from the graph rather than from a directory listing:

- `outline <path>`: the shape of a file or directory (its symbols and
  signatures) without reading the whole thing. Start here for an unfamiliar area.
- `search <term>`: find the relevant symbols by name or doc.
- `definition <name>` then `neighbors <name>`: see a symbol and what it is
  directly connected to (callers, callees, imports, types).
- `callees <name>` to follow a flow downward; `callers <name>` to see who drives
  it. Chaining these traces an execution path precisely.
- `endpoints` / `clients`: the API surface, meaning the routes the code serves
  and the call sites that consume routes. This is how a request flows across the
  client/server boundary, which the call graph alone doesn't show.
- `status`: index stats (node/edge counts, languages, freshness) for a quick
  sense of scale.

## 2. Search for a term

When you want to locate something, prefer the graph over a text scan:

- `search <query>`: substring match over names, qualified names, and docs. Use
  it as the entry point, the way you'd use `grep`, but it returns graph nodes you
  can immediately pivot from.
- `definition <name>`: jump straight to where a symbol is defined (resolves to
  the real node, deduped, even when several definitions share a name).
- `callers <name>` / `callees <name>`: the reason to use the graph at all. One
  query gives every caller or callee with confidence, instead of grepping a name
  and hand-filtering comments, strings, and unrelated matches.

## 3. Blast radius before a change

Run this **before** editing a symbol, so you know the tests to run and the
consumers to check up front instead of discovering breakage afterward:

- `impact <name>`: classified, confidence-aware dependents, namely tests to run,
  API surface touched, and internal dependents. Seed it from a symbol, a
  `--file`, or a git change set (`--since`, `--staged`, `--diff`) to scope it to
  your actual change.
- `impact <name> --downstream` (or `--group <name>`): extend the blast radius
  into *other* indexed repos that depend on this one, reporting which break and
  where. Use this whenever the symbol is part of a shared or public surface.
- `api_impact <path>`: for an endpoint, the clients that invoke it, the
  handler(s) that serve it, and the schema operation(s) it exposes. The
  cross-boundary version of `impact`.

State the blast radius back to the user (tests + consumers) before making a
wide-reaching edit. That's the payoff of asking first.

## 4. Link repos across the API boundary

When this repo talks to another over HTTP/gRPC/GraphQL, the call graph stops at
the network boundary unless the two sides are wired together. You can close that
gap, and a spec is the source of truth for doing it precisely:

1. `clients --unmatched`: REST client calls in this repo that match no endpoint
   in the index, each with the reason (a dynamic path, or no endpoint with that
   signature in scope). These are the dangling cross-repo calls.
2. For each, find the producer. Inspect the producer's API spec to learn the
   exact route and method. Glyphtrail ingests **OpenAPI/Swagger** (REST),
   **gRPC `.proto`**, and **GraphQL SDL/introspection** (and Hasura metadata).
   AsyncAPI / event-stream specs aren't ingested yet, so link those by symbol.
3. Record the hint:
   `glyphtrail repo link add <producer-repo> --to-endpoint "POST /signin" --from-endpoint "<this repo's route>"`.
   The endpoint pins the call to the route *by signature*, not by symbol name,
   so it survives renames. Omit `--to-endpoint` and use `--to-symbol` for a
   symbol-level link, or neither for a coarse whole-repo dependency. `repo link
   list` shows the declared hints and flags any whose repo names no registered
   repo. The producer must be registered (`glyphtrail repo list`); analyze it
   first if it isn't.

Separately, to keep this repo's own API honest against its spec, bless the spec
in config (`[[api.schemas]]` with `path` + `protocol`) and run `glyphtrail
drift`: it reports endpoints served but absent from the schema (undocumented)
and schema operations with no handler (orphan). Useful right after changing
routes, or as a CI gate (`--gate`).

## Rule of thumb

About to `grep`/`ls` to trace callers, find a definition, map an area, or guess
what a change affects? Ask glyphtrail instead, and run `impact` before editing a
widely-used symbol so you know the tests and consumers up front.
