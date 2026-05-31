//! Minimal `.csproj` reader for .NET / NuGet cross-repo identity (#272-adjacent).
//!
//! A repo publishes NuGet packages and consumes others (commonly via a private
//! NuGet feed). This extracts, from a project file, the package a project
//! *publishes* and the packages it *references* — the producer and consumer
//! sides the cross-repo link step matches, exactly as the Cargo manifest reader
//! does for crates.
//!
//! It is deliberately a lightweight string scan rather than a full XML parse:
//! `.csproj` is not a parsed source language, the handful of elements we need
//! are unambiguous, and a best-effort read (skipping anything malformed) is the
//! right behaviour for an analysis pass. Pure: bytes in, identity out.

/// What a single `.csproj` declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsprojProject {
    /// The published package id: `<PackageId>` if set, else `<AssemblyName>`,
    /// else the caller's fallback (the file stem). This is the cross-repo match
    /// key — what another project writes in `<PackageReference Include="…">`.
    pub package_id: String,
    /// `<Version>`/`<PackageVersion>` when present.
    pub version: Option<String>,
    /// `Include` values of every `<PackageReference>` — the NuGet packages this
    /// project consumes.
    pub package_refs: Vec<String>,
}

/// Parse a `.csproj`, using `fallback_id` (typically the file stem) as the
/// package id when neither `<PackageId>` nor `<AssemblyName>` is set — which is
/// the NuGet default (the package id is the assembly name is the project file
/// name).
pub fn parse_csproj(text: &str, fallback_id: &str) -> CsprojProject {
    let package_id = first_element(text, "PackageId")
        .or_else(|| first_element(text, "AssemblyName"))
        .unwrap_or_else(|| fallback_id.to_string());
    let version = first_element(text, "Version").or_else(|| first_element(text, "PackageVersion"));
    let mut package_refs: Vec<String> = package_reference_includes(text);
    package_refs.sort();
    package_refs.dedup();
    CsprojProject {
        package_id,
        version,
        package_refs,
    }
}

/// The text content of the first `<tag>…</tag>`, trimmed. `None` if absent or
/// empty. Ignores attributes on the open tag and is case-sensitive (MSBuild
/// element names are conventionally PascalCase).
fn first_element(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The `Include="…"` value of every `<PackageReference …>`. Tolerates single or
/// double quotes and arbitrary attribute order/whitespace.
fn package_reference_includes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("<PackageReference") {
        // Bound the element to its tag end so an `Include` from a *later* element
        // can't be misattributed.
        let after = &rest[pos..];
        let tag_end = after.find('>').map(|e| e + 1).unwrap_or(after.len());
        if let Some(include) = attr_value(&after[..tag_end], "Include") {
            out.push(include);
        }
        rest = &after[tag_end..];
    }
    out
}

/// Value of `name="…"` (or `name='…'`) within a single tag's text.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let start = tag.find(&key)? + key.len();
    let bytes = tag[start..].trim_start();
    let quote = bytes.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &bytes[1..];
    let end = inner.find(quote)?;
    let value = inner[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn reads_package_id_version_and_references() {
        let csproj = r#"
            <Project Sdk="Microsoft.NET.Sdk">
              <PropertyGroup>
                <PackageId>Acme.Models</PackageId>
                <Version>2.3.1</Version>
              </PropertyGroup>
              <ItemGroup>
                <PackageReference Include="Acme.Core" Version="1.0.0" />
                <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
              </ItemGroup>
            </Project>
        "#;
        let p = parse_csproj(csproj, "ignored");
        check!(p.package_id == "Acme.Models");
        check!(p.version == Some("2.3.1".to_string()));
        check!(p.package_refs == vec!["Acme.Core".to_string(), "Newtonsoft.Json".to_string()]);
    }

    #[test]
    fn falls_back_to_assembly_name_then_file_stem() {
        let with_asm = "<Project><PropertyGroup><AssemblyName>Acme.Tooling</AssemblyName></PropertyGroup></Project>";
        check!(parse_csproj(with_asm, "stem").package_id == "Acme.Tooling");
        let bare = "<Project><ItemGroup></ItemGroup></Project>";
        check!(parse_csproj(bare, "Acme.Service").package_id == "Acme.Service");
    }

    #[test]
    fn tolerates_single_quotes_and_attribute_order() {
        let csproj = "<Project><ItemGroup>\
            <PackageReference Version=\"1.0\" Include='Acme.Core' />\
            </ItemGroup></Project>";
        check!(parse_csproj(csproj, "x").package_refs == vec!["Acme.Core".to_string()]);
    }

    #[test]
    fn ignores_project_references_and_dedups() {
        let csproj = "<Project><ItemGroup>\
            <ProjectReference Include=\"../Other/Other.csproj\" />\
            <PackageReference Include=\"Acme.Core\" />\
            <PackageReference Include=\"Acme.Core\" />\
            </ItemGroup></Project>";
        let p = parse_csproj(csproj, "x");
        check!(p.package_refs == vec!["Acme.Core".to_string()]); // ProjectReference excluded; deduped
    }
}
