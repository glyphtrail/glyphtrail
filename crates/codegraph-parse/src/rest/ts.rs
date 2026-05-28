//! Framework-agnostic tree-sitter helpers shared by the REST extractors.

use std::collections::{HashMap, HashSet};

use codegraph_core::{normalize_path, HttpMethod, Span};
use tree_sitter::Node;

/// Visit `node` and all of its descendants, pre-order.
pub(super) fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}

/// The `i`-th named child of an `arguments` node, if present.
pub(super) fn named_arg<'a>(args: Option<Node<'a>>, i: usize) -> Option<Node<'a>> {
    let args = args?;
    let mut cursor = args.walk();
    let nth = args.named_children(&mut cursor).nth(i);
    nth
}

/// UTF-8 source text of a node (empty on invalid UTF-8).
pub(super) fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// The full callee text of a `call_expression`'s `function` child.
pub(super) fn func_name(func: Node, src: &[u8]) -> String {
    text(func, src)
}

/// Last `::`-separated segment of a path (e.g. `a::b::c` -> `c`).
pub(super) fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Inner text of a string literal, with quotes and any raw-string `r#"…"#`
/// delimiters removed. Works for plain (`"…"`) and raw (`r"…"`, `r#"…"#`)
/// literals by reading the grammar's `string_content` child.
pub(super) fn string_text(node: Node, src: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_content" {
            return text(child, src);
        }
    }
    // Fallback for unexpected shapes (e.g. an empty literal with no content
    // node): strip a leading `r`, matched `#`s, then the surrounding quotes.
    text(node, src)
        .trim()
        .trim_start_matches('r')
        .trim_matches('#')
        .trim_matches('"')
        .to_string()
}

/// Inner text of `node`, but only when it is a string literal (plain or raw).
/// Returns `None` for dynamic paths (idents, consts, `format!`/`concat!`, …),
/// which are out of scope and must not be emitted as literal routes.
pub(super) fn string_literal_text(node: Node, src: &[u8]) -> Option<String> {
    matches!(node.kind(), "string_literal" | "raw_string_literal").then(|| string_text(node, src))
}

/// Join an accumulated prefix with a path segment and normalize.
pub(super) fn join(prefix: &str, path: &str) -> String {
    let combined = format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    normalize_path(&combined)
}

/// Byte/line span of a node (1-based lines).
pub(super) fn span_of(node: Node) -> Span {
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

/// Ascend a node through `receiver.method(...)` chains to the outermost call.
pub(super) fn chain_root(mut n: Node) -> Node {
    while let Some(p) = n.parent() {
        if p.kind() == "field_expression"
            && p.child_by_field_name("value").map(|v| v.id()) == Some(n.id())
        {
            if let Some(gp) = p.parent() {
                if gp.kind() == "call_expression"
                    && gp.child_by_field_name("function").map(|f| f.id()) == Some(p.id())
                {
                    n = gp;
                    continue;
                }
            }
        }
        break;
    }
    n
}

/// The handler name from a call's arguments (first arg's last path segment).
pub(super) fn handler_name(args: Option<Node>, src: &[u8]) -> String {
    match named_arg(args, 0) {
        Some(n) if matches!(n.kind(), "identifier" | "scoped_identifier") => {
            last_segment(&text(n, src)).to_string()
        }
        _ => String::new(),
    }
}

/// Collect `(method, handler)` pairs from a `MethodRouter` expression such as
/// `get(list)` or `get(list).post(create)` (combinators like `.layer` are
/// transparently skipped by recursing into the receiver).
pub(super) fn method_router(node: Node, src: &[u8]) -> Vec<(HttpMethod, String)> {
    let mut out = Vec::new();
    collect_method_router(node, src, &mut out);
    out
}

fn collect_method_router(node: Node, src: &[u8], out: &mut Vec<(HttpMethod, String)>) {
    if node.kind() != "call_expression" {
        return;
    }
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    let args = node.child_by_field_name("arguments");
    if func.kind() == "field_expression" {
        if let Some(recv) = func.child_by_field_name("value") {
            collect_method_router(recv, src, out);
        }
        let verb = func
            .child_by_field_name("field")
            .map(|n| text(n, src))
            .unwrap_or_default();
        if let Some(m) = HttpMethod::parse(&verb) {
            out.push((m, handler_name(args, src)));
        }
    } else {
        let verb = func_name(func, src);
        if let Some(m) = HttpMethod::parse(last_segment(&verb)) {
            out.push((m, handler_name(args, src)));
        }
    }
}

/// The topmost `call_expression` of every method chain whose base is a scoped
/// constructor on `type_name` — e.g. `Router::new`, `OpenApiRouter::new`, or
/// `OpenApiRouter::with_openapi` (any associated fn, possibly path-qualified
/// like `axum::Router::new`).
pub(super) fn router_chain_roots<'a>(root: Node<'a>, src: &[u8], type_name: &str) -> Vec<Node<'a>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    walk(root, &mut |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let is_ctor = n
            .child_by_field_name("function")
            .map(|f| f.kind() == "scoped_identifier" && constructs(&text(f, src), type_name))
            .unwrap_or(false);
        if !is_ctor {
            return;
        }
        let cr = chain_root(n);
        if seen.insert(cr.id()) {
            roots.push(cr);
        }
    });
    roots
}

