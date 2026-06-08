---
name: glyphtrail-atlas
glyphtrail-version: 1
description: >-
  Use when working with the glyphtrail atlas: the personal, cross-repo commit
  history graph and its embeddings (semantic repo/commit similarity). Covers
  syncing, embedding (local or OpenAI), similarity search, and - importantly -
  how the (paid) embeddings are persisted, survive upgrades, and are backed up to
  Parquet for recovery or external analysis.
---

# Glyphtrail atlas: cross-repo history + embeddings

The atlas is an opt-in, **personal** store (separate from per-repo indexes) that
ingests the commit history of your registered repos and lets you search it
semantically. It lives under `~/.glyphtrail/atlas/` (the LadybugDB database is the
single file `~/.glyphtrail/atlas/ladybug`). Nothing here touches a repo's working
tree; it reads git history only.

## Identity (`[me]`) and interruption

`atlas sync` keeps only **your** commits by default (`--everyone` for all authors).
Configure who you are in `~/.glyphtrail/atlas/atlas.toml` (created on first use;
never overwritten). A read-only `atlas.toml.example` next to it always mirrors the
current built-in template, so every option stays documented:

```toml
[me]
emails   = ["you@example.com", "you@work.com"]      # exact addresses
domains  = ["example.com"]                           # any address @ a domain you own
patterns = ["you+*@gmail.com", "*@*.example.com"]    # globs (* / ?) over the whole address
```

An address is yours if it matches any `emails` entry, an owned `domain`, or a
`patterns` glob — all case-insensitive. Exact `emails` matching is provider-aware:
for providers whose sub-addressing is documented, a `+tag` is folded away
(`you+ci@gmail.com` = `you@gmail.com`), and on Gmail dots are too
(`m.mayer@gmail.com` = `mmayer@gmail.com`), so you needn't list every alias. Other
domains are matched verbatim. Unset, it falls back to `git config user.email`.

Each repo's visibility (`public` / `private` / `proprietary`) is auto-detected
from the forge's **actual** repo status: at `repo add` the forge API (a token in
`~/.glyphtrail/forge.toml`, or the `gh` CLI for GitHub) is queried for the repo's
private/public flag — so a private repo on a public host is correctly `private`,
not leaked as `public`. When no forge confirms the status it falls back to a
host-based guess. `repo refresh` re-syncs visibility from the live forge status:
it corrects a stale tier in either direction (a now-private repo to `private`, a
confirmed-public one to `public`), but never relabels on an unconfirmed guess and
never overrides a hand-set `proprietary`.

Override the auto-detected tier by name / forge-org with `[repos]` globs (matched
against the repo name and each forge id `host/owner/repo`) so a whole organization
is treated as work without tagging each repo:

```toml
[repos]
proprietary = ["*acme*", "*/acme-corp/*"]   # hidden from public/default views
private     = ["*-internal"]
public      = ["*/acme-corp/oss-*"]          # allowlist — wins over the above
```

`proprietary`/`private` only *raise* restrictiveness; `public` is an allowlist that
**wins**, keeping a public repo public even when a broad work-org glob would catch
it. Restricted repos are gated out of `story`/`export` and the default
`similar`/`viz` output (`--include-restricted` to include them). `CTRL-C` during any operation stops gracefully at a safe point so the
database stays consistent (a second `CTRL-C` aborts); an interrupted `sync` saves
the repos it finished and resumes on the next run.

Write commands (`sync`, every `embed` except `export`, `waka sync`, `init`) take an
exclusive `atlas.lock` for their duration, so a second write fails fast — `another
glyphtrail process is using the atlas (pid …)` — instead of colliding on the
single-file database. Read commands aren't blocked. If a crash leaves a stale lock,
clear it with `glyphtrail atlas unlock`.

## Command surface

```bash
glyphtrail atlas init                 # create the store (idempotent)
glyphtrail atlas sync [--everyone]    # ingest git history (mine-only by default)
glyphtrail atlas status               # store + embedding state
glyphtrail atlas timeline | topics | story   # browse / narrate the history

# Embeddings (namespaced by space + model; models never mix in search)
glyphtrail atlas embed repos    [--provider local|openai --model … --base-url …]
glyphtrail atlas embed graph                  # structural embedding per repo
glyphtrail atlas embed commits  [--provider … --model …]   # one vector per commit
glyphtrail atlas similar repos   <repo|text> [--graph] [--model …]
glyphtrail atlas similar commits <text>       [--model …]
glyphtrail atlas digest          [repo] [--json]   # structured repo digest (#338)
glyphtrail atlas viz   [--graph --neighbors N -o map.html]  # repo-similarity map → HTML
glyphtrail atlas serve [--graph --neighbors N --port P]     # serve the similarity map

# Backup / portability (under `embed`)
glyphtrail atlas embed export --space <s> --model <m> [--out f.jsonl]
glyphtrail atlas embed import [f.jsonl]
glyphtrail atlas embed restore                # rebuild from the Parquet backup

# Time tracking (WakaTime, #486) — off-machine, opt-in
glyphtrail atlas waka sync [--since YYYY-MM-DD --until YYYY-MM-DD]   # default: last 7d
glyphtrail atlas waka show [--since --until --limit N --json|--yaml]
```

