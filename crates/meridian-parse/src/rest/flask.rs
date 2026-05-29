//! Flask / FastAPI (Python) REST route extraction.
//!
//! Routes are decorators on a handler function:
//! - FastAPI / Flask 2.0 verb shorthands `@app.get("/users/{id}")`,
//!   `@router.post("/users")`, …; and
//! - Flask `@app.route("/items", methods=["POST", "PUT"])` — one endpoint per
//!   listed method, defaulting to GET when `methods=` is absent.
//!
//! The decorated function's name is the handler. FastAPI prefixes accumulate:
//! a route on a `router = APIRouter(prefix="/x")` is prefixed with `/x`, and
//! `app.include_router(router, prefix="/y")` further prefixes it with `/y`, so
//! `@router.get("/{id}")` becomes `GET /y/x/{id}`. `include_router` also emits a
//! `MOUNTS` edge between `Router` nodes via [`extract_flask_router_mounts`].

use std::collections::HashMap;

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use super::ts::{join, span_of, text};
use super::{RawEndpoint, RawMount, RawRouterMount};
use crate::registry;

const VERBS: [&str; 6] = ["get", "post", "put", "delete", "patch", "head"];

/// Extract Flask/FastAPI routes. Returns empty on parse failure.
pub fn extract_flask(source: &str) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let prefixes = router_prefixes(tree.root_node(), src);
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "decorated_definition" {
            collect(n, src, &prefixes, &mut out);
        }
    });
    out
}

/// Map each FastAPI router variable to its accumulated path prefix: the
/// `APIRouter(prefix=…)` declared prefix, prepended by any
/// `include_router(router, prefix=…)` mount prefix.
fn router_prefixes(root: Node, src: &[u8]) -> HashMap<String, String> {
    let mut own: HashMap<String, String> = HashMap::new();
    let mut mounted: HashMap<String, String> = HashMap::new();
    super::ts::walk(root, &mut |n| match n.kind() {
        "assignment" => collect_apirouter(n, src, &mut own),
        "call" => collect_include_router(n, src, &mut mounted),
        _ => {}
    });

    own.keys()
        .chain(mounted.keys())
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|var| {
            let o = own.get(&var).map(String::as_str).unwrap_or("");
            let m = mounted.get(&var).map(String::as_str).unwrap_or("");
            (var, join(m, o))
        })
        .collect()
}

/// Record a `router = APIRouter(prefix="/x")` declaration.
fn collect_apirouter(n: Node, src: &[u8], own: &mut HashMap<String, String>) {
    if let Some(var) = n.child_by_field_name("left").map(|l| text(l, src))
        && let Some(rhs) = n
            .child_by_field_name("right")
            .filter(|r| r.kind() == "call")
        && is_call_to(rhs, "APIRouter", src)
    {
        let p =
            string_kwarg(rhs.child_by_field_name("arguments"), "prefix", src).unwrap_or_default();
        own.insert(var, p);
    }
}

/// Record an `app.include_router(router, prefix="/y")` mount.
fn collect_include_router(n: Node, src: &[u8], mounted: &mut HashMap<String, String>) {
    if attr_name(n, src).as_deref() == Some("include_router")
        && let Some(child) = first_positional_ident(n.child_by_field_name("arguments"), src)
    {
        let p = string_kwarg(n.child_by_field_name("arguments"), "prefix", src).unwrap_or_default();
        mounted.insert(child, p);
    }
}

/// Flask blueprints / builder mounting are a follow-up; no builder mounts are
/// emitted. Router-variable mounts (`include_router`) are handled separately by
/// [`extract_flask_router_mounts`].
pub fn extract_flask_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

