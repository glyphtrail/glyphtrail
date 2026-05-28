//! ASP.NET Core (C#) REST route extraction.
//!
//! Two styles:
//! - **Attribute controllers**: `[HttpGet("{id}")]` / `[HttpPost]` on a method,
//!   with an optional class-level `[Route("api/users")]` base prefixed onto each
//!   route. The `[controller]` route token expands to the class name minus its
//!   `Controller` suffix. The handler is the method name.
//! - **Minimal APIs**: `app.MapGet("/health", handler)` / `app.MapPost(...)`.
//!   The handler is the symbol after the path (empty for an inline lambda).
//!
//! Route grouping (`MapGroup`) is a follow-up; flat routes are extracted.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use super::ts::{join, span_of, text};
use super::{RawEndpoint, RawMount};
use crate::registry;

/// Extract ASP.NET routes. Returns empty on parse failure.
pub fn extract_aspnet(source: &str) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| match n.kind() {
        "class_declaration" => controller_routes(n, src, &mut out),
        "invocation_expression" => {
            if let Some(ep) = minimal_api(n, src) {
                out.push(ep);
            }
        }
        _ => {}
    });
    out
}

/// ASP.NET has no in-code router-builder composition we model; no mounts.
pub fn extract_aspnet_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::CSharp).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// Endpoints from a `[Http*]`-annotated controller, prefixed by the class
/// `[Route(...)]` base.
fn controller_routes(class: Node, src: &[u8], out: &mut Vec<RawEndpoint>) {
    let class_name = class
        .child_by_field_name("name")
        .map(|n| text(n, src))
        .unwrap_or_default();
    let base = class_base_route(class, src, &class_name);

    let Some(body) = class.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for method in body.named_children(&mut cursor) {
        if method.kind() != "method_declaration" {
            continue;
        }
        let handler = method
            .child_by_field_name("name")
            .map(|n| text(n, src))
            .unwrap_or_default();
        for attr in attributes(method) {
            if let Some(verb) = http_verb(&attribute_name(attr, src)) {
                let path = attribute_string(attr, src).unwrap_or_default();
                out.push(RawEndpoint {
                    method: verb,
                    path: join(&base, &path),
                    handler: handler.clone(),
                    span: span_of(method),
                });
            }
        }
    }
}

/// Class-level `[Route("...")]` base path, with `[controller]` expanded to the
/// class name minus a trailing `Controller`.
fn class_base_route(class: Node, src: &[u8], class_name: &str) -> String {
    for attr in attributes(class) {
        if attribute_name(attr, src) == "Route"
            && let Some(route) = attribute_string(attr, src)
        {
            let controller = class_name.strip_suffix("Controller").unwrap_or(class_name);
            return route.replace("[controller]", controller);
        }
    }
    String::new()
}

/// A Minimal-API `app.MapGet("/path", handler)` call as a `RawEndpoint`.
fn minimal_api(call: Node, src: &[u8]) -> Option<RawEndpoint> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_access_expression" {
        return None;
    }
    let verb = map_verb(&text(func.child_by_field_name("name")?, src))?;
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut arg_nodes = args
        .named_children(&mut cursor)
        .filter(|a| a.kind() == "argument");
    let path = cs_string(first_child(arg_nodes.next()?)?, src)?;
    let handler = arg_nodes
        .next()
        .and_then(first_child)
        .map(|h| handler_name(h, src))
        .unwrap_or_default();
    Some(RawEndpoint {
        method: verb,
        path,
        handler,
        span: span_of(call),
    })
}