/// Whether `callee` (e.g. `OpenApiRouter::with_openapi`) is an associated fn of
/// `type_name`, i.e. the segment just before the final `::method` matches. A
/// turbofish (`Router::<State>::new`) is tolerated by dropping the `<...>`.
fn constructs(callee: &str, type_name: &str) -> bool {
    let Some((ty, _method)) = callee.rsplit_once("::") else {
        return false;
    };
    let ty = ty.split('<').next().unwrap_or(ty).trim_end_matches(':');
    last_segment(ty) == type_name
}

/// Index `fn NAME(...) -> Ret { ... }` builders whose return type mentions
/// `type_name`, keyed by name -> the router chain root of the function body.
pub(super) fn collect_builders<'a>(
    root: Node<'a>,
    src: &[u8],
    type_name: &str,
) -> HashMap<String, Node<'a>> {
    let mut builders = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "function_item" {
            return;
        }
        let returns_router = n
            .child_by_field_name("return_type")
            .map(|rt| text(rt, src).contains(type_name))
            .unwrap_or(false);
        if !returns_router {
            return;
        }
        let (Some(name), Some(body)) = (
            n.child_by_field_name("name").map(|x| text(x, src)),
            n.child_by_field_name("body"),
        ) else {
            return;
        };
        // The returned router is the body's top-level chain (not one nested as
        // an argument of another chain); take the last such in source order.
        let chain = router_chain_roots(body, src, type_name)
            .into_iter()
            .rfind(|c| c.parent().map(|p| p.kind()) != Some("arguments"));
        if let Some(chain) = chain {
            builders.insert(name, chain);
        }
    });
    builders
}

/// Names of builder fns referenced by a `.nest`/`.nest_service`/`.merge`
/// argument (so they are emitted through their parent, not as roots).
pub(super) fn collect_referenced_builders(
    root: Node,
    src: &[u8],
    builders: &HashMap<String, Node>,
) -> HashSet<String> {
    let mut referenced = HashSet::new();
    walk(root, &mut |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let Some(func) = n.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "field_expression" {
            return;
        }
        let method = func
            .child_by_field_name("field")
            .map(|x| text(x, src))
            .unwrap_or_default();
        if method != "nest" && method != "merge" && method != "nest_service" {
            return;
        }
        let args = n.child_by_field_name("arguments");
        for i in 0..2 {
            if let Some(a) = named_arg(args, i) {
                let name = arg_ref_name(a, src);
                let short = last_segment(&name);
                if builders.contains_key(short) {
                    referenced.insert(short.to_string());
                }
            }
        }
    });
    referenced
}

/// The referenced name of a `.nest`/`.merge` argument: the callee of a builder
/// call (`users_router()`), or a bare router identifier (`users_router`).
fn arg_ref_name(arg: Node, src: &[u8]) -> String {
    match arg.kind() {
        "call_expression" => arg
            .child_by_field_name("function")
            .map(|f| func_name(f, src))
            .unwrap_or_default(),
        "identifier" | "scoped_identifier" => text(arg, src),
        _ => String::new(),
    }
}

/// `(parent builder, child builder)` mount relationships: a `.nest`/`.merge`
/// within builder `F`'s composed router chain that targets another builder `G`
/// yields `(F, G)`. Both ends are same-file builders, so they resolve to local
/// `Function` nodes.
///
/// Collection is constrained to each builder's stored chain root (the actually
/// returned/composed router, including inline nested routers) rather than the
/// whole function body, so unused local routers don't produce phantom mounts.
pub(super) fn collect_mounts(
    src: &[u8],
    builders: &HashMap<String, Node>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (parent, chain_root) in builders {
        walk(*chain_root, &mut |n| {
            if n.kind() != "call_expression" {
                return;
            }
            let Some(func) = n.child_by_field_name("function") else {
                return;
            };
            if func.kind() != "field_expression" {
                return;
            }
            let method = func
                .child_by_field_name("field")
                .map(|x| text(x, src))
                .unwrap_or_default();
            if method != "nest" && method != "merge" && method != "nest_service" {
                return;
            }
            let args = n.child_by_field_name("arguments");
            for i in 0..2 {
                if let Some(a) = named_arg(args, i) {
                    let name = arg_ref_name(a, src);
                    let short = last_segment(&name);
                    // Skip self-references; only same-file builders are mounts.
                    if short != parent && builders.contains_key(short) {
                        out.push((parent.clone(), short.to_string()));
                    }
                }
            }
        });
    }
    out
}