/// Router-variable mounts: `app.include_router(router, prefix="/y")` mounts the
/// `router` variable under `app` at `/y` (#128). The receiver is the host
/// router, the first positional identifier is the mounted router.
pub fn extract_flask_router_mounts(source: &str) -> Vec<RawRouterMount> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call"
            && attr_name(n, src).as_deref() == Some("include_router")
            && let Some(host) = n
                .child_by_field_name("function")
                .and_then(|f| f.child_by_field_name("object"))
                .map(|o| text(o, src))
            && let Some(mounted) = first_positional_ident(n.child_by_field_name("arguments"), src)
        {
            let prefix =
                string_kwarg(n.child_by_field_name("arguments"), "prefix", src).unwrap_or_default();
            out.push(RawRouterMount {
                host,
                mounted,
                prefix,
                span: span_of(n),
            });
        }
    });
    out
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Python).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// Emit endpoints for every route decorator on a decorated function. The
/// decorator's receiver (`@router.get`) selects the accumulated prefix.
fn collect(node: Node, src: &[u8], prefixes: &HashMap<String, String>, out: &mut Vec<RawEndpoint>) {
    let Some(handler) = node
        .child_by_field_name("definition")
        .filter(|d| d.kind() == "function_definition")
        .and_then(|d| d.child_by_field_name("name"))
        .map(|n| text(n, src))
    else {
        return;
    };

    let mut cursor = node.walk();
    for dec in node.named_children(&mut cursor) {
        if dec.kind() != "decorator" {
            continue;
        }
        let Some(call) = decorator_call(dec) else {
            continue;
        };
        let Some(func) = call.child_by_field_name("function") else {
            continue;
        };
        if func.kind() != "attribute" {
            continue;
        }
        let Some(verb) = func.child_by_field_name("attribute").map(|a| text(a, src)) else {
            continue;
        };
        let args = call.child_by_field_name("arguments");
        let Some(path) = first_string_arg(args, src) else {
            continue;
        };
        // Prefix by the receiver router's accumulated prefix, if any.
        let prefix = func
            .child_by_field_name("object")
            .map(|o| text(o, src))
            .and_then(|recv| prefixes.get(&recv).cloned())
            .unwrap_or_default();
        let full = join(&prefix, &path);
        for method in decorator_methods(&verb, args, src) {
            out.push(RawEndpoint {
                method,
                path: full.clone(),
                handler: handler.clone(),
                span: span_of(call),
            });
        }
    }
}

/// The attribute name of a `recv.attr(...)` call (e.g. `include_router`), else `None`.
fn attr_name(call: Node, src: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    (func.kind() == "attribute")
        .then(|| func.child_by_field_name("attribute").map(|a| text(a, src)))
        .flatten()
}

/// Whether `call`'s function is `name(...)` or `pkg.name(...)`.
fn is_call_to(call: Node, name: &str, src: &[u8]) -> bool {
    let Some(func) = call.child_by_field_name("function") else {
        return false;
    };
    match func.kind() {
        "identifier" => text(func, src) == name,
        "attribute" => {
            func.child_by_field_name("attribute")
                .map(|a| text(a, src))
                .as_deref()
                == Some(name)
        }
        _ => false,
    }
}

/// The first positional argument when it is a bare identifier (the child router
/// in `include_router(router, …)`).
fn first_positional_ident(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find(|a| a.kind() == "identifier")
        .map(|a| text(a, src))
}

/// String value of a `name="..."` keyword argument.
fn string_kwarg(args: Option<Node>, name: &str, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor).find_map(|arg| {
        (arg.kind() == "keyword_argument"
            && arg
                .child_by_field_name("name")
                .map(|n| text(n, src))
                .as_deref()
                == Some(name))
        .then(|| {
            arg.child_by_field_name("value")
                .and_then(|v| py_string(v, src))
        })
        .flatten()
    })
}

/// The `call` inside a decorator (`@app.get(...)`), if the decorator is a call.
fn decorator_call(decorator: Node) -> Option<Node> {
    let mut cursor = decorator.walk();
    decorator
        .named_children(&mut cursor)
        .find(|c| c.kind() == "call")
}

/// The HTTP method(s) a route decorator declares: a verb shorthand yields that
/// method; `route` yields the `methods=[…]` list, or GET by default.
fn decorator_methods(verb: &str, args: Option<Node>, src: &[u8]) -> Vec<HttpMethod> {
    if let Some(m) = HttpMethod::parse(verb).filter(|_| VERBS.contains(&verb)) {
        return vec![m];
    }
    if verb != "route" {
        return Vec::new();
    }
    let listed = methods_kwarg(args, src);
    if listed.is_empty() {
        vec![HttpMethod::Get]
    } else {
        listed
    }
}

