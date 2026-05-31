//! `glyphtrail wiki` — generate a docs wiki from the knowledge graph via an LLM
//! (#52). Facts are extracted from the graph (offline, deterministic), composed
//! into per-page prompts, and sent to a configurable provider (Anthropic /
//! OpenAI / OpenRouter). `--dry-run` writes the prompts instead of calling the
//! API, so the graph→prompt pipeline is fully testable without network or keys.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use glyphtrail_core::NodeKind;
use glyphtrail_core::config::RepoPaths;
use glyphtrail_store::GraphStore;

use super::backend;
use super::llm::{Llm, Provider};

#[derive(Args)]
pub struct WikiArgs {
    /// Repository root.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// LLM provider.
    #[arg(long, value_enum, default_value_t)]
    pub provider: Provider,
    /// Model id (defaults to a sensible per-provider model).
    #[arg(long)]
    pub model: Option<String>,
    /// Override the API base URL (for OpenAI-compatible gateways, e.g. Kilo).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Output directory for the generated pages.
    #[arg(long, default_value = "wiki")]
    pub output: PathBuf,
    /// Output target: plain Markdown, a GitHub Pages site (adds a Jekyll
    /// `_config.yml`), a GitHub repo-wiki layout (`Home.md` + `_Sidebar.md`), or
    /// serve the generated pages locally over HTTP.
    #[arg(long, value_enum, default_value_t)]
    pub target: Target,
    /// Port for `--target serve` (the local wiki browser).
    #[arg(long, default_value_t = 7780)]
    pub port: u16,
    /// Write the composed prompts instead of calling the LLM (no network/keys).
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Target {
    /// Plain Markdown pages.
    #[default]
    Static,
    /// Markdown plus a Jekyll `_config.yml` for GitHub Pages.
    Pages,
    /// Generate, then serve the pages locally over HTTP (rendered to HTML).
    Serve,
    /// GitHub repository-wiki layout: `Home.md` home page, title-named pages and
    /// a `_Sidebar.md` nav, ready to commit into a `<repo>.wiki.git` checkout.
    Wiki,
}

pub fn run(args: WikiArgs) -> Result<()> {
    let paths = RepoPaths::new(&args.repo);
    // `--target serve --dry-run` browses a previously-generated wiki offline:
    // skip the graph/LLM work and just serve the existing output directory.
    if args.target == Target::Serve && args.dry_run {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(glyphtrail_server::serve_wiki(
            args.output.clone(),
            args.port,
        ));
    }

    let store = backend::open_existing(&paths)?;
    let pages = build_pages(store.as_ref())?;

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("cannot create {}", args.output.display()))?;

    if args.dry_run {
        for p in &pages {
            let path = args.output.join(format!("{}.prompt.txt", p.slug));
            std::fs::write(
                &path,
                format!("# SYSTEM\n{}\n\n# USER\n{}\n", p.system, p.user),
            )?;
            println!("wrote {}", path.display());
        }
        println!("(dry run: {} prompt(s); no LLM called)", pages.len());
    } else {
        let llm = Llm::new(args.provider, args.model, args.base_url)?;
        for p in &pages {
            let md = llm
                .complete(&p.system, &p.user)
                .with_context(|| format!("generating page '{}'", p.slug))?;
            // The GitHub repo-wiki target names files by wiki page (Home.md, …);
            // every other target uses the slug.
            let file = match args.target {
                Target::Wiki => format!("{}.md", wiki_page_name(p.slug)),
                _ => format!("{}.md", p.slug),
            };
            let path = args.output.join(file);
            std::fs::write(&path, md)?;
            println!("wrote {}", path.display());
        }
    }

    // The repo-wiki sidebar is deterministic (no LLM), so it is written for both
    // dry-run and live generation, making the output a valid wiki checkout.
    if args.target == Target::Wiki {
        let path = args.output.join("_Sidebar.md");
        std::fs::write(&path, wiki_sidebar(&pages))
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("wrote {} (repo-wiki nav)", path.display());
    }

    // The Pages scaffold is deterministic (no LLM), so it is written for both
    // dry-run and live generation, making the output dir publishable as-is.
    if args.target == Target::Pages {
        let title = args
            .repo
            .canonicalize()
            .ok()
            .as_deref()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "Wiki".into());
        let path = args.output.join("_config.yml");
        std::fs::write(&path, pages_config(&title))
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("wrote {} (GitHub Pages config)", path.display());
    }

    // Serve the generated pages locally, rendering Markdown to HTML on request.
    if args.target == Target::Serve {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(glyphtrail_server::serve_wiki(
            args.output.clone(),
            args.port,
        ))?;
    }
    Ok(())
}

/// Minimal Jekyll config making the wiki output dir publishable via GitHub
/// Pages (the `index.md` home page is the generated overview).
fn pages_config(title: &str) -> String {
    format!(
        "title: {title}\ndescription: Documentation generated by glyphtrail from the code graph.\ntheme: jekyll-theme-cayman\n"
    )
}

