//! Express (JS/TS) REST route extraction.
//!
//! Routes are verb methods on an app or router value:
//! `app.get("/users/:id", handler)`, `router.post("/users", mw, handler)`, etc.
//! The verb is the property name; the path is the first string argument; the
//! handler is the last argument (a function or a handler reference).
//!
//! `app.get(...)` shares the `obj.get(...)` shape with an axios client call, so
//! a route is only recognized when the call has a string path *and* a trailing
//! function-like handler argument (an axios `.get(url)` / `.get(url, {config})`
//! has no handler), and the receiver is not `axios`.
//!
//! Router mounting (`app.use("/api", router)`) and prefix accumulation are a
//! follow-up; flat routes are extracted.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use super::ts::{span_of, text};
use super::{RawEndpoint, RawMount};
use crate::registry;

const VERBS: [&str; 7] = ["get", "post", "put", "delete", "patch", "head", "options"];

/// Extract Express routes from JS/TS/TSX `source`. Returns empty on parse
/// failure or for other languages.
pub fn extract_express(source: &str, lang: Language) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(ep) = route(n, src)
        {
            out.push(ep);
        }
    });
    out
}

/// Router mounting is a follow-up; no mounts are emitted.
pub fn extract_express_mounts(_source: &str, _lang: Language) -> Vec<RawMount> {
    Vec::new()
}

fn parse(source: &str, lang: Language) -> Option<Tree> {
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return None;
    }
    let mut parser = Parser::new();
    parser.set_language(&registry::grammar(lang)).ok()?;
    parser.parse(source, None)
}

/// An `app.VERB("/path", …handlers, handler)` call as a `RawEndpoint`, else
/// `None`.
fn route(call: Node, src: &[u8]) -> Option<RawEndpoint> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    // Exclude axios so its `.get`/`.post` client calls aren't read as routes.
    if text(func.child_by_field_name("object")?, src) == "axios" {
        return None;
    }
    let verb = text(func.child_by_field_name("property")?, src);
    if !VERBS.contains(&verb.as_str()) {
        return None;
    }
    let method = HttpMethod::parse(&verb)?;

    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let named: Vec<Node> = args.named_children(&mut cursor).collect();
    // A route needs a path *and* a handler; this is what separates it from an
    // axios `.get(url)` / `.get(url, {config})` client call.
    if named.len() < 2 {
        return None;
    }
    let path = js_url(named[0], src)?;
    let last = *named.last()?;
    if !is_handler(last) {
        return None;
    }
    Some(RawEndpoint {
        method,
        path,
        handler: handler_name(last, src),
        span: span_of(call),
    })
}

/// Whether a trailing argument looks like a request handler (function literal or
/// a reference to one), as opposed to a config object.
fn is_handler(node: Node) -> bool {
    matches!(
        node.kind(),
        "arrow_function"
            | "function"
            | "function_expression"
            | "identifier"
            | "member_expression"
            | "call_expression"
    )
}

/// Handler symbol name: a bare identifier, the property of `obj.handler`, or
/// empty for an inline function.
fn handler_name(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "identifier" => text(node, src),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|p| text(p, src))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// URL from a string or template-literal argument; `None` for dynamic URLs.
/// Template interpolations are kept verbatim (collapse to a dynamic segment).
fn js_url(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" => {
            let mut cursor = node.walk();
            let mut s = String::new();
            for ch in node.named_children(&mut cursor) {
                if ch.kind() == "string_fragment" {
                    s.push_str(&text(ch, src));
                }
            }
            Some(s)
        }
        "template_string" => Some(text(node, src).trim_matches('`').to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    const APP: &str = r#"
const app = express();
app.get("/users/:id", getUser);
app.post("/users", validate, createUser);
app.delete("/users/:id", (req, res) => res.end());
router.put("/items/:id", handlers.update);
app.use("/api", apiRouter);
"#;

    #[test]
    fn extracts_verbs_paths_and_handlers() {
        let eps = extract_express(APP, Language::JavaScript);
        assert_eq!(
            ep(&eps, HttpMethod::Get, "/users/:id").map(|e| e.handler.as_str()),
            Some("getUser")
        );
        // Last arg is the handler, after any middleware.
        assert_eq!(
            ep(&eps, HttpMethod::Post, "/users").map(|e| e.handler.as_str()),
            Some("createUser")
        );
        // Inline handler -> no symbol.
        assert_eq!(
            ep(&eps, HttpMethod::Delete, "/users/:id").map(|e| e.handler.as_str()),
            Some("")
        );
        // `obj.handler` reference keeps the function name; works on `router` too.
        assert_eq!(
            ep(&eps, HttpMethod::Put, "/items/:id").map(|e| e.handler.as_str()),
            Some("update")
        );
    }

    #[test]
    fn use_is_not_a_route() {
        let eps = extract_express(APP, Language::JavaScript);
        assert_eq!(eps.len(), 4);
    }

    #[test]
    fn axios_client_calls_are_not_routes() {
        // `.get(url)` and `.get(url, {config})` on axios have no handler arg.
        let src = "axios.get(\"/api/x\"); axios.get(\"/api/y\", { timeout: 5 });";
        assert!(extract_express(src, Language::TypeScript).is_empty());
    }

    #[test]
    fn works_in_tsx_and_skips_other_languages() {
        let src = "app.get(\"/ping\", pong);";
        assert!(
            ep(
                &extract_express(src, Language::Tsx),
                HttpMethod::Get,
                "/ping"
            )
            .is_some()
        );
        assert!(extract_express(src, Language::Rust).is_empty());
    }
}
