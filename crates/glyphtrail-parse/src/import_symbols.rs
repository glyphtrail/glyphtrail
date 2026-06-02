//! Symbol-level import extraction: which local name was imported from which
//! module (#167). The general import extraction records only module specifiers;
//! to resolve a *variable* (e.g. a mounted router) to the file that defines it,
//! we also need the `(local symbol, module)` pairs. JS/TS/TSX and Python are
//! covered — the two ecosystems with router-variable mounts.

use glyphtrail_core::Language;
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// `(local symbol, module specifier)` pairs from `source`. Empty on parse
/// failure or for languages without symbol-level import support here.
pub fn extract_import_symbols(source: &str, lang: &Language) -> Vec<(String, String)> {
    let Some(tree) = parse(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    match lang {
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            walk(tree.root_node(), &mut |n| match n.kind() {
                "import_statement" => js_import(n, src, &mut out),
                "lexical_declaration" | "variable_declaration" => js_require(n, src, &mut out),
                _ => {}
            });
        }
        Language::Python => {
            walk(tree.root_node(), &mut |n| {
                if n.kind() == "import_from_statement" {
                    py_from_import(n, src, &mut out);
                }
            });
        }
        Language::Java => {
            walk(tree.root_node(), &mut |n| {
                if n.kind() == "import_declaration" {
                    java_static_import(n, src, &mut out);
                }
            });
        }
        _ => {}
    }
    out
}

fn parse(source: &str, lang: &Language) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(lang)?).ok()?;
    parser.parse(source, None)
}

/// `import X from "m"` / `import { a, b as c } from "m"`.
fn js_import(n: Node, src: &[u8], out: &mut Vec<(String, String)>) {
    let Some(module) = n
        .child_by_field_name("source")
        .map(|s| string_text(s, src))
        .filter(|m| !m.is_empty())
    else {
        return;
    };
    let mut cursor = n.walk();
    let Some(clause) = n
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause")
    else {
        return;
    };
    let mut cc = clause.walk();
    for c in clause.named_children(&mut cc) {
        match c.kind() {
            // default import: `import X from "m"` → X
            "identifier" => out.push((text(c, src), module.clone())),
            "named_imports" => {
                let mut nc = c.walk();
                for spec in c.named_children(&mut nc) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    // `name` → name; `name as alias` → alias (the local binding).
                    let mut sc = spec.walk();
                    let ids: Vec<Node> = spec
                        .named_children(&mut sc)
                        .filter(|x| x.kind() == "identifier")
                        .collect();
                    if let Some(local) = ids.last() {
                        out.push((text(*local, src), module.clone()));
                    }
                }
            }
            _ => {}
        }
    }
}

/// `const X = require("m")`.
fn js_require(n: Node, src: &[u8], out: &mut Vec<(String, String)>) {
    let mut cursor = n.walk();
    for decl in n.named_children(&mut cursor) {
        if decl.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = decl
            .child_by_field_name("name")
            .filter(|x| x.kind() == "identifier")
            .map(|x| text(x, src))
        else {
            continue;
        };
        if let Some(value) = decl.child_by_field_name("value")
            && value.kind() == "call_expression"
            && value
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .as_deref()
                == Some("require")
            && let Some(args) = value.child_by_field_name("arguments")
        {
            let mut ac = args.walk();
            if let Some(m) = args
                .named_children(&mut ac)
                .find(|a| a.kind() == "string")
                .map(|s| string_text(s, src))
                .filter(|m| !m.is_empty())
            {
                out.push((name, m));
            }
        }
    }
}

/// `from m import a, b as c`.
fn py_from_import(n: Node, src: &[u8], out: &mut Vec<(String, String)>) {
    let Some(module_node) = n.child_by_field_name("module_name") else {
        return;
    };
    let module = text(module_node, src);
    if module.is_empty() {
        return;
    }
    // The imported names follow the module: `dotted_name` (plain) or
    // `aliased_import` (`a as b`, the alias being the local binding).
    let mut cursor = n.walk();
    for c in n.named_children(&mut cursor) {
        if c.id() == module_node.id() {
            continue;
        }
        match c.kind() {
            "dotted_name" => out.push((text(c, src), module.clone())),
            "aliased_import" => {
                if let Some(alias) = c.child_by_field_name("alias") {
                    out.push((text(alias, src), module.clone()));
                }
            }
            _ => {}
        }
    }
}

/// `import static pkg.Class.method;` -> `(method, "pkg.Class")` so an unqualified
/// `method()` call can resolve to the declaring class's file (#395). Non-static
/// imports name a type (handled by the module-level extractor), and static
/// wildcards (`import static pkg.Class.*;`) name no specific symbol, so both are
/// skipped here.
fn java_static_import(n: Node, src: &[u8], out: &mut Vec<(String, String)>) {
    let mut cursor = n.walk();
    let children: Vec<Node> = n.children(&mut cursor).collect();
    let is_static = children.iter().any(|c| c.kind() == "static");
    let is_wildcard = children.iter().any(|c| c.kind() == "asterisk");
    if !is_static || is_wildcard {
        return;
    }
    let Some(scoped) = children.iter().find(|c| c.kind() == "scoped_identifier") else {
        return;
    };
    let full = text(*scoped, src); // `pkg.Class.method`
    if let Some((module, symbol)) = full.rsplit_once('.')
        && !module.is_empty()
        && !symbol.is_empty()
    {
        out.push((symbol.to_string(), module.to_string()));
    }
}

fn text(n: Node, src: &[u8]) -> String {
    n.utf8_text(src).unwrap_or("").to_string()
}

/// Inner text of a JS string literal (its `string_fragment`).
fn string_text(n: Node, src: &[u8]) -> String {
    let mut cursor = n.walk();
    n.named_children(&mut cursor)
        .find(|c| c.kind() == "string_fragment")
        .map(|c| text(c, src))
        .unwrap_or_default()
}

fn walk(node: Node, f: &mut dyn FnMut(Node)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn js_named_default_and_alias_imports() {
        let src = "import apiRouter from './api';\nimport { router, foo as bar } from './users';\n";
        let got = extract_import_symbols(src, &Language::JavaScript);
        check!(got.contains(&("apiRouter".into(), "./api".into())));
        check!(got.contains(&("router".into(), "./users".into())));
        // `foo as bar` binds the local name `bar`.
        check!(got.contains(&("bar".into(), "./users".into())));
    }

    #[test]
    fn js_require_binding() {
        let got = extract_import_symbols("const users = require('./users');\n", &Language::Tsx);
        check!(got == vec![("users".to_string(), "./users".to_string())]);
    }

    #[test]
    fn python_from_imports_with_alias() {
        let src = "from .users import router\nfrom .api import a as b\nimport os\n";
        let got = extract_import_symbols(src, &Language::Python);
        check!(got.contains(&("router".into(), ".users".into())));
        check!(got.contains(&("b".into(), ".api".into())));
        // Plain `import os` is not a from-import; not captured here.
        check!(!got.iter().any(|(s, _)| s == "os"));
    }

    #[test]
    fn java_static_imports() {
        let src = "package app;\n\
                   import static util.MathUtil.square;\n\
                   import util.Other;\n\
                   import static util.MathUtil.*;\n";
        let got = extract_import_symbols(src, &Language::Java);
        // The static single-member import binds `square` from `util.MathUtil`.
        check!(got == vec![("square".to_string(), "util.MathUtil".to_string())]);
        // A plain type import and a static wildcard contribute no symbol here.
    }

    #[test]
    fn skips_unsupported_languages() {
        check!(extract_import_symbols("use crate::x;", &Language::Rust).is_empty());
    }
}