The `embed`, `similar`, and `waka` commands are grouped: `embed repos|graph|
commits|export|import|restore`, `similar repos|commits`, `waka sync|show`.
Keys (OpenAI / WakaTime) come from the environment; a `.env` in the working
directory (or an ancestor) is auto-loaded.

`atlas viz` renders a **repo-similarity map**: each repo is a node, each edge links
a repo to its most embedding-similar repos, laid out as a force graph so clusters of
related projects emerge (`--graph` for structural similarity, `--neighbors N` for
edge density). `atlas viz` writes a self-contained HTML file; `atlas serve` serves
the same map over HTTP. Both reuse the `glyphtrail viz`/`serve` renderer.

The `text` embedding represents each repo by a **structured digest** (languages,
dependencies, API/endpoint surface, structure counts, git timeline, README excerpt,
and frequency-ranked topics) built from its own index + commit history — not raw
commit subjects — so repo similarity matches what a repo *is*. `atlas digest [repo]`
prints that digest (`--json` for structured); it's the same document `embed` uses.

An **embedding namespace** is a `(space, model)` pair. `space` is one of `text`
(repo digest), `graph` (structural, from the code graph),
or `commit` (one vector per commit). `model` is the embedder id, e.g.
`lexical-hash-v1` (local, no network), `graph-struct-v1`, or
`openai:text-embedding-3-small`. Vectors for different models coexist and are
never compared against each other.

When several models are stored, `similar` / `viz` / `serve` default to the
**quality** (neural) model over the local `lexical-hash` bootstrap — so a
convenient local `embed` never silently demotes your paid vectors — and print
which model they used. Pass `--model <id>` to force one (e.g. `--model
lexical-hash-v1`); `atlas status` lists what's stored.

## Time tracking (WakaTime)

`atlas waka sync` pulls WakaTime daily summaries (effort + environment — the axis
git can't show: coding seconds, time by language, editor/IDE, OS, machine/device,
category) into a `WakaStat(date, dimension, name, seconds)` side table, and `atlas
waka show` reports them: effort per repo, plus language / editor / device / OS /
category breakdowns (`--json`/`--yaml` for agents). `atlas status` shows a summary
line when data is present.

- **Off-machine, opt-in.** The fetch hits the WakaTime cloud API; the request is
  announced before it's sent. The key is read from `WAKATIME_API_KEY` and never
  stored. Override the endpoint (e.g. a self-hosted Wakapi) via `[waka].base_url`
  in `atlas.toml`.
- **Repo join.** A WakaTime `project` maps to its registry repo by name; add
  `[waka].projects = { "<waka project>" = "<repo name>" }` for mismatches.
- **`atlas story` weaves it in.** When WakaTime data covers the story window, the
  narration prompt gains a time-tracking block (total, effort per repo, languages,
  editors, machines) so the narrative reflects effort + environment. Per-repo
  effort is gated to the visible repo set, so a public-only story never names a
  hidden/proprietary project; the aggregate breakdowns carry no repo names.
- Like embeddings, `WakaStat` is **fetched data preserved across glyphtrail
  upgrades** (it is not dropped by a code-graph schema migration). Not embedded —
  the data is quantitative, not semantic.

## Embedding durability — read this before paying for OpenAI embeddings

Embeddings can be expensive (one paid API call per document). They are protected
three ways so you don't recompute them:

1. **In-database, across upgrades.** Embeddings live in the atlas database but are
   versioned independently of the code-graph schema (`EMBEDDING_SCHEMA_VERSION`).
   A glyphtrail upgrade that changes the code-graph schema rebuilds only the
   code-graph tables and **preserves** the embeddings, their catalog, and the
   active-model pointers. They are only ever discarded if the embedding storage
   format itself changes (a deliberate version bump).

