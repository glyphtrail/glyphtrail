# codegraph

Turn a repository into a queryable **code knowledge graph**. `codegraph` parses
source with [tree-sitter](https://tree-sitter.github.io/), extracts symbols,
calls, imports, inheritance and design-rationale comments, stores them in a
per-repo SQLite graph, and lets you query or visualize the result.

Inspired by [graphify](https://github.com/safishams/zesty-kahn) and similar
tools — built native in Rust, fast, and dependency-light.

## Install

```sh
cargo build --release
# binary at target/release/codegraph
```

## Usage

```sh
# Index the current repository (writes .codegraph/graph.db)
codegraph analyze .

# Re-index only files that changed
codegraph analyze . --update

# Query the graph
codegraph query def <name>            # locate a definition
codegraph query callers <name>        # who calls it
codegraph query callees <name>        # what it calls
codegraph query neighbors <name>      # direct graph neighbours
codegraph query search <text>         # full-text search (names + doc comments)
codegraph query impact <name>         # transitive blast radius if it changes
#   add --json for machine-readable output

# Visualize
codegraph viz --output graph.html     # self-contained interactive page
codegraph serve --port 7700           # live explorer at http://127.0.0.1:7700

# Stats
codegraph status
```

## Languages

Coverage is driven by a tree-sitter grammar registry. Currently:
Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++. Adding a language is
a grammar in `codegraph-parse/src/registry.rs` plus a query file under
`codegraph-parse/queries/`.

## Graph model

- **Nodes:** repo, file, module, function, method, class/struct/interface/enum/
  trait, and design-rationale comments (`NOTE`/`WHY`/`HACK`/`TODO`/`FIXME`).
- **Edges:** `contains`, `calls`, `imports`, `extends`, `implements`,
  `documents`. Each edge is tagged `extracted` (straight from the AST) or
  `inferred` (resolved heuristically across files).

## Architecture

A Cargo workspace:

| Crate | Responsibility |
|-------|----------------|
| `codegraph-core`   | domain model, language detection, config |
| `codegraph-parse`  | tree-sitter registry, extraction, graph building |
| `codegraph-store`  | SQLite + FTS5 storage and graph queries |
| `codegraph-viz`    | Cytoscape graph rendering (HTML/JSON) |
| `codegraph-server` | `axum` server for the interactive explorer |
| `codegraph-cli`    | the `codegraph` binary |

Storage is SQLite-first behind a store layer so a LadybugDB (Cypher) backend can
be added later. An MCP server and multi-repo support are planned.

## License

MIT OR Apache-2.0.
