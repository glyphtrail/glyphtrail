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

use glyphtrail_core::{Language, Span};

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

    #[test]
    fn first_operation_handles_named_anonymous_and_alias() {
        check!(
            first_operation("query GetUser { getUser(id: 1) { name } }")
                == Some(("Query", "getUser".into()))
        );
        check!(
            first_operation("mutation { createUser(name: \"a\") { id } }")
                == Some(("Mutation", "createUser".into()))
        );
        check!(first_operation("{ users { id } }") == Some(("Query", "users".into())));
        check!(
            first_operation("query Foo($id: ID!) { u: getUser(id: $id) { name } }")
                == Some(("Query", "getUser".into()))
        );
        check!(first_operation("subscription { ticks }") == Some(("Subscription", "ticks".into())));
    }

    #[test]
    fn extracts_gql_tagged_and_called_client_ops() {
        let src = "const Q = gql`query GetUser { getUser(id: 1) { name } }`;\n\
                   const M = graphql(`mutation { createUser(name: \"a\") { id } }`);";
        let ops = extract_graphql_client_ops(src, &Language::JavaScript);
        check!(
            ops.iter()
                .any(|o| o.op_type == "Query" && o.field == "getUser")
        );
        check!(
            ops.iter()
                .any(|o| o.op_type == "Mutation" && o.field == "createUser")
        );
    }

    #[test]
    fn non_js_yields_no_client_ops() {
        check!(extract_graphql_client_ops("gql`query { x }`", &Language::Rust).is_empty());
    }
}

/// A client-side GraphQL operation call site (`gql`/`graphql` tagged document).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawGraphqlClientOp {
    pub op_type: &'static str,
    pub field: String,
    pub span: Span,
}

/// Extract client GraphQL operations from JS/TS source: `gql`/`graphql`-tagged
/// (or -called) documents, reduced to the operation's first root field.
pub fn extract_graphql_client_ops(source: &str, lang: &Language) -> Vec<RawGraphqlClientOp> {
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return Vec::new();
    }
    let Some(tree) = parse_lang(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let callee = n
            .child_by_field_name("function")
            .or_else(|| n.named_child(0))
            .map(|c| text(c, src));
        if !matches!(callee.as_deref(), Some("gql") | Some("graphql")) {
            return;
        }
        if let Some(frag) = template_text(n, src)
            && let Some((op_type, field)) = first_operation(&frag)
        {
            out.push(RawGraphqlClientOp {
                op_type,
                field,
                span: span_of(n),
            });
        }
    });
    out
}

/// Concatenated `string_fragment`s of the first `template_string` under `node`.
fn template_text(node: Node, src: &[u8]) -> Option<String> {
    let mut found: Option<String> = None;
    walk(node, &mut |n| {
        if found.is_none() && n.kind() == "template_string" {
            let mut s = String::new();
            let mut cursor = n.walk();
            for c in n.named_children(&mut cursor) {
                if c.kind() == "string_fragment" {
                    s.push_str(&text(c, src));
                }
            }
            found = Some(s);
        }
    });
    found
}

/// Reduce a GraphQL operation document to `(OpType, first root field)`.
/// Handles named/anonymous operations, variable definitions, and field aliases.
fn first_operation(doc: &str) -> Option<(&'static str, String)> {
    let cleaned: String = doc
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let s = cleaned.trim_start();
    let (op, rest) = if let Some(r) = s.strip_prefix("mutation") {
        ("Mutation", r)
    } else if let Some(r) = s.strip_prefix("subscription") {
        ("Subscription", r)
    } else if let Some(r) = s.strip_prefix("query") {
        ("Query", r)
    } else {
        ("Query", s) // anonymous `{ … }`
    };
    let brace = rest.find('{')?;
    first_field(&rest[brace + 1..]).map(|field| (op, field))
}

/// Leading identifier of `s`, with the remainder after it.
fn take_ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// The first field name inside a selection set, resolving `alias: field`.
fn first_field(sel: &str) -> Option<String> {
    let (first, rest) = take_ident(sel)?;
    // `alias: field` — the real field follows the colon.
    if let Some(after) = rest.trim_start().strip_prefix(':') {
        return take_ident(after).map(|(f, _)| f);
    }
    Some(first)
}

fn parse_lang(source: &str, lang: &Language) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&registry::grammar(lang)?).ok()?;
    parser.parse(source, None)
}