/// Handler symbol from a Minimal-API argument: an identifier, the member of
/// `Type.Method`, or empty for an inline lambda.
fn handler_name(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "identifier" => text(node, src),
        "member_access_expression" => node
            .child_by_field_name("name")
            .map(|n| text(n, src))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// `Http<Verb>` attribute name -> method.
fn http_verb(name: &str) -> Option<HttpMethod> {
    match name {
        "HttpGet" => Some(HttpMethod::Get),
        "HttpPost" => Some(HttpMethod::Post),
        "HttpPut" => Some(HttpMethod::Put),
        "HttpDelete" => Some(HttpMethod::Delete),
        "HttpPatch" => Some(HttpMethod::Patch),
        "HttpHead" => Some(HttpMethod::Head),
        _ => None,
    }
}

/// `Map<Verb>` minimal-API method name -> method.
fn map_verb(name: &str) -> Option<HttpMethod> {
    match name {
        "MapGet" => Some(HttpMethod::Get),
        "MapPost" => Some(HttpMethod::Post),
        "MapPut" => Some(HttpMethod::Put),
        "MapDelete" => Some(HttpMethod::Delete),
        "MapPatch" => Some(HttpMethod::Patch),
        _ => None,
    }
}

/// The `attribute` nodes attached to `node` (across its `attribute_list`s).
fn attributes<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    let mut out = Vec::new();
    for list in node.children(&mut cursor) {
        if list.kind() == "attribute_list" {
            let mut lc = list.walk();
            out.extend(list.children(&mut lc).filter(|c| c.kind() == "attribute"));
        }
    }
    out
}

fn attribute_name(attr: Node, src: &[u8]) -> String {
    attr.child_by_field_name("name")
        .map(|n| text(n, src))
        .unwrap_or_default()
}

/// First string-literal argument of an attribute (`[Http..("path")]`).
fn attribute_string(attr: Node, src: &[u8]) -> Option<String> {
    let mut cursor = attr.walk();
    let args = attr
        .children(&mut cursor)
        .find(|c| c.kind() == "attribute_argument_list")?;
    let mut ac = args.walk();
    for arg in args.named_children(&mut ac) {
        if arg.kind() == "attribute_argument"
            && let Some(s) = first_child(arg)
            && let Some(text) = cs_string(s, src)
        {
            return Some(text);
        }
    }
    None
}

/// First named child of a node.
fn first_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

/// Inner text of a C# `string_literal`, else `None`.
fn cs_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let mut cursor = node.walk();
    Some(
        node.named_children(&mut cursor)
            .find(|c| c.kind() == "string_literal_content")
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
[ApiController]
[Route("api/users")]
public class UsersController : ControllerBase {
    [HttpGet("{id}")]
    public User Get(int id) => null;

    [HttpPost]
    public void Create() {}
}

[Route("api/[controller]")]
public class OrdersController : ControllerBase {
    [HttpDelete("{id}")]
    public void Remove(int id) {}
}

var app = builder.Build();
app.MapGet("/health", Health);
app.MapPost("/items", () => {});
"#;

    #[test]
    fn attribute_controller_routes_with_base() {
        let eps = extract_aspnet(APP);
        // Paths are normalized to a leading slash, like every other extractor.
        check!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}").map(|e| e.handler.as_str()) == Some("Get")
        );
        // No method-level path -> base only.
        check!(
            ep(&eps, HttpMethod::Post, "/api/users").map(|e| e.handler.as_str()) == Some("Create")
        );
        // `[controller]` token expands to the class name minus `Controller`.
        check!(ep(&eps, HttpMethod::Delete, "/api/Orders/{id}").is_some());
    }

    #[test]
    fn minimal_api_routes() {
        let eps = extract_aspnet(APP);
        check!(ep(&eps, HttpMethod::Get, "/health").map(|e| e.handler.as_str()) == Some("Health"));
        // Inline lambda handler -> no symbol.
        check!(ep(&eps, HttpMethod::Post, "/items").map(|e| e.handler.as_str()) == Some(""));
    }

    #[test]
    fn no_mounts_and_parse_failure_safe() {
        check!(extract_aspnet_mounts(APP).is_empty());
        check!(extract_aspnet("???").is_empty());
    }
}
