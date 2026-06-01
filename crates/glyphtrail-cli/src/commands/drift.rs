//! `glyphtrail drift` — reconcile code-first API endpoints against the blessed
//! schema operations and report contract drift (#45).
//!
//! The analyzer already discovers blessed schema files (config-gated
//! `[[api.schemas]]`) into `SchemaOp` nodes and links a code `Endpoint` to the
//! operation it implements with an `EXPOSES` edge. Drift is then read off the
//! graph: an endpoint with no outgoing `EXPOSES` is **undocumented** (served but
//! not in the contract), and a schema op with no incoming `EXPOSES` is an
//! **orphan** (declared in the contract but not served). A method/path mismatch
//! surfaces as both — the two keys simply fail to match.

use anyhow::Result;
use clap::{Args, ValueEnum};
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{EdgeKind, NodeKind, OperationKey};
use glyphtrail_store::GraphStore;
use serde::Serialize;

use crate::commands::backend;

/// Exit code used by `--gate` when the API surface has drifted.
const GATE_EXIT_CODE: i32 = 2;

#[derive(Args)]
pub struct DriftArgs {
    /// Repository root.
    #[arg(long, default_value = ".")]
    pub repo: std::path::PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
    /// Exit non-zero (code 2) when any drift is found (for CI gating).
    #[arg(long)]
    pub gate: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Format {
    #[default]
    Text,
    Json,
    /// YAML — compact structured output for LLMs/agents.
    Yaml,
}

/// One endpoint or schema operation in the drift report.
#[derive(Serialize)]
struct Item {
    protocol: &'static str,
    method: Option<&'static str>,
    path: String,
    file: String,
    line: Option<usize>,
}

#[derive(Serialize)]
struct Report {
    /// Endpoints served by code but absent from the blessed schema.
    undocumented_endpoints: Vec<Item>,
    /// Schema operations declared in the contract but served by no endpoint.
    orphan_schema_ops: Vec<Item>,
    /// Whether a blessed schema was present to reconcile against.
    schema_present: bool,
}

impl Report {
    fn has_drift(&self) -> bool {
        !self.undocumented_endpoints.is_empty() || !self.orphan_schema_ops.is_empty()
    }
}

pub fn run(args: DriftArgs) -> Result<()> {
    let paths = RepoPaths::new(&args.repo);
    let store = backend::open_existing(&paths)?;
    crate::commands::note_staleness(&args.repo, store.as_ref());
    let report = reconcile(store.as_ref())?;

    match args.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        Format::Yaml => print!("{}", serde_norway::to_string(&report)?),
        Format::Text => print_text(&report),
    }

    if args.gate && report.has_drift() {
        std::process::exit(GATE_EXIT_CODE);
    }
    Ok(())
}

/// Reconcile code endpoints against schema operations over the graph: an
/// endpoint with no outgoing `EXPOSES` is undocumented; a schema op with no
/// incoming `EXPOSES` is orphan. Undocumented endpoints are only reported when a
/// blessed schema exists (else every endpoint is trivially undocumented).
fn reconcile(store: &dyn GraphStore) -> Result<Report> {
    let schema_ops = store.operations_by_kind(NodeKind::SchemaOp)?;
    let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
    let schema_present = !schema_ops.is_empty();

    let mut undocumented = Vec::new();
    if schema_present {
        for (id, key) in &endpoints {
            if store
                .neighbors(&id.0, Some(EdgeKind::Exposes), true)?
                .is_empty()
            {
                undocumented.push(item(store, &id.0, key)?);
            }
        }
    }
    let mut orphan = Vec::new();
    for (id, key) in &schema_ops {
        if store
            .neighbors(&id.0, Some(EdgeKind::Exposes), false)?
            .is_empty()
        {
            orphan.push(item(store, &id.0, key)?);
        }
    }
    Ok(Report {
        undocumented_endpoints: undocumented,
        orphan_schema_ops: orphan,
        schema_present,
    })
}

