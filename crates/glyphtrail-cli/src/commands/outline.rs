//! `glyphtrail outline <path>` — the symbol skeleton of a file or directory:
//! each definition with its signature, at adjustable detail. A compact "shape
//! of the code" view for priming an agent (or a human) before deeper queries.

use std::path::Path;

use anyhow::Result;
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{Detail, OutlineSymbol, is_outline_kind, outline_symbol};
use serde_json::{Value, json};

use crate::commands::backend;
use crate::commands::query::{Emit, print_value};
use crate::ui;

pub fn run(repo: &Path, path: &str, detail: Detail, emit: Emit) -> Result<()> {
    let paths = RepoPaths::new(repo);
    let store = backend::open_existing(&paths)?;
    let prefix = repo_relative(repo, path);

    // Indexed files under `path` (a single file matches itself).
    let mut files: Vec<String> = store
        .all_files()?
        .into_iter()
        .filter(|f| prefix.is_empty() || *f == prefix || f.starts_with(&format!("{prefix}/")))
        .collect();
    files.sort();

    let mut outline: Vec<(String, Vec<OutlineSymbol>)> = Vec::new();
    for file in files {
        let mut nodes: Vec<_> = store
            .nodes_in_file(&file)?
            .into_iter()
            .filter(|n| is_outline_kind(n.kind))
            .collect();
        nodes.sort_by_key(|n| n.span.map(|s| s.start_line).unwrap_or(0));
        // Read each source once; skipped for the minimal (no-signature) level.
        let source = (detail != Detail::Minimal)
            .then(|| std::fs::read_to_string(paths.root.join(&file)).ok())
            .flatten();
        let symbols: Vec<OutlineSymbol> = nodes
            .iter()
            .map(|n| outline_symbol(n, source.as_deref(), detail))
            .collect();
        if !symbols.is_empty() {
            outline.push((file, symbols));
        }
    }

    match emit {
        Emit::Text => print_text(&outline, &prefix),
        _ => print_value(&to_value(&outline), emit)?,
    }
    Ok(())
}

/// Resolve `path` to a repo-root-relative, forward-slashed prefix. Canonicalises
/// against the repo root when both exist; otherwise treats `path` as already
/// relative (handling `./` and trailing slashes).
fn repo_relative(repo: &Path, path: &str) -> String {
    if let (Ok(root), Ok(target)) = (repo.canonicalize(), Path::new(path).canonicalize())
        && let Ok(rel) = target.strip_prefix(&root)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .replace('\\', "/")
}

/// The line shown for a symbol: its sliced signature, else `kind name`.
fn symbol_line(s: &OutlineSymbol) -> String {
    s.signature
        .clone()
        .unwrap_or_else(|| format!("{} {}", s.kind.as_str(), s.name))
}

fn print_text(outline: &[(String, Vec<OutlineSymbol>)], prefix: &str) {
    if outline.is_empty() {
        println!("no indexed symbols under '{prefix}'");
        return;
    }
    if ui::pretty() {
        for (file, symbols) in outline {
            println!("{file}");
            for s in symbols {
                let doc = s
                    .doc
                    .as_deref()
                    .map(|d| format!("  · {d}"))
                    .unwrap_or_default();
                println!("  {}  :{}{doc}", symbol_line(s), s.line);
            }
        }
    } else {
        // Plain, greppable: `file:line signature` (the signature already carries
        // the kind keyword; minimal falls back to `kind name`).
        for (file, symbols) in outline {
            for s in symbols {
                println!("{file}:{} {}", s.line, symbol_line(s));
            }
        }
    }
}

/// Per-file symbol arrays for `--json` / `--yaml`.
fn to_value(outline: &[(String, Vec<OutlineSymbol>)]) -> Value {
    Value::Array(
        outline
            .iter()
            .map(|(file, symbols)| json!({ "file": file, "symbols": symbols }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_core::NodeKind;

    fn sym(name: &str, sig: Option<&str>) -> OutlineSymbol {
        OutlineSymbol {
            name: name.into(),
            qualified_name: name.into(),
            kind: NodeKind::Function,
            line: 7,
            signature: sig.map(str::to_string),
            doc: None,
        }
    }

    #[test]
    fn symbol_line_prefers_signature() {
        check!(symbol_line(&sym("go", Some("fn go() -> u8"))) == "fn go() -> u8");
        check!(symbol_line(&sym("go", None)) == "function go"); // falls back to kind name
    }

    #[test]
    fn to_value_groups_by_file() {
        let outline = vec![("a.rs".to_string(), vec![sym("go", Some("fn go()"))])];
        let v = to_value(&outline);
        check!(v[0]["file"] == json!("a.rs"));
        check!(v[0]["symbols"][0]["name"] == json!("go"));
        check!(v[0]["symbols"][0]["signature"] == json!("fn go()"));
    }
}
