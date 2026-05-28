# Meridian

[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

Meridian maps codebases as **semantic and historical graphs**, so you can query
structure, trace lineage, and discover recurring ideas across time.

It parses source with [tree-sitter](https://tree-sitter.github.io/), extracts
symbols, calls, imports, inheritance, design-rationale comments, and
cross-boundary API links, stores them in a per-repo SQLite graph, and lets you
query or visualize the result.

Built native in Rust — fast and dependency-light.

## Install

Prebuilt binaries for Linux, macOS (Intel + Apple Silicon) and Windows are
attached to each [tagged release](https://github.com/sunsided/meridian/releases).

Or build from source:

```sh
cargo install --git https://github.com/sunsided/meridian meridian-cli
# or, from a checkout:
cargo build --release   # binary at target/release/meridian
```

## Usage

```sh
# Index the current repository (writes .meridian/graph.db)
meridian analyze .

# Re-index only files that changed
meridian analyze . --update

# Query the graph
meridian query def <name>            # locate a definition
meridian query callers <name>        # who calls it
meridian query callees <name>        # what it calls
meridian query neighbors <name>      # direct graph neighbours
meridian query search <text>         # full-text search (names + doc comments)
meridian query impact <name>         # transitive blast radius if it changes
#   add --json for machine-readable output

# Visualize
meridian viz --output graph.html     # self-contained interactive page
meridian serve --port 7700           # live explorer at http://127.0.0.1:7700

# Agent integration (Model Context Protocol)
meridian mcp                         # MCP server over stdio (query/endpoints/impact/…)
#   `meridian serve` also exposes the same tools at POST /mcp (JSON-RPC)

# Stats
meridian status
```

## Languages

Coverage is driven by a tree-sitter grammar registry. Currently:
Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++. Adding a language is
a grammar in `meridian-parse/src/registry.rs` plus a query file under
`meridian-parse/queries/`.

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
| `meridian-core`   | domain model, language detection, config |
| `meridian-parse`  | tree-sitter registry, extraction, graph building |
| `meridian-store`  | SQLite + FTS5 storage and graph queries |
| `meridian-viz`    | Cytoscape graph rendering (HTML/JSON) |
| `meridian-server` | `axum` server for the interactive explorer |
| `meridian-mcp`    | Model Context Protocol server (stdio) exposing the query tools |
| `meridian-cli`    | the `meridian` binary |

Storage is SQLite-first behind a store layer so a LadybugDB (Cypher) backend can
be added later. Multi-repo support is planned.

## Development

### Prerequisites

- Rust toolchain: install from [rustup.rs](https://rustup.rs/)
- [`task`](https://taskfile.dev/#/installation) - task runner (`Taskfile.dist.yaml`)
- `prek` - fast Rust-native pre-commit hook runner (`cargo install prek`)

### Setup

```sh
# Clone and build
git clone https://github.com/sunsided/meridian
cd meridian
cargo build --workspace

# Install pre-commit hooks (runs on every `git commit`)
prek install
```

### Checks

```sh
task fmt        # format in place
task lint       # clippy -D warnings
task test       # full test suite
task ci         # fmt:check + lint + test (mirrors CI)

# Run hooks manually against all files
prek run --all-files
```

The CI matrix runs against `stable` and the MSRV (`1.95`).

## License

[European Union Public Licence 1.2](LICENSE-EUPL-1.2)
