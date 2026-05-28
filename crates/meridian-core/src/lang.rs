use serde::{Deserialize, Serialize};
use std::path::Path;

/// Languages the analyzer can parse. Adding one is: a variant here, a grammar in
/// the parse registry, and a query file — mirroring the breadth of tree-sitter's
/// language packs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

impl Language {
    pub fn name(self) -> &'static str {
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
            _ => return None,
        })
    }

    pub const ALL: [Language; 10] = [
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
    ];
}
