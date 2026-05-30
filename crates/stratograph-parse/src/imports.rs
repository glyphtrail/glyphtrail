//! Import-target resolution.
//!
//! [`resolve_import`] turns a cleaned import token (quotes/brackets already
//! stripped by extraction) into the repository-relative paths it might name.
//! The caller matches these candidates against the actual file set (exact
//! first, then a unique path-suffix match), so we can stay agnostic about
//! source roots like `src/` or `src/main/java/`.
//!
//! Resolution is deliberately conservative: it only emits candidates for the
//! deterministic, path-shaped import styles. External packages and module
//! systems that need project metadata we don't have (Go import paths, Rust
//! `use` paths) yield no candidates and are recorded as placeholders instead.

use stratograph_core::Language;

const JS_EXTS: [&str; 7] = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs"];

/// Candidate repository-relative paths an import may resolve to, best-first.
/// Empty when the import is not locally resolvable.
pub fn resolve_import(importer_rel: &str, raw: &str, lang: &Language) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    match lang {
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            js_candidates(importer_rel, raw)
        }
        Language::C | Language::Cpp => c_candidates(importer_rel, raw),
        Language::Python => python_candidates(raw),
        Language::Java => java_candidates(raw),
        // Go/Rust/C#/Ruby/Kotlin imports name modules/namespaces, not file
        // paths, so they aren't resolved to files here.
        Language::Go
        | Language::Rust
        | Language::CSharp
        | Language::Ruby
        | Language::Kotlin
        | Language::Bash
        | Language::Php
        | Language::Scala
        | Language::OCaml
        | Language::Haskell
        | Language::Lua
        | Language::Swift
        | Language::Elixir
        | Language::Other(_) => Vec::new(),
    }
}

/// Directory portion of a repo-relative path (`""` for a top-level file).
fn dir_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// Join `rel` onto `dir` and resolve `.`/`..` segments. Returns `None` if the
/// path escapes the repository root.
fn join_normalize(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// ES/TS relative imports (`./x`, `../y`). Bare specifiers are external.
fn js_candidates(importer_rel: &str, raw: &str) -> Vec<String> {
    if !raw.starts_with('.') {
        return Vec::new();
    }
    let Some(base) = join_normalize(dir_of(importer_rel), raw) else {
        return Vec::new();
    };
    if base.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(1 + JS_EXTS.len() * 2);
    // An explicit extension in the specifier: try it verbatim first.
    out.push(base.clone());
    out.extend(JS_EXTS.iter().map(|e| format!("{base}{e}")));
    out.extend(JS_EXTS.iter().map(|e| format!("{base}/index{e}")));
    out
}

/// C/C++ includes. After quote/bracket stripping we can't tell `"x"` from
/// `<x>`, so try the path relative to the including file and to the repo root;
/// system headers simply won't match any repo file.
fn c_candidates(importer_rel: &str, raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = join_normalize(dir_of(importer_rel), raw)
        && !p.is_empty()
    {
        out.push(p);
    }
    if let Some(p) = join_normalize("", raw)
        && !p.is_empty()
        && !out.contains(&p)
    {
        out.push(p);
    }
    out
}

/// Python dotted module names (`pkg.mod`) map to a module file or package init.
fn python_candidates(raw: &str) -> Vec<String> {
    let path = raw.trim_matches('.').replace('.', "/");
    if path.is_empty() {
        return Vec::new();
    }
    vec![format!("{path}.py"), format!("{path}/__init__.py")]
}

/// Java fully-qualified types (`com.foo.Bar`) map to `com/foo/Bar.java`.
/// Wildcard imports (`com.foo.*`) name a package, not a type, so are skipped.
fn java_candidates(raw: &str) -> Vec<String> {
    if raw.is_empty() || raw.ends_with(".*") {
        return Vec::new();
    }
    vec![format!("{}.java", raw.replace('.', "/"))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn js_relative_resolves_with_extensions_and_index() {
        let c = resolve_import("web/app.ts", "./util", &Language::TypeScript);
        check!(c.contains(&"web/util.ts".to_string()));
        check!(c.contains(&"web/util/index.ts".to_string()));
        // Parent traversal.
        let c = resolve_import("web/sub/app.ts", "../util", &Language::TypeScript);
        check!(c.contains(&"web/util.ts".to_string()));
    }

    #[test]
    fn js_bare_specifier_is_external() {
        check!(resolve_import("web/app.ts", "react", &Language::TypeScript).is_empty());
        check!(resolve_import("web/app.ts", "@scope/pkg", &Language::Tsx).is_empty());
    }

    #[test]
    fn js_escaping_root_yields_nothing() {
        check!(resolve_import("app.ts", "../../x", &Language::JavaScript).is_empty());
    }

    #[test]
    fn c_include_tries_local_and_root() {
        let c = resolve_import("src/a.c", "util.h", &Language::C);
        check!(c.contains(&"src/util.h".to_string()));
        check!(c.contains(&"util.h".to_string()));
    }

    #[test]
    fn python_dotted_maps_to_module_or_package() {
        let c = resolve_import("app/main.py", "app.models", &Language::Python);
        check!(c == vec!["app/models.py", "app/models/__init__.py"]);
    }

    #[test]
    fn java_fqn_maps_to_path_but_not_wildcard() {
        check!(
            resolve_import("X.java", "com.foo.Bar", &Language::Java) == vec!["com/foo/Bar.java"]
        );
        check!(resolve_import("X.java", "com.foo.*", &Language::Java).is_empty());
    }

    #[test]
    fn go_and_rust_are_not_locally_resolved() {
        check!(resolve_import("m.go", "github.com/x/y", &Language::Go).is_empty());
        check!(resolve_import("m.rs", "crate::a::b", &Language::Rust).is_empty());
    }
}
