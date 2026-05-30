//! Spring Boot REST controller extraction.
//!
//! Endpoints are declared with annotations rather than a router chain:
//! - `@GetMapping("/users/{id}")` / `@PostMapping` / `@PutMapping` /
//!   `@DeleteMapping` / `@PatchMapping` on a handler method, and
//! - `@RequestMapping(value = "/x", method = RequestMethod.PUT)` (method
//!   required for a method-level endpoint).
//!
//! A class-level `@RequestMapping("/api")` contributes a base path that is
//! prepended to each handler's declared path. Spring has no in-code router
//! composition, so no mounts are emitted.

use tree_sitter::{Node, Parser, Tree};

use stratograph_core::{HttpMethod, Language};

use super::ts::{join, span_of, text};
use super::{RawEndpoint, RawMount};
use crate::registry;

/// Extract Spring controller endpoints. Returns empty on parse failure.
pub fn extract_spring(source: &str) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() != "class_declaration" {
            return;
        }
        let base = child_of_kind(n, "modifiers")
            .and_then(|m| class_base_path(m, src))
            .unwrap_or_default();
        let Some(body) = n.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for m in body.named_children(&mut cursor) {
            if m.kind() != "method_declaration" {
                continue;
            }
            let Some(mods) = child_of_kind(m, "modifiers") else {
                continue;
            };
            let handler = m
                .child_by_field_name("name")
                .map(|id| text(id, src))
                .unwrap_or_default();
            for ann in annotations(mods) {
                if let Some((method, path)) = mapping(ann, src) {
                    out.push(RawEndpoint {
                        method,
                        path: join(&base, &path),
                        handler: handler.clone(),
                        span: span_of(m),
                    });
                }
            }
        }
    });
    out
}

/// Spring controllers have no in-code router composition.
pub fn extract_spring_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Java).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// First direct child of `node` with the given kind.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

/// Annotation nodes (with or without arguments) inside a `modifiers` node.
fn annotations<'a>(modifiers: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = modifiers.walk();
    modifiers
        .children(&mut cursor)
        .filter(|c| matches!(c.kind(), "annotation" | "marker_annotation"))
        .collect()
}

/// Simple annotation name (last `.`-segment of the annotation's name).
fn annotation_name(ann: Node, src: &[u8]) -> String {
    let raw = ann
        .child_by_field_name("name")
        .map(|n| text(n, src))
        .unwrap_or_default();
    raw.rsplit('.').next().unwrap_or(&raw).to_string()
}

/// Base path from a class-level mapping annotation, if any.
fn class_base_path(modifiers: Node, src: &[u8]) -> Option<String> {
    for ann in annotations(modifiers) {
        let name = annotation_name(ann, src);
        if name.ends_with("Mapping")
            && let Some(p) = annotation_path(ann, src)
            && !p.is_empty()
        {
            return Some(p);
        }
    }
    None
}

/// `(method, declared path)` for a handler-method mapping annotation, or `None`
/// if the annotation is not a request mapping (or a `@RequestMapping` without a
/// concrete HTTP method).
fn mapping(ann: Node, src: &[u8]) -> Option<(HttpMethod, String)> {
    let path = annotation_path(ann, src).unwrap_or_default();
    let method = match annotation_name(ann, src).as_str() {
        "GetMapping" => HttpMethod::Get,
        "PostMapping" => HttpMethod::Post,
        "PutMapping" => HttpMethod::Put,
        "DeleteMapping" => HttpMethod::Delete,
        "PatchMapping" => HttpMethod::Patch,
        "RequestMapping" => request_method(ann, src)?,
        _ => return None,
    };
    Some((method, path))
}

/// The path string of a mapping annotation: a positional string argument, or a
/// `value = "…"` / `path = "…"` element. `None` for marker annotations.
fn annotation_path(ann: Node, src: &[u8]) -> Option<String> {
    let args = ann.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        match arg.kind() {
            "string_literal" => return Some(string_value(arg, src)),
            "element_value_pair" => {
                let key = arg.child_by_field_name("key").map(|k| text(k, src));
                if matches!(key.as_deref(), Some("value") | Some("path"))
                    && let Some(v) = arg.child_by_field_name("value")
                    && v.kind() == "string_literal"
                {
                    return Some(string_value(v, src));
                }
            }
            _ => {}
        }
    }
    None
}

/// HTTP method from a `@RequestMapping(method = RequestMethod.X)` element.
fn request_method(ann: Node, src: &[u8]) -> Option<HttpMethod> {
    let args = ann.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        if arg.kind() == "element_value_pair"
            && arg
                .child_by_field_name("key")
                .map(|k| text(k, src))
                .as_deref()
                == Some("method")
            && let Some(v) = arg.child_by_field_name("value")
        {
            // `RequestMethod.PUT` (field_access) or a bare `PUT`.
            let token = v
                .child_by_field_name("field")
                .map(|f| text(f, src))
                .unwrap_or_else(|| text(v, src));
            return HttpMethod::parse(&token);
        }
    }
    None
}

/// The text inside a `string_literal` (its `string_fragment`), or empty.
fn string_value(string_literal: Node, src: &[u8]) -> String {
    child_of_kind(string_literal, "string_fragment")
        .map(|f| text(f, src))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    const CONTROLLER: &str = r#"
@RestController
@RequestMapping("/api")
class UserController {
    @GetMapping("/users/{id}")
    public User get(@PathVariable Long id) { return null; }

    @PostMapping
    public void create() {}

    @RequestMapping(value = "/legacy", method = RequestMethod.PUT)
    public void legacy() {}

    @DeleteMapping(path = "/users/{id}")
    public void remove() {}
}
"#;

    #[test]
    fn extracts_methods_paths_and_handlers_with_base_prefix() {
        let eps = extract_spring(CONTROLLER);
        let get = ep(&eps, HttpMethod::Get, "/api/users/{id}").expect("GET");
        check!(get.handler == "get");
        check!(ep(&eps, HttpMethod::Post, "/api").map(|e| e.handler.as_str()) == Some("create"));
        check!(
            ep(&eps, HttpMethod::Put, "/api/legacy").map(|e| e.handler.as_str()) == Some("legacy")
        );
        check!(ep(&eps, HttpMethod::Delete, "/api/users/{id}").is_some());
    }

    #[test]
    fn request_mapping_without_method_is_not_an_endpoint() {
        // Class-level @RequestMapping is a base path, not an endpoint itself.
        let eps = extract_spring(CONTROLLER);
        check!(!eps.iter().any(|e| e.handler.is_empty()));
        check!(eps.len() == 4);
    }

    #[test]
    fn no_mounts() {
        check!(extract_spring_mounts(CONTROLLER).is_empty());
    }

    #[test]
    fn invalid_source_yields_nothing() {
        check!(extract_spring("???").is_empty());
    }
}
