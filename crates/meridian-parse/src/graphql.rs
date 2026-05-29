//! GraphQL resolver extraction (#47). Recognizes async-graphql root objects in
//! Rust — an inherent `impl <RootType>` preceded by an `#[Object]` /
//! `#[Subscription]` attribute — and emits one GraphQL endpoint per resolver
//! method, keyed `OpType.field`. The cross-boundary matcher links these to the
//! SDL/Hasura-derived `SchemaOp`s (EXPOSES) via the GraphQL-normalized
//! signature, so a `Query.get_user` resolver reconciles with the schema's
//! `Query.getUser`.
//!
//! Client operation extraction (`gql` tagged documents → INVOKES) is a
//! follow-up.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{Language, Span};

use crate::registry;

/// A resolved GraphQL field implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawGraphqlField {
    /// `Query`, `Mutation`, or `Subscription`.
    pub op_type: &'static str,
    pub field: String,
    /// Handler symbol (the resolver method name), resolved to a `HANDLES` edge.
    pub handler: String,
    pub span: Span,
}

/// Extract async-graphql resolver methods from Rust `source`.
pub fn extract_graphql_resolvers(source: &str) -> Vec<RawGraphqlField> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "impl_item" {
            return;
        }
        // async-graphql roots are inherent impls (no trait) annotated `#[Object]`
        // / `#[Subscription]` on the immediately-preceding attribute item.
        if n.child_by_field_name("trait").is_some() {
            return;
        }
        let Some(attr) = preceding_attribute(n, src) else {
            return;
        };
        let Some(type_name) = n.child_by_field_name("type").map(|t| text(t, src)) else {
            return;
        };
        let Some(op_type) = op_type_of(&attr, &type_name) else {
            return;
        };
        let Some(body) = n.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for item in body.named_children(&mut cursor) {
            if item.kind() == "function_item"
                && let Some(field) = item.child_by_field_name("name").map(|x| text(x, src))
            {
                out.push(RawGraphqlField {
                    op_type,
                    field: field.clone(),
                    handler: field,
                    span: span_of(item),
                });
            }
        }
    });
    out
}

/// The text of the attribute immediately preceding `impl_item`, if any.
fn preceding_attribute(impl_item: Node, src: &[u8]) -> Option<String> {
    let prev = impl_item.prev_sibling()?;
    (prev.kind() == "attribute_item").then(|| text(prev, src))
}

/// Map an attribute (`#[Object]` / `#[Subscription]`) + root type name to the
/// GraphQL operation type. `#[Object]` defaults to Query unless the type name
/// says otherwise; `#[Subscription]` is always a subscription.
fn op_type_of(attr: &str, type_name: &str) -> Option<&'static str> {
    let a = attr.to_ascii_lowercase();
    if a.contains("subscription") {
        return Some("Subscription");
    }
    if !a.contains("object") {
        return None; // not an async-graphql resolver root
    }
    let t = type_name.to_ascii_lowercase();
    Some(if t.contains("mutation") {
        "Mutation"
    } else if t.contains("subscription") {
        "Subscription"
    } else {
        "Query"
    })
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Rust).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
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
    fn extracts_query_and_mutation_resolvers() {
        let src = r#"
#[Object]
impl QueryRoot {
    async fn get_user(&self, id: i32) -> User { todo!() }
    async fn users(&self) -> Vec<User> { todo!() }
}

#[Object]
impl MutationRoot {
    async fn create_user(&self, name: String) -> User { todo!() }
}
"#;
        let r = extract_graphql_resolvers(src);
        check!(
            r.iter()
                .any(|f| f.op_type == "Query" && f.field == "get_user")
        );
        check!(r.iter().any(|f| f.op_type == "Query" && f.field == "users"));
        check!(
            r.iter()
                .any(|f| f.op_type == "Mutation" && f.field == "create_user")
        );
    }

    #[test]
    fn subscription_attribute_sets_op_type() {
        let src = "#[Subscription]\nimpl Sub {\n async fn ticks(&self) -> i32 { 0 }\n}\n";
        let r = extract_graphql_resolvers(src);
        check!(r.len() == 1);
        check!(r[0].op_type == "Subscription" && r[0].field == "ticks");
    }

    #[test]
    fn plain_impls_and_trait_impls_are_ignored() {
        // No `#[Object]` attribute → not a resolver; trait impls excluded too.
        check!(extract_graphql_resolvers("impl Foo { fn bar(&self) {} }").is_empty());
        check!(
            extract_graphql_resolvers("#[Object]\nimpl Display for Foo { fn fmt(&self) {} }")
                .is_empty()
        );
    }
}
