//! Runtime loading of extra tree-sitter grammars — the dynamic half of hybrid
//! language support.
//!
//! A grammar's tree-sitter `src` directory (`parser.c` + `grammar.json`, plus
//! an optional scanner) is compiled on demand by `tree-sitter-loader` (offline;
//! needs a C toolchain) and paired with a `.scm` query so the same capture
//! convention as the built-in languages drives extraction. Pair the result with
//! [`crate::parse_with`] to parse a file with no compiled-in support.

use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::Language as TsLanguage;
use tree_sitter_loader::{CompileConfig, Loader};

/// A dynamically-loaded grammar and its extraction query.
pub struct DynamicGrammar {
    pub grammar: TsLanguage,
    pub query: String,
}

/// Compile + load the grammar whose tree-sitter `src` directory is `grammar_src`
/// and read its extraction query from `query_path`. The compiled parser is
/// cached by `tree-sitter-loader` (under `TREE_SITTER_LIBDIR` or the platform
/// cache), so repeated loads of the same grammar are cheap.
pub fn load_dynamic(grammar_src: &Path, query_path: &Path) -> Result<DynamicGrammar> {
    let loader = Loader::new().context("initialize tree-sitter loader")?;
    let config = CompileConfig::new(grammar_src, None, None);
    let grammar = loader
        .load_language_at_path(config)
        .map_err(|e| anyhow::anyhow!("compile/load grammar at {}: {e}", grammar_src.display()))?;
    let query = std::fs::read_to_string(query_path)
        .with_context(|| format!("read query {}", query_path.display()))?;
    Ok(DynamicGrammar { grammar, query })
}
