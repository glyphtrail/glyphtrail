PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
    path       TEXT PRIMARY KEY,
    language   TEXT,
    hash       TEXT NOT NULL,
    indexed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS nodes (
    id             TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    file           TEXT NOT NULL,
    language       TEXT,
    start_byte     INTEGER,
    end_byte       INTEGER,
    start_line     INTEGER,
    end_line       INTEGER,
    doc            TEXT
);

CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file);
CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);

CREATE TABLE IF NOT EXISTS edges (
    src        TEXT NOT NULL,
    dst        TEXT NOT NULL,
    kind       TEXT NOT NULL,
    confidence TEXT NOT NULL,
    PRIMARY KEY (src, dst, kind)
);

CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst);

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    id UNINDEXED,
    name,
    qualified_name,
    doc
);

-- API operation keys for endpoint / client-call / schema-op nodes. Kept in a
-- side table (keyed by node id) so the generic nodes table stays protocol-free.
CREATE TABLE IF NOT EXISTS api_operations (
    node_id   TEXT PRIMARY KEY,
    protocol  TEXT NOT NULL,
    method    TEXT,
    path      TEXT NOT NULL,
    signature TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_api_ops_sig ON api_operations(signature);

-- Unresolved cross-file edges, persisted so they can be re-resolved against the
-- whole graph on every (incremental) run. `anchor` is the known endpoint of the
-- edge; `name` is the symbol to resolve for the other endpoint. When
-- `name_is_src` is 1 the resolved node is the edge source (`name -> anchor`),
-- otherwise it is the destination (`anchor -> name`).
CREATE TABLE IF NOT EXISTS pending_edges (
    anchor      TEXT NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    name_is_src INTEGER NOT NULL,
    PRIMARY KEY (anchor, name, kind, name_is_src)
);

CREATE INDEX IF NOT EXISTS idx_pending_anchor ON pending_edges(anchor);

-- Raw import targets per file, persisted so IMPORTS edges can be rebuilt and
-- re-resolved against the whole file set on every (incremental) run. `importer`
-- is the file's repo-relative path; `raw` is the cleaned import token.
CREATE TABLE IF NOT EXISTS pending_imports (
    importer TEXT NOT NULL,
    raw      TEXT NOT NULL,
    language TEXT NOT NULL,
    PRIMARY KEY (importer, raw, language)
);

CREATE INDEX IF NOT EXISTS idx_pending_imports_importer ON pending_imports(importer);
