use meridian_core::Language;
use tree_sitter::Language as TsLanguage;

/// Resolve a [`Language`] to its tree-sitter grammar.
pub fn grammar(lang: Language) -> TsLanguage {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
    }
}

/// The extraction query for a language. Captures follow a small convention shared
/// across grammars (see `queries/<lang>.scm`):
///   `@def.<kind>` + `@name`  — a definition and its identifier
///   `@call`                  — a call/reference identifier
///   `@import`                — an imported name or path
///   `@extends` / `@implements` — supertype identifiers
///   `@comment`               — a comment node (for rationale colocation)
pub fn query_source(lang: Language) -> &'static str {
    match lang {
        Language::Rust => include_str!("../queries/rust.scm"),
        Language::Python => include_str!("../queries/python.scm"),
        Language::JavaScript => include_str!("../queries/javascript.scm"),
        Language::TypeScript => include_str!("../queries/typescript.scm"),
        Language::Tsx => include_str!("../queries/typescript.scm"),
        Language::Go => include_str!("../queries/go.scm"),
        Language::Java => include_str!("../queries/java.scm"),
        Language::C => include_str!("../queries/c.scm"),
        Language::Cpp => include_str!("../queries/cpp.scm"),
        Language::CSharp => include_str!("../queries/csharp.scm"),
    }
}
