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
//! Router mounting (`app.use("/api", router)`) emits a `MOUNTS` edge between
//! `Router` nodes via [`extract_express_router_mounts`], and routes declared on
//! a mounted router are prefixed with its mount path (#166).

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use std::collections::HashMap;

use super::ts::{join, span_of, text};
use super::{RawEndpoint, RawMount, RawRouterMount};
use crate::registry;

const VERBS: [&str; 7] = ["get", "post", "put", "delete", "patch", "head", "options"];

/// Extract Express routes from JS/TS/TSX `source`. Returns empty on parse
/// failure or for other languages.
pub fn extract_express(source: &str, lang: &Language) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    // Mounted router variable -> its mount prefix, so a route declared on a
    // router that `app.use("/api", router)` mounts is prefixed `/api` (#166).
    let prefixes = mount_prefixes(tree.root_node(), src);
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(ep) = route(n, src, &prefixes)
        {
            out.push(ep);
        }
    });
    out
}

/// Map each mounted router variable to the prefix it is mounted under, from
/// `app.use("/api", router)` calls. One level deep (no transitive chains).
fn mount_prefixes(root: Node, src: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    super::ts::walk(root, &mut |n| {
        if n.kind() == "call_expression"
            && let Some(m) = router_mount(n, src)
        {
            map.insert(m.mounted, m.prefix);
        }
    });
    map
}

/// Express composes via router variables, not function builders, so no builder
/// mounts are emitted; see [`extract_express_router_mounts`].
pub fn extract_express_mounts(_source: &str, _lang: &Language) -> Vec<RawMount> {
    Vec::new()
}

/// Router-variable mounts: `app.use("/api", apiRouter)` mounts `apiRouter` under
/// `app` at `/api` (#128). Recognized as a `.use(<string path>, …, <identifier>)`
/// call: a string path argument and a trailing bare-identifier router. This
/// shape excludes middleware forms (`app.use(express.json())`, `app.use(cors())`)
/// whose trailing argument is a call, not an identifier.
pub fn extract_express_router_mounts(source: &str, lang: &Language) -> Vec<RawRouterMount> {
    let Some(tree) = parse(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(m) = router_mount(n, src)
        {
            out.push(m);
        }
    });
    out
}

/// An `app.use("/path", router)` call as a [`RawRouterMount`], else `None`.
fn router_mount(call: Node, src: &[u8]) -> Option<RawRouterMount> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let host = text(func.child_by_field_name("object")?, src);
    if host == "axios" || text(func.child_by_field_name("property")?, src) != "use" {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let named: Vec<Node> = args.named_children(&mut cursor).collect();
    if named.len() < 2 {
        return None;
    }
    let prefix = js_url(named[0], src)?;
    let mounted_node = *named.last()?;
    if mounted_node.kind() != "identifier" {
        return None;
    }
    Some(RawRouterMount {
        host,
        mounted: text(mounted_node, src),
        prefix,
        span: span_of(call),
    })
}

fn parse(source: &str, lang: &Language) -> Option<Tree> {
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return None;
    }
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(lang).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// An `app.VERB("/path", …handlers, handler)` call as a `RawEndpoint`, else
/// `None`. The route's path is prefixed with the receiver router's mount prefix
/// (`prefixes`) when it is a router that `app.use(...)` mounts under a path.
fn route(call: Node, src: &[u8], prefixes: &HashMap<String, String>) -> Option<RawEndpoint> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    // Exclude axios so its `.get`/`.post` client calls aren't read as routes.
    let receiver = text(func.child_by_field_name("object")?, src);
    if receiver == "axios" {
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
    let full = match prefixes.get(&receiver) {
        Some(prefix) => join(prefix, &path),
        None => path,
    };
    Some(RawEndpoint {
        method,
        path: full,
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
    use assert2::check;

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
        let eps = extract_express(APP, &Language::JavaScript);
        check!(
            ep(&eps, HttpMethod::Get, "/users/:id").map(|e| e.handler.as_str()) == Some("getUser")
        );
        // Last arg is the handler, after any middleware.
        check!(
            ep(&eps, HttpMethod::Post, "/users").map(|e| e.handler.as_str()) == Some("createUser")
        );
        // Inline handler -> no symbol.
        check!(ep(&eps, HttpMethod::Delete, "/users/:id").map(|e| e.handler.as_str()) == Some(""));
        // `obj.handler` reference keeps the function name; works on `router` too.
        check!(
            ep(&eps, HttpMethod::Put, "/items/:id").map(|e| e.handler.as_str()) == Some("update")
        );
    }

    #[test]
    fn use_is_not_a_route() {
        let eps = extract_express(APP, &Language::JavaScript);
        check!(eps.len() == 4);
    }

    #[test]
    fn routes_on_a_mounted_router_get_the_mount_prefix() {
        let src = r#"
const app = express();
const apiRouter = express.Router();
apiRouter.get("/users/:id", getUser);
apiRouter.post("/users", createUser);
app.get("/health", health);
app.use("/api", apiRouter);
"#;
        let eps = extract_express(src, &Language::JavaScript);
        // Routes on apiRouter are prefixed with the /api mount path (#166). The
        // prefix join also normalizes the param (`:id` -> `{id}`); the matcher
        // collapses both spellings to one signature regardless.
        check!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}").map(|e| e.handler.as_str())
                == Some("getUser")
        );
        check!(
            ep(&eps, HttpMethod::Post, "/api/users").map(|e| e.handler.as_str())
                == Some("createUser")
        );
        // A route on `app` itself is unprefixed.
        check!(ep(&eps, HttpMethod::Get, "/health").map(|e| e.handler.as_str()) == Some("health"));
    }

    #[test]
    fn use_with_path_and_router_is_a_router_mount() {
        let mounts = extract_express_router_mounts(APP, &Language::JavaScript);
        check!(mounts.len() == 1);
        check!(mounts[0].host == "app");
        check!(mounts[0].mounted == "apiRouter");
        check!(mounts[0].prefix == "/api");
    }

    #[test]
    fn middleware_use_is_not_a_router_mount() {
        // Trailing call argument (middleware), not a router identifier.
        let src = "const app = express(); app.use(express.json()); app.use(cors());";
        check!(extract_express_router_mounts(src, &Language::JavaScript).is_empty());
    }

    #[test]
    fn axios_client_calls_are_not_routes() {
        // `.get(url)` and `.get(url, {config})` on axios have no handler arg.
        let src = "axios.get(\"/api/x\"); axios.get(\"/api/y\", { timeout: 5 });";
        check!(extract_express(src, &Language::TypeScript).is_empty());
    }

    #[test]
    fn works_in_tsx_and_skips_other_languages() {
        let src = "app.get(\"/ping\", pong);";
        check!(
            ep(
                &extract_express(src, &Language::Tsx),
                HttpMethod::Get,
                "/ping"
            )
            .is_some()
        );
        check!(extract_express(src, &Language::Rust).is_empty());
    }
}
