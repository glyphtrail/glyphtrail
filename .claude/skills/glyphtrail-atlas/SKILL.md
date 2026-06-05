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

## Command surface

```bash
glyphtrail atlas init                 # create the store (idempotent)
glyphtrail atlas sync [--everyone]    # ingest git history (mine-only by default)
glyphtrail atlas status               # store + embedding state
glyphtrail atlas timeline | topics | story   # browse / narrate the history

# Embeddings (namespaced by space + model; models never mix in search)
glyphtrail atlas embed          [--provider local|openai --model … --base-url …]
glyphtrail atlas graph-embed                  # structural embedding per repo
glyphtrail atlas embed-commits  [--provider … --model …]   # one vector per commit
glyphtrail atlas similar         <repo|text> [--graph] [--model …]
glyphtrail atlas similar-commits <text>       [--model …]

# Backup / portability
glyphtrail atlas embed-export --space <s> --model <m> [--out f.jsonl]
glyphtrail atlas embed-import [f.jsonl]
glyphtrail atlas embed-restore-backup         # rebuild from the Parquet backup
```

An **embedding namespace** is a `(space, model)` pair. `space` is one of `text`
(repo summary from commit subjects), `graph` (structural, from the code graph),
or `commit` (one vector per commit). `model` is the embedder id, e.g.
`lexical-hash-v1` (local, no network), `graph-struct-v1`, or
`openai:text-embedding-3-small`. Vectors for different models coexist and are
never compared against each other.

## Embedding durability — read this before paying for OpenAI embeddings

Embeddings can be expensive (one paid API call per document). They are protected
three ways so you don't recompute them:

1. **In-database, across upgrades.** Embeddings live in the atlas database but are
   versioned independently of the code-graph schema (`EMBEDDING_SCHEMA_VERSION`).
   A glyphtrail upgrade that changes the code-graph schema rebuilds only the
   code-graph tables and **preserves** the embeddings, their catalog, and the
   active-model pointers. They are only ever discarded if the embedding storage
   format itself changes (a deliberate version bump).

2. **Automatic Parquet backup on disk.** Every `atlas embed`, `graph-embed`,
   `embed-commits`, and `embed-import` mirrors the vectors to a portable Parquet
   backup beside the database:

   ```
   ~/.glyphtrail/atlas/ladybug-embeddings-backup/
     manifest.json
     Vec_<space>_<modelslug>_<hash>.parquet   # one per namespace
   ```

   The backup is a **sibling directory** of the database file (LadybugDB stores the
   DB as a single file, not a directory). It is refreshed to match the live data on
   every embed, removed when no embeddings remain, and written best-effort (a
   backup failure never fails an embed). Restore after a database loss with:

   ```bash
   glyphtrail atlas embed-restore-backup
   ```

   This rebuilds every namespace — the vector tables, the catalog, the
   active-model pointers, and the HNSW indexes — and works **even if the database
   file itself is gone**, as long as the `-embeddings-backup/` sibling survives.

3. **Explicit JSONL export.** `atlas embed-export` / `embed-import` is the
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

Standard Apache Parquet (`PAR1`), one row per embedded node, two columns:

| column      | Parquet type                              | meaning |
|-------------|-------------------------------------------|---------|
| `v.node_id` | `BYTE_ARRAY` (UTF-8 string)               | the node id (see below) |
| `v.vec`     | 3-level `LIST` of `FLOAT` (float32)       | the embedding vector, length = the namespace `dim` |

The `v.` prefix is an artifact of the export query (`RETURN v.node_id, v.vec`); use
those exact column names. `v.vec` uses the standard Parquet LIST encoding
(`v.vec` → `list` → `element: float`), so it decodes as a list/array column of
float32 in any Parquet reader.

`v.node_id` is a stable, one-way id, derived per space and **not reversible to a
name**: `text` and `commit`-relative repo ids are `derive(["repo", name])`, `graph`
ids are `derive(["repo_graph", name])`, and per-commit ids are the commit node ids.
To recover a human name, join against the atlas (`atlas similar`, the MCP
`atlas_similar` tool) or the registry, which hold the id→name mapping.

### Reading example

```python
import json, pyarrow.parquet as pq, numpy as np
from pathlib import Path

backup = Path.home() / ".glyphtrail/atlas/ladybug-embeddings-backup"
manifest = json.loads((backup / "manifest.json").read_text())
for ns in manifest["namespaces"]:
    t = pq.read_table(backup / ns["file"])
    ids = t.column("v.node_id").to_pylist()
    vecs = np.array(t.column("v.vec").to_pylist(), dtype=np.float32)  # (n, dim)
    print(ns["space"], ns["model"], vecs.shape)
```

```sql
-- DuckDB
SELECT "v.node_id", "v.vec"
FROM read_parquet('~/.glyphtrail/atlas/ladybug-embeddings-backup/*.parquet');
```
