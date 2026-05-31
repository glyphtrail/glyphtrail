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

use glyphtrail_core::{HttpMethod, Language};

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
    super::ts::walk(tree.root_node(), &mut |n| match n.kind() {
        "decorated_definition" => collect(n, src, &prefixes, &mut out),
        "call" => collect_add_url_rule(n, src, &prefixes, &mut out),
        _ => {}
    });
    out
}

/// `app.add_url_rule("/x", view_func=fn, methods=[...])` — the imperative
/// route registration. The receiver's accumulated prefix applies (e.g. when
/// called on a blueprint variable); methods default to GET.
fn collect_add_url_rule(
    n: Node,
    src: &[u8],
    prefixes: &HashMap<String, String>,
    out: &mut Vec<RawEndpoint>,
) {
    if attr_name(n, src).as_deref() != Some("add_url_rule") {
        return;
    }
    let args = n.child_by_field_name("arguments");
    let Some(path) = first_string_arg(args, src) else {
        return;
    };
    let handler = ident_kwarg(args, "view_func", src).unwrap_or_default();
    let prefix = n
        .child_by_field_name("function")
        .and_then(|f| f.child_by_field_name("object"))
        .map(|o| text(o, src))
        .and_then(|recv| prefixes.get(&recv).cloned())
        .unwrap_or_default();
    let full = join(&prefix, &path);
    let methods = methods_kwarg(args, src);
    let methods = if methods.is_empty() {
        vec![HttpMethod::Get]
    } else {
        methods
    };
    for method in methods {
        out.push(RawEndpoint {
            method,
            path: full.clone(),
            handler: handler.clone(),
            span: span_of(n),
        });
    }
}

/// The identifier value of a `name=ident` keyword argument (e.g. `view_func=fn`).
fn ident_kwarg(args: Option<Node>, name: &str, src: &[u8]) -> Option<String> {
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
                .filter(|v| v.kind() == "identifier")
                .map(|v| text(v, src))
        })
        .flatten()
    })
}

/// The attribute names that mount a router/blueprint variable under a host,
/// with the keyword carrying the mount prefix: FastAPI `include_router(…,
/// prefix=)` and Flask `register_blueprint(…, url_prefix=)`.
const MOUNT_CALLS: [&str; 2] = ["include_router", "register_blueprint"];

/// Map each router/blueprint variable to its accumulated path prefix: the
/// `APIRouter(prefix=…)` / `Blueprint(url_prefix=…)` declared prefix, prepended
/// by any `include_router` / `register_blueprint` mount prefix.
fn router_prefixes(root: Node, src: &[u8]) -> HashMap<String, String> {
    let mut own: HashMap<String, String> = HashMap::new();
    let mut mounted: HashMap<String, String> = HashMap::new();
    super::ts::walk(root, &mut |n| match n.kind() {
        "assignment" => collect_router_decl(n, src, &mut own),
        "call" => collect_mount(n, src, &mut mounted),
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

/// Record a `router = APIRouter(prefix="/x")` or `bp = Blueprint(..,
/// url_prefix="/x")` declaration and its own prefix.
fn collect_router_decl(n: Node, src: &[u8], own: &mut HashMap<String, String>) {
    let Some(var) = n.child_by_field_name("left").map(|l| text(l, src)) else {
        return;
    };
    let Some(rhs) = n
        .child_by_field_name("right")
        .filter(|r| r.kind() == "call")
    else {
        return;
    };
    let kwarg = if is_call_to(rhs, "APIRouter", src) {
        "prefix"
    } else if is_call_to(rhs, "Blueprint", src) {
        "url_prefix"
    } else {
        return;
    };
    let p = string_kwarg(rhs.child_by_field_name("arguments"), kwarg, src).unwrap_or_default();
    own.insert(var, p);
}

/// Record an `app.include_router(r, prefix="/y")` / `app.register_blueprint(bp,
/// url_prefix="/y")` mount and the prefix it imposes on the mounted variable.
fn collect_mount(n: Node, src: &[u8], mounted: &mut HashMap<String, String>) {
    if MOUNT_CALLS.contains(&attr_name(n, src).as_deref().unwrap_or(""))
        && let Some(child) = first_positional_ident(n.child_by_field_name("arguments"), src)
    {
        let args = n.child_by_field_name("arguments");
        let p = mount_prefix(args, src);
        mounted.insert(child, p);
    }
}

/// The mount prefix from an `include_router`/`register_blueprint` call: FastAPI
/// uses `prefix=`, Flask uses `url_prefix=`.
fn mount_prefix(args: Option<Node>, src: &[u8]) -> String {
    string_kwarg(args, "prefix", src)
        .or_else(|| string_kwarg(args, "url_prefix", src))
        .unwrap_or_default()
}

/// Flask builder mounting is not used; routers/blueprints compose via variables,
/// emitted by [`extract_flask_router_mounts`].
pub fn extract_flask_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

/// Router-variable mounts: `app.include_router(r, prefix="/y")` (FastAPI) and
/// `app.register_blueprint(bp, url_prefix="/y")` (Flask) mount the variable
/// under the receiver (#128/#89). The receiver is the host, the first positional
/// identifier is the mounted router/blueprint.
pub fn extract_flask_router_mounts(source: &str) -> Vec<RawRouterMount> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call"
            && MOUNT_CALLS.contains(&attr_name(n, src).as_deref().unwrap_or(""))
            && let Some(host) = n
                .child_by_field_name("function")
                .and_then(|f| f.child_by_field_name("object"))
                .map(|o| text(o, src))
            && let Some(mounted) = first_positional_ident(n.child_by_field_name("arguments"), src)
        {
            out.push(RawRouterMount {
                host,
                mounted,
                prefix: mount_prefix(n.child_by_field_name("arguments"), src),
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
    fn flask_blueprint_prefix_register_and_mount() {
        let src = r#"
bp = Blueprint("users", __name__, url_prefix="/users")

@bp.route("/<int:id>", methods=["GET", "DELETE"])
def get_user(id): ...

app.register_blueprint(bp, url_prefix="/api")
"#;
        let eps = extract_flask(src);
        // /api (register_blueprint) + /users (Blueprint) + /{id} (route, with the
        // `<int:id>` Flask converter normalized to `{id}`).
        check!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}").map(|e| e.handler.as_str())
                == Some("get_user")
        );
        check!(ep(&eps, HttpMethod::Delete, "/api/users/{id}").is_some());
        // register_blueprint emits a router-variable MOUNTS pair.
        let mounts = extract_flask_router_mounts(src);
        check!(
            mounts
                .iter()
                .any(|m| m.host == "app" && m.mounted == "bp" && m.prefix == "/api")
        );
    }

    #[test]
    fn flask_add_url_rule_imperative_route() {
        let src = r#"
def home(): ...
app.add_url_rule("/home", view_func=home, methods=["GET", "POST"])
app.add_url_rule("/ping", view_func=ping)
"#;
        let eps = extract_flask(src);
        check!(ep(&eps, HttpMethod::Get, "/home").map(|e| e.handler.as_str()) == Some("home"));
        check!(ep(&eps, HttpMethod::Post, "/home").is_some());
        // Methods default to GET when omitted.
        check!(ep(&eps, HttpMethod::Get, "/ping").map(|e| e.handler.as_str()) == Some("ping"));
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
