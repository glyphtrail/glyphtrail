# Meridian

<p align="center">
  <img src="https://raw.githubusercontent.com/sunsided/meridian/main/.readme/hero.jpg" alt="Meridian" />
</p>

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

# Impact analysis (blast radius from a symbol, file, or change set)
meridian impact <name>                       # seed: a symbol
meridian impact --file src/api.rs            # seed: every symbol in a file
meridian impact --since main..HEAD           # seed: changed symbols vs a git range
meridian impact --staged | --diff            # seed: staged / working-tree changes
#   [--cross-boundary] reach API consumers (HANDLES/INVOKES/EXPOSES/MOUNTS)
#   [--edges calls,imports,impl,api] [--depth N] [--min-confidence extracted|inferred]
#   [--format text|json|md]   [--gate]  exit 2 when the change touches the API surface

# Visualize
meridian viz --output graph.html     # self-contained interactive page
meridian serve --port 7700           # live explorer at http://127.0.0.1:7700

# Agent integration (Model Context Protocol)
meridian mcp                         # MCP server over stdio (query/endpoints/impact/…)
#   `meridian serve` also exposes the same tools at POST /mcp (JSON-RPC)

# Stats
meridian status
```

### Impact reports in CI

Seed the impact analysis from a pull request's diff and post a Markdown summary,
optionally failing the job when the change touches the public API / schema
surface (drift gate):

```yaml
# .github/workflows/impact.yml
name: impact
on: pull_request
jobs:
  impact:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0           # need the base ref for the diff
      - run: cargo install --path crates/meridian-cli   # or download a release binary
      - run: meridian analyze .
      - name: Impact report
        run: |
          meridian impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --format md >> "$GITHUB_STEP_SUMMARY"
      # Optional: fail the job if the PR changes the API/contract surface.
      - name: Drift gate
        run: |
          meridian impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --gate
```

## Languages

Coverage is driven by a tree-sitter grammar registry. Built in:
Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++, C#, Ruby. Adding a built-in
language is a grammar in `meridian-parse/src/registry.rs` plus a query file under
`meridian-parse/queries/`.

Extra languages can also be loaded at runtime without rebuilding — point
`.meridian/config.toml` at a tree-sitter grammar and a query (the grammar is
compiled on demand; needs a C toolchain):

```toml
[[languages]]
name = "ruby"
extensions = ["rb"]
grammar = "grammars/tree-sitter-ruby/src"   # dir with parser.c + grammar.json
query = "queries/ruby.scm"                   # @def.<kind>/@call/@import/…
```

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
