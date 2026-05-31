# Stratograph

<p align="center">
  <img src="https://raw.githubusercontent.com/sunsided/stratograph/main/.readme/hero.jpg" alt="Stratograph" />
</p>

[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

Stratograph maps codebases as **semantic and historical graphs**, so you can query
structure, trace lineage, and discover recurring ideas across time.

It parses source with [tree-sitter](https://tree-sitter.github.io/), extracts
symbols, calls, imports, inheritance, design-rationale comments, and
cross-boundary API links, stores them in a per-repo LadybugDB graph, and lets
you query or visualize the result.

Built native in Rust.

## Install

Prebuilt binaries for Linux, macOS (Intel + Apple Silicon) and Windows are
attached to each [tagged release](https://github.com/sunsided/stratograph/releases).

Or build from source:

```sh
cargo install --git https://github.com/sunsided/stratograph stratograph-cli
# or, from a checkout:
cargo build --release   # binary at target/release/stratograph
```

## Usage

```sh
# Index the current repository (writes .stratograph/ladybug)
stratograph analyze .

# Re-index only files that changed
stratograph analyze . --update

# Query the graph
stratograph query def <name>            # locate a definition
stratograph query callers <name>        # who calls it
stratograph query callees <name>        # what it calls
stratograph query neighbors <name>      # direct graph neighbours
stratograph query search <text>         # full-text search (names + doc comments)
stratograph query impact <name>         # transitive blast radius if it changes
#   add --json (or --yaml, compact for agents) for machine-readable output

# Impact analysis (blast radius from a symbol, file, or change set)
stratograph impact <name>                       # seed: a symbol
stratograph impact --file src/api.rs            # seed: every symbol in a file
stratograph impact --since main..HEAD           # seed: changed symbols vs a git range
stratograph impact --staged | --diff            # seed: staged / working-tree changes
#   [--cross-boundary] reach API consumers (HANDLES/INVOKES/EXPOSES/MOUNTS)
#   [--edges calls,imports,impl,api] [--depth N] [--min-confidence extracted|inferred]
#   [--format text|json|md]   [--gate]  exit 2 when the change touches the API surface

# Visualize
stratograph viz --output graph.html     # self-contained interactive page
stratograph serve --port 7700           # live explorer at http://127.0.0.1:7700

# Agent integration (Model Context Protocol)
stratograph mcp                         # MCP server over stdio (query/endpoints/impact/…)
#   `stratograph serve` also exposes the same tools at POST /mcp (JSON-RPC)

# Generate a docs wiki from the graph via an LLM
stratograph wiki --provider claude        # or openai / openrouter (reads *_API_KEY)
stratograph wiki --dry-run                # write the prompts only (no network/keys)
#   --base-url lets an OpenAI-compatible gateway (e.g. Kilo) stand in

# Stats
stratograph status
```

### Excluding sensitive files

`analyze` honors `.gitignore`/`.git/info/exclude`, skips dotfiles, and reads
exclusion lists from `.stratographignore`, `.aiignore`, `.aiexclude`, and
`.claudeignore`. List any file with secrets/key material there to keep it out of
the index entirely — and therefore out of every agent-facing surface (wiki, MCP).

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
      - run: cargo install --path crates/stratograph-cli   # or download a release binary
      - run: stratograph analyze .
      - name: Impact report
        run: |
          stratograph impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --format md >> "$GITHUB_STEP_SUMMARY"
      # Optional: fail the job if the PR changes the API/contract surface.
      - name: Drift gate
        run: |
          stratograph impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --gate
```

### Cross-repo blast radius

Register repositories once, then trace a change's impact *across* them — when you
change a crate that other locally-indexed repos depend on (via crates.io, git,
or a path dependency), `impact --downstream` reports which of those repos break
and where, not just your own.

```bash
stratograph repo add .                  # register the current repo
stratograph repo list                   # registered repos + health + forge ids
stratograph group add svc api core      # optional: a named subset to scope to

stratograph analyze .                   # index each repo as usual

# Blast radius extended into downstream repos that depend on this one:
stratograph impact MySymbol --downstream            # federate over the registry
stratograph impact --since main..HEAD --group svc   # scope to a group
```

Cross-repo links are matched by package name (Cargo today): a consumer's
dependency is tied to the producer repo whose crate publishes it. The MCP
`impact` tool takes the same `downstream`/`group` arguments, and `list_repos`
enumerates the registry, so an agent gets the cross-repo blast radius in one
call.

### Repository identity

Each registered repo gets stable **forge identities** derived from its git
remotes, so it is recognised across folder renames, multiple clones, and name
collisions with published crates — and a repo's mirrors (e.g. a GitHub origin
plus a Codeberg mirror) all resolve to the same repo. Two kinds, recorded at
`repo add`:

- **Slug** (always, offline): `host/owner/repo` from each remote → a UUIDv5.
- **Numeric** (optional, rename-proof): the forge's numeric repo id via its API,
  which survives a rename *on the forge*. Resolved only when a token is
  available, per forge:

  | Forge | Host | Token env var |
  | --- | --- | --- |
  | GitHub | `github.com` | `GITHUB_TOKEN` (else falls back to the `gh` CLI) |
  | GitLab | `gitlab.com` | `GITLAB_TOKEN` |
  | Gitea / Forgejo | `codeberg.org` | `CODEBERG_TOKEN` |

Numeric ids are entirely opt-in: with no token (and no `gh`), only the slug ids
are recorded. Tokens are read from the environment and never logged.

## Languages

Coverage is driven by a tree-sitter grammar registry. Built in:
Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++, C#, Ruby, Kotlin. Adding a built-in
language is a grammar in `stratograph-parse/src/registry.rs` plus a query file under
`stratograph-parse/queries/`.

Extra languages can also be loaded at runtime without rebuilding — point
`.stratograph/config.toml` at a tree-sitter grammar and a query (the grammar is
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
| `stratograph-core`   | domain model, language detection, config |
| `stratograph-parse`  | tree-sitter registry, extraction, graph building |
| `stratograph-store`  | LadybugDB (Cypher) storage and graph queries |
| `stratograph-viz`    | Cytoscape graph rendering (HTML/JSON) |
| `stratograph-server` | `axum` server for the interactive explorer |
| `stratograph-mcp`    | Model Context Protocol server (stdio) exposing the query tools |
| `stratograph-cli`    | the `stratograph` binary |

Storage sits behind the `GraphStore` trait. **LadybugDB** (Cypher, native graph
traversal) is the storage backend; the `.stratograph/ladybug` index is the source
of truth.

```sh
stratograph analyze .                    # writes .stratograph/ladybug
stratograph cypher "MATCH (n:Node) RETURN n.name LIMIT 10"   # raw Cypher
```

LadybugDB links the `lbug` crate, which downloads a prebuilt `liblbug` or builds
from source via **cmake + a C/C++ toolchain** (clang/gcc) — install those if the
prebuilt archive is unavailable (e.g. `apt-get install cmake clang
build-essential`).

## Development

### Prerequisites

- Rust toolchain: install from [rustup.rs](https://rustup.rs/)
- [`task`](https://taskfile.dev/#/installation) - task runner (`Taskfile.dist.yaml`)
- `prek` - fast Rust-native pre-commit hook runner (`cargo install prek`)

### Setup

```sh
# Clone and build
git clone https://github.com/sunsided/stratograph
cd stratograph
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