/// Methods from a `methods=["GET", "POST"]` keyword argument.
fn methods_kwarg(args: Option<Node>, src: &[u8]) -> Vec<HttpMethod> {
    let Some(args) = args else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        if arg.kind() == "keyword_argument"
            && arg
                .child_by_field_name("name")
                .map(|n| text(n, src))
                .as_deref()
                == Some("methods")
            && let Some(list) = arg
                .child_by_field_name("value")
                .filter(|v| v.kind() == "list")
        {
            let mut lc = list.walk();
            return list
                .named_children(&mut lc)
                .filter_map(|s| py_string(s, src))
                .filter_map(|s| HttpMethod::parse(&s))
                .collect();
        }
    }
    Vec::new()
}

/// First positional string-literal argument's text.
fn first_string_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find_map(|a| py_string(a, src))
}

/// Inner text of a Python `string` literal, else `None`.
fn py_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    Some(
        node.named_children(&mut cursor)
            .find(|c| c.kind() == "string_content")
            .map(|c| text(c, src))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    const APP: &str = r#"
@app.get("/users/{id}")
def get_user(id): ...

@router.post("/users")
def create_user(): ...

@app.route("/items", methods=["POST", "PUT"])
def items(): ...

@app.route("/health")
def health(): ...

def plain(): ...
"#;

    #[test]
    fn fastapi_verb_decorators() {
        let eps = extract_flask(APP);
        check!(
            ep(&eps, HttpMethod::Get, "/users/{id}").map(|e| e.handler.as_str())
                == Some("get_user")
        );
        check!(
            ep(&eps, HttpMethod::Post, "/users").map(|e| e.handler.as_str()) == Some("create_user")
        );
    }

    #[test]
    fn flask_route_methods_and_default() {
        let eps = extract_flask(APP);
        // One endpoint per listed method, same handler.
        check!(ep(&eps, HttpMethod::Post, "/items").map(|e| e.handler.as_str()) == Some("items"));
        check!(ep(&eps, HttpMethod::Put, "/items").is_some());
        // `route` without `methods=` defaults to GET.
        check!(ep(&eps, HttpMethod::Get, "/health").map(|e| e.handler.as_str()) == Some("health"));
    }

    #[test]
    fn fastapi_router_prefix_accumulates() {
        let src = r#"
router = APIRouter(prefix="/users")

@router.get("/{id}")
def get_user(id): ...

app.include_router(router, prefix="/api")
"#;
        let eps = extract_flask(src);
        // /api (include) + /users (APIRouter) + /{id} (route)
        check!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}").map(|e| e.handler.as_str())
                == Some("get_user")
        );
    }

    #[test]
    fn fastapi_apirouter_prefix_without_include() {
        let src = r#"
router = APIRouter(prefix="/v1")

@router.post("/items")
def create(): ...
"#;
        let eps = extract_flask(src);
        check!(
            ep(&eps, HttpMethod::Post, "/v1/items").map(|e| e.handler.as_str()) == Some("create")
        );
    }

    #[test]
    fn include_router_yields_router_mount() {
        let src = r#"
router = APIRouter(prefix="/users")
app.include_router(router, prefix="/api")
"#;
        let m = extract_flask_router_mounts(src);
        check!(m.len() == 1);
        check!(m[0].host == "app");
        check!(m[0].mounted == "router");
        check!(m[0].prefix == "/api");
        // No include_router in the decorator-only fixture.
        check!(extract_flask_router_mounts(APP).is_empty());
    }

    #[test]
    fn undecorated_functions_and_parse_failure() {
        // `plain` has no route decorator; nothing emitted for it.
        let eps = extract_flask(APP);
        check!(!eps.iter().any(|e| e.handler == "plain"));
        check!(extract_flask("def f(:::").is_empty());
        check!(extract_flask_mounts(APP).is_empty());
    }
}