2. **Automatic Parquet backup on disk.** Every `atlas embed repos`, `embed graph`,
   `embed commits`, and `embed import` mirrors the vectors to a portable Parquet
   backup beside the database:

   ```
   ~/.glyphtrail/atlas/ladybug-embeddings-backup/
     manifest.json
     Vec_<space>_<modelslug>_<hash>.parquet   # one per namespace
   ```

   The backup is a **sibling directory** of the database file (LadybugDB stores the
   DB as a single file, not a directory). It is refreshed to match the live data on
   every embed, removed when no embeddings remain, and written best-effort (a
   backup failure never fails an embed).

   **Recovery is automatic on `sync`.** `atlas sync` recreates the database if it's
   gone, rebuilds the commit/repo graph, and then — when the live embeddings are
   empty but a backup is present — **restores the paid-for vectors from the Parquet
   backup itself** (rebuilding the HNSW indexes). So a single command recovers from
   a wiped store:

   ```bash
   glyphtrail atlas sync --full
   #   …
   #   total:   <n> commits ingested
   #   restored 2 embedding namespaces from the Parquet backup
   ```

   The node ids are deterministic (derived from repo name + commit hash), so the
   re-synced commits line back up with the restored vectors — **no re-embedding, no
   API spend**. Auto-restore never overwrites a live embedding set (it only fires
   when the catalog is empty), and an ordinary glyphtrail upgrade never loses the
   graph or embeddings in the first place.

   `glyphtrail atlas embed restore` does the same restore on demand (e.g.
   without re-syncing, or to force a restore over the current set), and works even
   if the database file itself is gone, as long as the `-embeddings-backup/` sibling
   survives.

3. **Explicit JSONL export.** `atlas embed export` / `embed import` is the
   human-inspectable, per-namespace path (e.g. to move one model between machines).

**Portability gotcha:** if you relocate or clean `~/.glyphtrail/atlas`, move the
`ladybug-embeddings-backup/` sibling directory too — it is not inside the DB file.

## Parquet backup format (for external tools)

The backup directory is self-describing, so pandas / pyarrow / DuckDB / Polars can
read the vectors directly without glyphtrail.

### `manifest.json`

```json
{
  "embedding_schema_version": "1",
  "namespaces": [
    {
      "space": "text",
      "model": "openai:text-embedding-3-small",
      "dim": 1536,
      "file": "Vec_text_openaitextembedding3small_1a2b3c4d5e6f7890.parquet",
      "active": true,
      "base_url": ""
    }
  ]
}
```

- `space` / `model` — the namespace identity.
- `dim` — vector width (every row in the file has exactly this length).
- `file` — the Parquet file (relative to the backup dir). Use the manifest to map a
  file to its `(space, model, dim)`; do not parse the table name.
- `active` — whether this model is the default for its space.
- `base_url` — the provider endpoint when set (empty for the default OpenAI / local).

### The `*.parquet` files

Standard Apache Parquet (`PAR1`), one row per embedded node, three columns:

| column    | Parquet type                        | meaning |
|-----------|-------------------------------------|---------|
| `node_id` | `BYTE_ARRAY` (UTF-8 string)         | glyphtrail's internal id (see below) — used by restore |
| `key`     | `BYTE_ARRAY` (UTF-8 string)         | the **human identifier**: the commit hash for the `commit` space, the repo name for `text`/`graph` (empty if unresolved) |
| `vec`     | 3-level `LIST` of `FLOAT` (float32) | the embedding vector, length = the namespace `dim` |

`vec` uses the standard Parquet LIST encoding (`vec` → `list` → `element: float`),
so it decodes as a list/array column of float32 in any reader.

**Use `key`, not `node_id`, to identify a row.** `node_id` is a stable, one-way
hash (`blake3(parts joined by \0)[:16]`) and is **not reversible**: `text` ids are
`derive(["repo", name])`, `graph` ids are `derive(["repo_graph", name])`, and
`commit` ids are `derive(["commit", repo_name, commit_hash])`. `key` is the
plaintext you actually have — so to find the vector for commit `abc123…`, filter
the `commit` parquet on `key == "abc123…"`. (`node_id` exists only so glyphtrail can
re-key the vectors to its own graph on restore.)

### Reading example

```python
import json, pyarrow.parquet as pq, numpy as np
from pathlib import Path

backup = Path.home() / ".glyphtrail/atlas/ladybug-embeddings-backup"
manifest = json.loads((backup / "manifest.json").read_text())
for ns in manifest["namespaces"]:
    t = pq.read_table(backup / ns["file"])
    keys = t.column("key").to_pylist()                                 # commit hash / repo name
    vecs = np.array(t.column("vec").to_pylist(), dtype=np.float32)      # (n, dim)
    print(ns["space"], ns["model"], vecs.shape)

# vector for a specific commit, no glyphtrail needed:
# commit = pq.read_table(backup / "<commit file>").to_pandas()
# v = commit.loc[commit["key"] == "abc123…", "vec"].iloc[0]
```

```sql
-- DuckDB: map a commit hash to its vector
SELECT key, vec
FROM read_parquet('~/.glyphtrail/atlas/ladybug-embeddings-backup/Vec_commit_*.parquet')
WHERE key = 'abc123…';
```
