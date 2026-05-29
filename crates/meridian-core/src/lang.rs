use serde::{Deserialize, Serialize};
use std::path::Path;

/// Languages the analyzer can parse. The built-in variants each have a grammar
/// in the parse registry and a query file; [`Language::Other`] names a language
/// identified at runtime whose grammar is supplied by the dynamic loader rather
/// than compiled in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Kotlin,
    Bash,
    Php,
    Scala,
    OCaml,
    Haskell,
    Lua,
    Swift,
    Elixir,
    /// A language identified by name but not built in (dynamically loaded).
    Other(String),
}

impl Language {
    pub fn name(&self) -> &str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Kotlin => "kotlin",
            Language::Bash => "bash",
            Language::Php => "php",
            Language::Scala => "scala",
            Language::OCaml => "ocaml",
            Language::Haskell => "haskell",
            Language::Lua => "lua",
            Language::Swift => "swift",
            Language::Elixir => "elixir",
            Language::Other(name) => name,
        }
    }

    /// Best-effort detection from a file extension.
    pub fn from_path(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "cs" => Language::CSharp,
            "rb" => Language::Ruby,
            "kt" | "kts" => Language::Kotlin,
            "sh" | "bash" => Language::Bash,
            "php" => Language::Php,
            "scala" | "sc" => Language::Scala,
            "ml" | "mli" => Language::OCaml,
            "hs" => Language::Haskell,
            "lua" => Language::Lua,
            "swift" => Language::Swift,
            "ex" | "exs" => Language::Elixir,
            _ => return None,
        })
    }

    pub fn from_name(name: &str) -> Option<Language> {
        Some(match name {
            "rust" => Language::Rust,
            "python" => Language::Python,
            "javascript" => Language::JavaScript,
            "typescript" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" => Language::C,
            "cpp" => Language::Cpp,
            "csharp" => Language::CSharp,
            "ruby" => Language::Ruby,
            "kotlin" => Language::Kotlin,
            "bash" => Language::Bash,
            "php" => Language::Php,
            "scala" => Language::Scala,
            "ocaml" => Language::OCaml,
            "haskell" => Language::Haskell,
            "lua" => Language::Lua,
            "swift" => Language::Swift,
            "elixir" => Language::Elixir,
            _ => return None,
        })
    }

    pub const ALL: [Language; 20] = [
        Language::Rust,
        Language::Python,
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Go,
        Language::Java,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Ruby,
        Language::Kotlin,
        Language::Bash,
        Language::Php,
        Language::Scala,
        Language::OCaml,
        Language::Haskell,
        Language::Lua,
        Language::Swift,
        Language::Elixir,
    ];
}