/// Build a report item from an operation node, resolving its file/line.
fn item(store: &dyn GraphStore, id: &str, key: &OperationKey) -> Result<Item> {
    let node = store.get_node(id)?;
    Ok(Item {
        protocol: key.protocol.as_str(),
        method: key.method.map(|m| m.as_str()),
        path: key.path.clone(),
        file: node.as_ref().map(|n| n.file.clone()).unwrap_or_default(),
        line: node.as_ref().and_then(|n| n.span.map(|s| s.start_line)),
    })
}

fn print_text(report: &Report) {
    if !report.schema_present {
        println!(
            "No blessed schema configured ([[api.schemas]]); nothing to reconcile \
             (see `glyphtrail query endpoints` for the indexed routes)."
        );
        return;
    }
    let label = |i: &Item| match i.method {
        Some(m) => format!("{m} {}", i.path),
        None => format!("{} {}", i.protocol, i.path),
    };
    let loc = |i: &Item| match i.line {
        Some(l) => format!(" ({}:{l})", i.file),
        None if !i.file.is_empty() => format!(" ({})", i.file),
        None => String::new(),
    };

    println!(
        "API drift: {} undocumented endpoint(s), {} orphan schema op(s).",
        report.undocumented_endpoints.len(),
        report.orphan_schema_ops.len()
    );
    if !report.undocumented_endpoints.is_empty() {
        println!("\nUndocumented (served, not in schema):");
        for i in &report.undocumented_endpoints {
            println!("  - {}{}", label(i), loc(i));
        }
    }
    if !report.orphan_schema_ops.is_empty() {
        println!("\nOrphan schema ops (in schema, no handler):");
        for i in &report.orphan_schema_ops {
            println!("  - {}{}", label(i), loc(i));
        }
    }
    if !report.has_drift() {
        println!("No drift: code endpoints and schema operations reconcile.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_core::{Confidence, Edge, HttpMethod, Node, NodeId, Span};
    use glyphtrail_store::LadybugStore;

    fn op_node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            name: id.into(),
            qualified_name: id.into(),
            file: "api.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
            }),
            doc: None,
        }
    }

    #[test]
    fn reconcile_reports_undocumented_and_orphan() {
        let mut store = LadybugStore::open_temp().unwrap();
        // Documented endpoint <-> schema op (linked by EXPOSES); an undocumented
        // endpoint; and an orphan schema op.
        let nodes = [
            op_node("e_doc", NodeKind::Endpoint),
            op_node("e_undoc", NodeKind::Endpoint),
            op_node("so_doc", NodeKind::SchemaOp),
            op_node("so_orphan", NodeKind::SchemaOp),
        ];
        let exposes = Edge {
            src: NodeId("e_doc".into()),
            dst: NodeId("so_doc".into()),
            kind: EdgeKind::Exposes,
            confidence: Confidence::Extracted,
        };
        store.insert_graph(&nodes, &[exposes]).unwrap();
        store
            .insert_operations(&[
                (
                    NodeId("e_doc".into()),
                    OperationKey::rest(HttpMethod::Get, "/users"),
                ),
                (
                    NodeId("e_undoc".into()),
                    OperationKey::rest(HttpMethod::Get, "/secret"),
                ),
                (
                    NodeId("so_doc".into()),
                    OperationKey::rest(HttpMethod::Get, "/users"),
                ),
                (
                    NodeId("so_orphan".into()),
                    OperationKey::rest(HttpMethod::Get, "/legacy"),
                ),
            ])
            .unwrap();

        let report = reconcile(&store).unwrap();
        check!(report.schema_present);
        check!(report.has_drift());
        let undoc: Vec<&str> = report
            .undocumented_endpoints
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        check!(undoc == ["/secret"]);
        let orphan: Vec<&str> = report
            .orphan_schema_ops
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        check!(orphan == ["/legacy"]);
    }

    #[test]
    fn no_schema_means_no_undocumented_noise() {
        let mut store = LadybugStore::open_temp().unwrap();
        store
            .insert_graph(&[op_node("e1", NodeKind::Endpoint)], &[])
            .unwrap();
        store
            .insert_operations(&[(
                NodeId("e1".into()),
                OperationKey::rest(HttpMethod::Get, "/x"),
            )])
            .unwrap();
        let report = reconcile(&store).unwrap();
        check!(!report.schema_present);
        check!(report.undocumented_endpoints.is_empty());
        check!(!report.has_drift());
    }
}
