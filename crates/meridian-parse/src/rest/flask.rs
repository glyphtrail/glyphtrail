//! Flask / FastAPI (Python) REST route extraction.
//!
//! Routes are decorators on a handler function:
//! - FastAPI / Flask 2.0 verb shorthands `@app.get("/users/{id}")`,
//!   `@router.post("/users")`, …; and
//! - Flask `@app.route("/items", methods=["POST", "PUT"])` — one endpoint per
//!   listed method, defaulting to GET when `methods=` is absent.
//!
//! The decorated function's name is the handler. Router mounting / blueprint
//! prefixes are a follow-up; flat routes are extracted.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use super::ts::{span_of, text};
use super::{RawEndpoint, RawMount};
use crate::registry;

const VERBS: [&str; 6] = ["get", "post", "put", "delete", "patch", "head"];

/// Extract Flask/FastAPI routes. Returns empty on parse failure.
pub fn extract_flask(source: &str) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "decorated_definition" {
            collect(n, src, &mut out);
        }
    });
    out
}

/// Flask blueprints / mounting are a follow-up; no mounts are emitted.
pub fn extract_flask_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Python).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// Emit endpoints for every route decorator on a decorated function.
fn collect(node: Node, src: &[u8], out: &mut Vec<RawEndpoint>) {
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
        for method in decorator_methods(&verb, args, src) {
            out.push(RawEndpoint {
                method,
                path: path.clone(),
                handler: handler.clone(),
                span: span_of(call),
            });
        }
    }
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
    fn undecorated_functions_and_parse_failure() {
        // `plain` has no route decorator; nothing emitted for it.
        let eps = extract_flask(APP);
        check!(!eps.iter().any(|e| e.handler == "plain"));
        check!(extract_flask("def f(:::").is_empty());
        check!(extract_flask_mounts(APP).is_empty());
    }
}
