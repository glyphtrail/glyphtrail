# Glyphtrail

<p align="center">
  <img src="https://raw.githubusercontent.com/glyphtrail/glyphtrail/refs/heads/main/.readme/hero.jpg" alt="Glyphtrail" />
</p>

[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

Glyphtrail maps codebases as **semantic and historical graphs**, so you can query
structure, trace lineage, and discover recurring ideas across time.

It parses source with [tree-sitter](https://tree-sitter.github.io/), extracts
symbols, calls, imports, inheritance, design-rationale comments, and
cross-boundary API links, stores them in a per-repo LadybugDB graph, and lets
you query or visualize the result.

Built native in Rust.

## Install

Prebuilt binaries for Linux, macOS (Intel + Apple Silicon) and Windows are
attached to each [tagged release](https://github.com/glyphtrail/glyphtrail/releases).

Or build from source:

```sh
cargo install --git https://github.com/glyphtrail/glyphtrail glyphtrail-cli
# or, from a checkout:
cargo build --release   # binary at target/release/glyphtrail
```

## Usage

```sh
# Index the current repository (writes .glyphtrail/ladybug)
glyphtrail analyze .

# Re-index only files that changed
glyphtrail analyze . --update

# Outline the shape of a file or directory (symbols + signatures)
glyphtrail outline src/api.rs              # standard detail
glyphtrail outline src --detail minimal    # names only, whole directory

# Query the graph
glyphtrail query def <name>            # locate a definition
glyphtrail query callers <name>        # who calls it
glyphtrail query callees <name>        # what it calls
glyphtrail query neighbors <name>      # direct graph neighbours
glyphtrail query search <text>         # full-text search (names + doc comments)
glyphtrail query impact <name>         # transitive blast radius if it changes
#   add --json (or --yaml, compact for agents) for machine-readable output

# Impact analysis (blast radius from a symbol, file, or change set)
glyphtrail impact <name>                       # seed: a symbol
glyphtrail impact --file src/api.rs            # seed: every symbol in a file
glyphtrail impact --since main..HEAD           # seed: changed symbols vs a git range
glyphtrail impact --staged | --diff            # seed: staged / working-tree changes
#   reports a risk level (LOW/MEDIUM/HIGH/CRITICAL) and, for a large blast radius,
#   a capped summary — add --details (full list) or --limit N
#   [--cross-boundary] reach API consumers (HANDLES/INVOKES/EXPOSES/MOUNTS)
#   [--edges calls,refs,imports,impl,api] [--depth N] [--min-confidence extracted|inferred]
#   [--format text|json|md]   [--gate]  exit 2 when the change touches the API surface

# Visualize
glyphtrail viz --output graph.html     # self-contained interactive page
glyphtrail serve --port 7700           # live explorer at http://127.0.0.1:7700

# Agent integration (Model Context Protocol)
glyphtrail mcp                         # MCP server over stdio (query/endpoints/impact/…)
#   `glyphtrail serve` also exposes the same tools at POST /mcp (JSON-RPC)
glyphtrail setup                       # onboard agents: write .claude/skills + a
#   managed CLAUDE.md/AGENTS.md section pointing them at the MCP/CLI, and gitignore
#   the index. Idempotent and stats-free (won't dirty the files on every commit).

# Generate a docs wiki from the graph via an LLM
glyphtrail wiki --provider claude        # or openai / openrouter (reads *_API_KEY)
glyphtrail wiki --dry-run                # write the prompts only (no network/keys)
#   --base-url lets an OpenAI-compatible gateway (e.g. Kilo) stand in

# Stats
glyphtrail status
```

### Registering glyphtrail as an MCP server

`glyphtrail mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio, exposing the query / impact / outline / endpoint tools to an agent.
Index the repo first (`glyphtrail analyze`), then register the server. Each agent
wires up MCP servers differently:

**Claude Code** (CLI) — add it to the current project:

```sh
claude mcp add glyphtrail -- glyphtrail mcp --repo .
```

**Claude Desktop** — download the `glyphtrail-<version>-<target>.mcpb` bundle for
your platform from a [release](https://github.com/glyphtrail/glyphtrail/releases)
and open it (Settings → Extensions, or double-click). The bundle ships the
`glyphtrail` binary; nothing else to install. Because a Desktop extension is
global, it starts **without** a pinned repository: the agent names the target
`repo` (a registered name or path) on each tool call, and the `list_repos` tool
discovers what is indexed. To wire it up by hand instead, add to
`claude_desktop_config.json` (pass `--repo` to pin one repo, or omit it for the
same repo-per-call behavior as the bundle):

```json
{
  "mcpServers": {
    "glyphtrail": {
      "command": "glyphtrail",
      "args": ["mcp", "--repo", "/path/to/your/repo"]
    }
  }
}
```

**Cursor / Windsurf / VS Code** — point the client at the same stdio command. For
example, a project-scoped Cursor `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "glyphtrail": {
      "command": "glyphtrail",
      "args": ["mcp", "--repo", "."]
    }
  }
}
```

**OpenCode / Kilo Code** — these use a different shape and location: a single
`command` array (program + args together) under a `type: "local"` server, in
`~/.config/opencode/opencode.json` or `~/.config/kilo/kilo.jsonc` respectively:

```jsonc
{
  "mcp": {
    "glyphtrail": {
      "type": "local",
      "enabled": true,
      "command": ["glyphtrail", "mcp", "--repo", "."]
    }
  }
}
```

The exact key, format, and config location vary by client (some split
`command`/`args`, some take one array; some are project-scoped, some global) —
but the server is always the same: run `glyphtrail mcp --repo <path>` over stdio.
Omit `--repo` (as the Desktop bundle does) and the server pins no repository:
every tool call must then name a `repo`, and `list_repos` lists the indexed ones.
(`glyphtrail serve` additionally exposes the same tools over HTTP at `POST /mcp`,
for clients that prefer a URL.)

Prefer it managed? `glyphtrail setup` writes an agent skill plus a
`CLAUDE.md`/`AGENTS.md` section that points coding agents at these tools.

### Excluding sensitive files

`analyze` honors `.gitignore`/`.git/info/exclude`, skips dotfiles, and reads
exclusion lists from `.glyphtrailignore`, `.aiignore`, `.aiexclude`, and
`.claudeignore`. List any file with secrets/key material there to keep it out of
the index entirely — and therefore out of every agent-facing surface (wiki, MCP).

A **user-wide** ignore file at `~/.glyphtrailignore` applies to every repo you
analyze — handy when bulk-indexing a whole work directory. It does two things:

- **gitignore-format patterns** (e.g. `*.generated.rs`) exclude matching files in
  every repo's walk; a repo's own ignore files can re-include with `!`.
- **absolute (or `~/…`) path lines** exclude that whole repo/tree — analyzing it,
  or any subfolder of it, does nothing. Useful to skip a few giant repos while
  bulk-scanning a work directory.

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
      - run: cargo install --path crates/glyphtrail-cli   # or download a release binary
      - run: glyphtrail analyze .
      - name: Impact report
        run: |
          glyphtrail impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --format md >> "$GITHUB_STEP_SUMMARY"
      # Optional: fail the job if the PR changes the API/contract surface.
      - name: Drift gate
        run: |
          glyphtrail impact --since "origin/${{ github.base_ref }}...HEAD" \
            --cross-boundary --gate
```

### Cross-repo blast radius

Register repositories once, then trace a change's impact *across* them — when you
change a crate that other locally-indexed repos depend on (via crates.io, git,
or a path dependency), `impact --downstream` reports which of those repos break
and where, not just your own.

```bash
glyphtrail repo add .                  # register the current repo
glyphtrail repo scan ~/code            # register already-indexed repos under a tree
glyphtrail repo scan ~/code --analyze  # index each repo found, then register it
glyphtrail repo scan ~/code --all      # register every repo, indexed or not
glyphtrail repo list                   # registered repos + health + forge ids
glyphtrail repo list --mine            # only repos you've contributed to
glyphtrail group add svc api core      # optional: a named subset to scope to

glyphtrail analyze .                   # index each repo as usual

# Blast radius extended into downstream repos that depend on this one:
glyphtrail impact MySymbol --downstream            # federate over the registry
glyphtrail impact --since main..HEAD --group svc   # scope to a group
```

Cross-repo links are matched by package name (Cargo today): a consumer's
dependency is tied to the producer repo whose crate publishes it. The MCP
`impact` tool takes the same `downstream`/`group` arguments, and `list_repos`
enumerates the registry, so an agent gets the cross-repo blast radius in one
call.

#### How it works (many local databases, one registry)

There is no central database. Each repo owns its index at
`<repo>/.glyphtrail/ladybug`, built independently by `analyze`. The only shared
state is the user-wide registry at `~/.glyphtrail/registry.json`, which just
maps repo names to their roots (plus forge ids and groups) — it holds no graph
data.

A federated query (`impact --downstream` / `--group`, or the MCP `impact` tool)
stitches those independent indexes together at query time:

1. **Select the members** — the whole registry, or a named group. The current
   repo (matched to a registry entry by path) is always included.
2. **Open each member's index** read-only and pull its persisted *package
   identity*: the packages it publishes (with their public exports) and its
   *external uses* (imports that reference a declared dependency). Both were
   computed and stored during that repo's own `analyze`.
3. **Resolve cross-repo links** by matching a consumer's external use to the
   producer repo whose package name (and exported symbol) it references. This
   yields edges from a producer's export to the exact consumer symbols that use
   it (falling back to file-level, then crate-level, when a symbol can't be
   pinned down).
4. **Traverse** the normal impact engine over the union, where those cross-repo
   edges connect the per-repo graphs, and report the blast radius grouped by
   repo (origin first, then downstream).

Because linking is derived from package metadata, a repo only participates once
it has been analyzed (so its identity is on disk) and registered; a repo with no
recognized package/dependency relationship to the others simply contributes no
cross edges. Linking is Cargo-only today.

`repo scan` walks for version-control roots (`.git`/`.svn`/`.bzr`/`.hg`),
skipping dot-directories (`--hidden` to include them) and treating each repo as
a boundary (`--recursive` to also find nested repos / submodules). A submodule's
code is never indexed as part of its parent — analysis stops at nested-repo
boundaries — so it lands only in its own index. The walk and per-repo work show
progress, which matters on slow network-backed drives.

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

`glyphtrail repo refresh [name]` re-derives ids for registered repos from their
current git remotes, in place — handy when remotes change or to repair ids
written by an older version, without re-analyzing.

The same repo can live at more than one path — a symlink, a second clone, a
backup copy. Glyphtrail keeps **one entry per repo**: when a path you register
shares a forge id with an existing entry, it's folded in as an additional
location (shown as `also at …` in `repo list`) rather than a duplicate, so the
repo isn't double-counted in federated impact. Symlinks collapse automatically
via path canonicalization.

Each repo records its top git authors at registration, so `repo list --mine`
(or `--author <name-or-email>`) narrows a large registry to the repos you've
worked on without scanning each repo's history.

For tokens under a non-standard env var, or self-hosted Gitea/GitLab/Forgejo
instances the tool can't recognise by host, map them in
`~/.glyphtrail/forge.toml`:

```toml
[hosts."codeberg.org"]
token_env = "CODEBERG_READ_ONLY_PAT"   # use this var instead of CODEBERG_TOKEN

[hosts."git.example.com"]
kind = "gitea"                          # github | gitlab | gitea
token_env = "EXAMPLE_TOKEN"
```

### Concurrent writes and recovery

The registry (`~/.glyphtrail/registry.json`) and groups files are updated under
a portable lock file (`registry.lock`) that works on network / FUSE / sync
filesystems (e.g. pCloud), where OS advisory locks (`flock`) are unreliable and
can leave a *permanent* lock when a writer dies. The lock self-heals: a lock
held by a dead process on this host, or one older than a couple of minutes, is
stolen automatically on the next run, so a crashed `repo add` never wedges the
registry.

If the lock is genuinely busy (another `repo add` is mid-write), `repo add`
**doesn't fail or block** — it writes the entry to a spillover file
(`registry.spill.*.json`) beside the registry, and the next run that holds the
lock merges it in. Spillovers are written atomically (temp + rename), so a
reader never sees a half-written file. This makes bulk registration (indexing
many repos at once) loss-proof even on a slow or contended drive.

As a manual escape hatch, `glyphtrail repo unlock` force-releases the lock. It
is rarely needed given the self-healing above, and only removes the lock file.

## Languages

Coverage is driven by a tree-sitter grammar registry. Built in:
Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++, C#, Ruby, Kotlin. Adding a built-in
language is a grammar in `glyphtrail-parse/src/registry.rs` plus a query file under
`glyphtrail-parse/queries/`.

Extra languages can also be loaded at runtime without rebuilding — point
`.glyphtrail/config.toml` at a tree-sitter grammar and a query (the grammar is
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
| `glyphtrail-core`   | domain model, language detection, config |
| `glyphtrail-parse`  | tree-sitter registry, extraction, graph building |
| `glyphtrail-store`  | LadybugDB (Cypher) storage and graph queries |
| `glyphtrail-viz`    | Cytoscape graph rendering (HTML/JSON) |
| `glyphtrail-server` | `axum` server for the interactive explorer |
| `glyphtrail-mcp`    | Model Context Protocol server (stdio) exposing the query tools |
| `glyphtrail-cli`    | the `glyphtrail` binary |

Storage sits behind the `GraphStore` trait. **LadybugDB** (Cypher, native graph
traversal) is the storage backend; the `.glyphtrail/ladybug` index is the source
of truth.

```sh
glyphtrail analyze .                    # writes .glyphtrail/ladybug
glyphtrail cypher "MATCH (n:Node) RETURN n.name LIMIT 10"   # raw Cypher
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
git clone https://github.com/glyphtrail/glyphtrail
cd glyphtrail
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
