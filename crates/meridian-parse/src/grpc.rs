//! gRPC server-impl extraction (#46). Recognizes tonic service implementations
//! in Rust — `impl <Service> for <Type>` blocks whose methods carry the tonic
//! shape (`Request<…>` / `Response<…>` / `Status`) — and emits one gRPC endpoint
//! per method, keyed `Service/method`. The cross-boundary matcher links these to
//! the proto-derived `SchemaOp`s (EXPOSES) via the gRPC-normalized signature, so
//! a `Greeter/say_hello` handler reconciles with `helloworld.Greeter/SayHello`.
//!
//! Client-stub (`INVOKES`) extraction needs receiver-type tracking and is a
//! follow-up; this covers the server side.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{Language, Span};

use crate::registry;

/// A gRPC method implementation: `service`/`method` plus the handler symbol name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawGrpcEndpoint {
    pub service: String,
    pub method: String,
    /// Handler symbol (the impl method name), resolved to a `HANDLES` edge.
    pub handler: String,
    pub span: Span,
}

/// Extract tonic gRPC service-method implementations from Rust `source`.
pub fn extract_grpc_endpoints(source: &str) -> Vec<RawGrpcEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "impl_item" {
            return;
        }
        // Only trait impls (`impl Service for Type`) can be a gRPC service.
        let Some(trait_node) = n.child_by_field_name("trait") else {
            return;
        };
        let Some(body) = n.child_by_field_name("body") else {
            return;
        };
        // Tonic shape gate: the impl body deals in `Request`/`Response`/`Status`.
        let body_text = text(body, src);
        if !(body_text.contains("Request") && body_text.contains("Status")) {
            return;
        }
        let Some(service) = last_type_ident(trait_node, src) else {
            return;
        };
        let mut cursor = body.walk();
        for item in body.named_children(&mut cursor) {
            if item.kind() == "function_item"
                && let Some(method) = item.child_by_field_name("name").map(|x| text(x, src))
            {
                out.push(RawGrpcEndpoint {
                    service: service.clone(),
                    method: method.clone(),
                    handler: method,
                    span: span_of(item),
                });
            }
        }
    });
    out
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Rust).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// The last `type_identifier` within a trait reference, so both `Greeter` and a
/// scoped/generic `greeter_server::Greeter<T>` yield `Greeter`.
fn last_type_ident(node: Node, src: &[u8]) -> Option<String> {
    let mut found = None;
    walk(node, &mut |n| {
        if n.kind() == "type_identifier" {
            found = Some(text(n, src));
        }
    });
    found
}

fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

fn span_of(node: Node) -> Span {
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn extracts_tonic_service_methods() {
        let src = r#"
#[tonic::async_trait]
impl Greeter for MyGreeter {
    async fn say_hello(&self, request: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        Ok(Response::new(HelloReply::default()))
    }
    async fn say_goodbye(&self, request: Request<ByeRequest>) -> Result<Response<ByeReply>, Status> {
        todo!()
    }
}
"#;
        let eps = extract_grpc_endpoints(src);
        check!(eps.len() == 2);
        check!(
            eps.iter()
                .any(|e| e.service == "Greeter" && e.method == "say_hello")
        );
        check!(
            eps.iter()
                .any(|e| e.service == "Greeter" && e.method == "say_goodbye")
        );
    }

    #[test]
    fn ignores_non_tonic_trait_impls() {
        // A plain trait impl without the tonic Request/Status shape is not gRPC.
        let src = "impl Display for Foo { fn fmt(&self, f: &mut Formatter) -> Result {} }";
        check!(extract_grpc_endpoints(src).is_empty());
    }

    #[test]
    fn scoped_trait_name_reduces_to_service() {
        let src = r#"
impl greeter_server::Greeter for S {
    async fn ping(&self, r: Request<()>) -> Result<Response<()>, Status> { todo!() }
}
"#;
        let eps = extract_grpc_endpoints(src);
        check!(eps.len() == 1);
        check!(eps[0].service == "Greeter");
    }
}