/// GitHub-wiki page name for a page slug: the overview becomes `Home` (the wiki
/// landing page), every other slug is capitalised.
fn wiki_page_name(slug: &str) -> String {
    if slug == "index" {
        return "Home".into();
    }
    let mut chars = slug.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A `_Sidebar.md` linking every generated page with GitHub wiki link syntax.
fn wiki_sidebar(pages: &[Page]) -> String {
    let mut out = String::from("## Pages\n\n");
    for p in pages {
        out.push_str(&format!("- [[{}]]\n", wiki_page_name(p.slug)));
    }
    out
}

/// A wiki page: a slug, a system prompt, and a user prompt of graph facts.
struct Page {
    slug: &'static str,
    system: String,
    user: String,
}

const SYSTEM: &str = "You are a precise technical writer documenting a software repository. \
Using only the structured facts provided, write clear, well-structured GitHub-flavored \
Markdown. Do not invent files, symbols, or endpoints that are not in the facts. Be concise.";

fn build_pages(store: &dyn GraphStore) -> Result<Vec<Page>> {
    let stats = store.stats()?;
    let langs = stats
        .languages
        .iter()
        .map(|(l, n)| format!("{l}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");

    let overview = format!(
        "Repository facts:\n- files: {}\n- graph nodes: {}\n- graph edges: {}\n- languages: {}\n\n\
         Write an `Overview` page: a short paragraph on what the codebase contains, \
         a languages section, and an at-a-glance stats table.",
        stats.files, stats.nodes, stats.edges, langs
    );

    let endpoints = operations_facts(store, NodeKind::Endpoint, "Server endpoints", true)?;
    let clients = operations_facts(store, NodeKind::ClientCall, "Client call sites", false)?;
    let api = format!(
        "API surface facts:\n{endpoints}\n{clients}\n\n\
         Write an `API surface` page describing the service's HTTP endpoints and the \
         client call sites that consume APIs. If a section has no entries, say so briefly.",
    );

    Ok(vec![
        Page {
            slug: "index",
            system: SYSTEM.into(),
            user: overview,
        },
        Page {
            slug: "api",
            system: SYSTEM.into(),
            user: api,
        },
    ])
}

/// A bulleted fact list of operations of `kind`, with handlers for endpoints.
fn operations_facts(
    store: &dyn GraphStore,
    kind: NodeKind,
    title: &str,
    with_handler: bool,
) -> Result<String> {
    let mut lines = vec![format!("## {title}")];
    let ops = store.operations_by_kind(kind)?;
    if ops.is_empty() {
        lines.push("(none)".into());
    }
    for (id, key) in ops {
        let method = key
            .method
            .map(|m| m.as_str())
            .unwrap_or(key.protocol.as_str());
        let handler = if with_handler {
            store
                .neighbors(&id.0, Some(glyphtrail_core::EdgeKind::Handles), false)?
                .into_iter()
                .next()
                .map(|(n, _, _)| format!(" -> {}", n.qualified_name))
                .unwrap_or_default()
        } else {
            String::new()
        };
        lines.push(format!("- {method} {}{handler}", key.path));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_store::LadybugStore;

    #[test]
    fn pages_compose_from_graph_facts() {
        use glyphtrail_core::{Edge, Node, NodeId, Span};
        let mut store = LadybugStore::open_temp().unwrap();
        let n = Node {
            id: NodeId("f".into()),
            kind: NodeKind::Function,
            name: "main".into(),
            qualified_name: "main".into(),
            file: "src/main.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
            }),
            doc: None,
        };
        store.insert_graph(&[n], &[] as &[Edge]).unwrap();
        store.set_file("src/main.rs", Some("rust"), "h").unwrap();

        let pages = build_pages(&store).unwrap();
        check!(pages.len() == 2);
        check!(pages[0].slug == "index");
        // The Pages scaffold is a valid Jekyll config carrying the repo title.
        let cfg = pages_config("acme");
        check!(cfg.contains("title: acme"));
        check!(cfg.contains("theme: jekyll-theme-cayman"));
        check!(pages[0].user.contains("languages: rust: 1"));
        check!(pages[1].slug == "api");
        check!(pages[1].user.contains("Server endpoints"));
    }

    #[test]
    fn wiki_target_names_pages_and_sidebar() {
        // The overview becomes the wiki landing page; others are capitalised.
        check!(wiki_page_name("index") == "Home");
        check!(wiki_page_name("api") == "Api");
        let pages = vec![
            Page {
                slug: "index",
                system: String::new(),
                user: String::new(),
            },
            Page {
                slug: "api",
                system: String::new(),
                user: String::new(),
            },
        ];
        let sidebar = wiki_sidebar(&pages);
        check!(sidebar.contains("- [[Home]]"));
        check!(sidebar.contains("- [[Api]]"));
    }
}
