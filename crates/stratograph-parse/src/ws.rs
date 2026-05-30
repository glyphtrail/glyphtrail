//! WebSocket boundary extraction (#51), both boundaries:
//!
//! - **Connection** — browser-native `new WebSocket(url)` (JS/TS/TSX) opens via
//!   an HTTP `GET` upgrade at `url`; the connection path links to the server's
//!   upgrade route (a REST `GET` endpoint) since a WebSocket connection
//!   [`OperationKey`](stratograph_core::OperationKey) shares a REST `GET` signature.
//! - **Message** — matched by event/channel/method name across the wire:
//!   socket.io `socket.on("event", h)` (handler) and `socket.emit("event", …)`
//!   (client), namespace-aware — a variable bound to `io.of("/ns")` / `io("/ns")`
//!   is recognised and its events are keyed by namespace; SignalR
//!   `connection.on("Method", h)` (handler) and
//!   `connection.invoke("Method", …)` / `.send(...)` (client); and Centrifugo
//!   pub/sub `centrifuge.subscribe("channel", h)` / `newSubscription("channel")`
//!   (subscriber) and `centrifuge.publish("channel", …)` (publisher); Phoenix
//!   Channels JS client `channel.push("event", …)` (send) / `channel.on(…)`
//!   (receive) and the Elixir server `handle_in("event", …)` handler with its
//!   `broadcast`/`push` sends. To avoid matching unrelated `.on`/`.emit`
//!   emitters, only socket-like ([`SOCKETIO_RECEIVERS`]), SignalR
//!   ([`SIGNALR_RECEIVERS`]), Centrifugo ([`CENTRIFUGO_RECEIVERS`]) and Phoenix
//!   ([`PHOENIX_RECEIVERS`]) receiver identifiers are considered, with verbs
//!   gated per family and reserved lifecycle events skipped.
//!
//! Only string/template-literal URLs and string-literal event/channel names are
//! extracted; dynamic values are out of scope. Native `send`/`onmessage` framing
//! (no event name to key on) remains a follow-up.

use std::collections::HashMap;

use stratograph_core::{Language, Span};
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// A client WebSocket connection site: the connection URL/path and its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWsConnect {
    /// Connection URL as written (literal or template); canonicalized later.
    pub path: String,
    pub span: Span,
}

/// Whether a socket.io call receives an event (`on`, a server-side handler) or
/// sends one (`emit`, a client-side call site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsEventKind {
    On,
    Emit,
}

/// A socket.io message-boundary site: `socket.on("ev", handler)` /
/// `socket.emit("ev", …)` (#51). Keyed later by event name so an emit links to
/// the matching `on` handler via INVOKES.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWsEvent {
    pub kind: WsEventKind,
    /// Event/channel name (string literal).
    pub event: String,
    /// Handler symbol for `on` (empty for an inline closure or for `emit`).
    pub handler: String,
    pub span: Span,
}

/// Receiver identifiers treated as socket.io sockets (verbs `on`/`emit`). Tight
/// allowlists trade recall for precision so unrelated event emitters (DOM nodes,
/// generic EventEmitters, raw WebSockets) aren't matched.
const SOCKETIO_RECEIVERS: [&str; 5] = ["socket", "io", "sock", "nsp", "namespace"];

/// Receiver identifiers treated as SignalR hub connections (verbs `invoke`/
/// `send` to call a hub method, `on` to register a client handler) (#51).
const SIGNALR_RECEIVERS: [&str; 4] = ["connection", "hubConnection", "conn", "hub"];

/// Receiver identifiers treated as Centrifugo clients/subscriptions (#51). The
/// pub/sub key is the **channel** name: `subscribe`/`newSubscription` register a
/// subscriber (listener), `publish` sends to the channel.
const CENTRIFUGO_RECEIVERS: [&str; 4] = ["centrifuge", "centrifugo", "sub", "subscription"];

/// Receiver identifiers treated as a Phoenix Channels client handle (#51): the
/// JS `channel.push("event", …)` sends to the server `handle_in` handler, and
/// `channel.on("event", h)` receives a server `push`/`broadcast`.
const PHOENIX_RECEIVERS: [&str; 2] = ["channel", "chan"];

/// socket.io reserved/lifecycle events that are not user message channels.
const RESERVED_EVENTS: [&str; 6] = [
    "connect",
    "connection",
    "disconnect",
    "disconnecting",
    "error",
    "reconnect",
];

/// Extract message-boundary sites from `source`: JS/TS/TSX socket.io
/// (`on`/`emit`), SignalR (`on`/`invoke`/`send`), Centrifugo
/// (`subscribe`/`newSubscription`/`publish`) and Phoenix (`channel.push`/`.on`);
/// and Elixir Phoenix channels (`handle_in` handlers, `broadcast`/`push`).
/// Empty on parse failure or unsupported languages.
pub fn extract_ws_events(source: &str, lang: &Language) -> Vec<RawWsEvent> {
    match lang {
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            extract_js_ws_events(source, lang)
        }
        Language::Elixir => extract_elixir_ws_events(source),
        _ => Vec::new(),
    }
}

/// socket.io / SignalR / Centrifugo / Phoenix client message sites in JS/TS.
fn extract_js_ws_events(source: &str, lang: &Language) -> Vec<RawWsEvent> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    // Map socket.io namespace-bound variables (`const ns = io.of("/admin")`,
    // `const sock = io("/admin")`) to their namespace, so their events are both
    // captured and kept distinct from other namespaces (#51).
    let namespaces = namespace_bindings(tree.root_node(), src);
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(ev) = ws_event(n, src, &namespaces)
        {
            out.push(ev);
        }
    });
    out
}

/// Variable → socket.io namespace from same-file `io.of("/ns")` / `io("/ns")` /
/// `io.connect("/ns")` assignments.
fn namespace_bindings(root: Node, src: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "variable_declarator" {
            return;
        }
        if let Some(name) = n
            .child_by_field_name("name")
            .filter(|x| x.kind() == "identifier")
            && let Some(value) = n.child_by_field_name("value")
            && let Some(ns) = io_namespace(value, src)
        {
            map.insert(text(name, src), ns);
        }
    });
    map
}

/// Namespace string of an `io.of("/ns")` / `io("/ns")` / `io.connect("/ns")`
/// call expression, else `None`.
fn io_namespace(call: Node, src: &[u8]) -> Option<String> {
    if call.kind() != "call_expression" {
        return None;
    }
    let func = call.child_by_field_name("function")?;
    let is_io = match func.kind() {
        "identifier" => text(func, src) == "io",
        "member_expression" => {
            text(func.child_by_field_name("object")?, src) == "io"
                && matches!(
                    text(func.child_by_field_name("property")?, src).as_str(),
                    "of" | "connect"
                )
        }
        _ => false,
    };
    if !is_io {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first = args.named_children(&mut cursor).next()?;
    string_literal(first, src).filter(|ns| !ns.is_empty())
}

/// Server-side Phoenix channel message sites in Elixir (#51): `handle_in("ev",
/// …)` clauses (the handler) and `broadcast`/`push` calls (server → client).
fn extract_elixir_ws_events(source: &str) -> Vec<RawWsEvent> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(&Language::Elixir).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "call" {
            return;
        }
        let Some(target) = elixir_call_target(n, src) else {
            return;
        };
        match target.as_str() {
            // `def handle_in("ev", payload, socket) do … end`: the def's first
            // argument is a call to `handle_in` whose first argument is the event.
            "def" | "defp" => {
                if let Some(inner) = elixir_first_arg_call(n)
                    && elixir_call_target(inner, src).as_deref() == Some("handle_in")
                    && let Some(event) = elixir_first_string_arg(inner, src)
                    && !RESERVED_EVENTS.contains(&event.as_str())
                {
                    out.push(RawWsEvent {
                        kind: WsEventKind::On,
                        event,
                        handler: "handle_in".into(),
                        span: span_of(inner),
                    });
                }
            }
            // Server → client sends, keyed by event name.
            "broadcast" | "broadcast!" | "broadcast_from" | "broadcast_from!" | "push" => {
                if let Some(event) = elixir_first_string_arg(n, src)
                    && !RESERVED_EVENTS.contains(&event.as_str())
                {
                    out.push(RawWsEvent {
                        kind: WsEventKind::Emit,
                        event,
                        handler: String::new(),
                        span: span_of(n),
                    });
                }
            }
            _ => {}
        }
    });
    out
}

/// Target identifier of an Elixir `call` node (`foo(...)` → `foo`).
fn elixir_call_target(call: Node, src: &[u8]) -> Option<String> {
    let first = call.named_child(0)?;
    (first.kind() == "identifier").then(|| text(first, src))
}

/// The `arguments` node of an Elixir call, if present.
fn elixir_args(call: Node) -> Option<Node> {
    let mut c = call.walk();
    call.named_children(&mut c)
        .find(|ch| ch.kind() == "arguments")
}

/// The first argument of an Elixir call that is itself a `call` (the nested
/// `handle_in(...)` inside `def handle_in(...)`).
fn elixir_first_arg_call(call: Node) -> Option<Node> {
    let args = elixir_args(call)?;
    let mut c = args.walk();
    args.named_children(&mut c).find(|ch| ch.kind() == "call")
}

/// First string-literal argument of an Elixir call, ignoring interpolation.
fn elixir_first_string_arg(call: Node, src: &[u8]) -> Option<String> {
    let args = elixir_args(call)?;
    let mut c = args.walk();
    args.named_children(&mut c)
        .find_map(|ch| elixir_string(ch, src))
}

/// Inner text of a simple (non-interpolated) Elixir string literal.
fn elixir_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let trimmed = text(node, src).trim_matches('"').to_string();
    (!trimmed.is_empty() && !trimmed.contains("#{")).then_some(trimmed)
}

/// A `socket.on("ev", handler)` / `socket.emit("ev", …)` call as a [`RawWsEvent`],
/// else `None`. Gated on a socket-like receiver and a string-literal event name.
/// `namespaces` maps socket.io namespace-bound variables to their namespace.
fn ws_event(call: Node, src: &[u8], namespaces: &HashMap<String, String>) -> Option<RawWsEvent> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let receiver = text(func.child_by_field_name("object")?, src);
    let property = text(func.child_by_field_name("property")?, src);
    // A variable bound to `io.of(...)` / `io(...)` is a socket.io namespace.
    let namespace = namespaces.get(&receiver);
    // Allowed verbs depend on the receiver family: socket.io uses on/emit;
    // SignalR uses on plus invoke/send (call a hub method).
    let kind = if SOCKETIO_RECEIVERS.contains(&receiver.as_str()) || namespace.is_some() {
        match property.as_str() {
            "on" => WsEventKind::On,
            "emit" => WsEventKind::Emit,
            _ => return None,
        }
    } else if SIGNALR_RECEIVERS.contains(&receiver.as_str()) {
        match property.as_str() {
            "on" => WsEventKind::On,
            "invoke" | "send" => WsEventKind::Emit,
            _ => return None,
        }
    } else if CENTRIFUGO_RECEIVERS.contains(&receiver.as_str()) {
        // Keyed by channel: a subscriber listens, a publisher sends to it.
        match property.as_str() {
            "subscribe" | "newSubscription" => WsEventKind::On,
            "publish" => WsEventKind::Emit,
            _ => return None,
        }
    } else if PHOENIX_RECEIVERS.contains(&receiver.as_str()) {
        // Phoenix JS client: `push` sends to the server, `on` receives.
        match property.as_str() {
            "on" => WsEventKind::On,
            "push" => WsEventKind::Emit,
            _ => return None,
        }
    } else {
        return None;
    };
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let named: Vec<Node> = args.named_children(&mut cursor).collect();
    let raw_event = string_literal(*named.first()?, src)?;
    if RESERVED_EVENTS.contains(&raw_event.as_str()) {
        return None;
    }
    // Qualify the event with its namespace so the same event name in different
    // namespaces stays distinct; the default namespace keeps the bare name.
    let event = match namespace {
        Some(ns) => format!("{ns}#{raw_event}"),
        None => raw_event,
    };
    let handler = match kind {
        WsEventKind::On => named
            .get(1)
            .map(|h| handler_name(*h, src))
            .unwrap_or_default(),
        WsEventKind::Emit => String::new(),
    };
    Some(RawWsEvent {
        kind,
        event,
        handler,
        span: span_of(call),
    })
}

/// Inner text of a plain string-literal argument (not a template), else `None`.
fn string_literal(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    Some(
        node.named_children(&mut cursor)
            .filter(|ch| ch.kind() == "string_fragment")
            .map(|ch| text(ch, src))
            .collect::<String>(),
    )
}

/// Handler symbol named by an `on` argument: a bare identifier or `obj.method`,
/// empty for an inline function/arrow.
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

/// Extract `new WebSocket(url)` connections from JS/TS/TSX `source`. Empty on
/// parse failure or for other languages.
pub fn extract_ws_connections(source: &str, lang: &Language) -> Vec<RawWsConnect> {
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "new_expression"
            && n.child_by_field_name("constructor")
                .map(|c| text(c, src))
                .as_deref()
                == Some("WebSocket")
            && let Some(url) = first_url_arg(n.child_by_field_name("arguments"), src)
        {
            out.push(RawWsConnect {
                path: url,
                span: span_of(n),
            });
        }
    });
    out
}

/// First argument as a string- or template-literal URL.
fn first_url_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find_map(|a| match a.kind() {
            "string" => {
                let mut c = a.walk();
                Some(
                    a.named_children(&mut c)
                        .filter(|ch| ch.kind() == "string_fragment")
                        .map(|ch| text(ch, src))
                        .collect::<String>(),
                )
            }
            "template_string" => Some(text(a, src).trim_matches('`').to_string()),
            _ => None,
        })
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

fn walk(node: Node, f: &mut dyn FnMut(Node)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn extracts_native_websocket_connection() {
        let src = r#"
const sock = new WebSocket("/ws/chat");
const url = `/rooms/${id}`;
const other = new WebSocket(url);          // dynamic var: skipped
const tmpl = new WebSocket(`/live/${id}`); // template: kept verbatim
"#;
        let conns = extract_ws_connections(src, &Language::JavaScript);
        let paths: Vec<&str> = conns.iter().map(|c| c.path.as_str()).collect();
        check!(paths.contains(&"/ws/chat"));
        check!(paths.contains(&"/live/${id}"));
        // A bare-variable URL is not a literal, so it is not extracted.
        check!(!paths.iter().any(|p| p.is_empty()));
        check!(conns.len() == 2);
    }

    #[test]
    fn skips_other_languages() {
        check!(extract_ws_connections("new WebSocket(\"/ws\")", &Language::Rust).is_empty());
    }

    #[test]
    fn extracts_socketio_on_and_emit_events() {
        let src = r#"
socket.on("chat:message", onMessage);
socket.emit("chat:message", payload);
io.on("connection", (s) => {});      // reserved: skipped
button.on("click", handleClick);      // non-socket receiver: skipped
socket.emit("typing", { user });
"#;
        let evs = extract_ws_events(src, &Language::JavaScript);
        let on = evs.iter().find(|e| e.kind == WsEventKind::On).expect("on");
        check!(on.event == "chat:message");
        check!(on.handler == "onMessage");
        let emits: Vec<&str> = evs
            .iter()
            .filter(|e| e.kind == WsEventKind::Emit)
            .map(|e| e.event.as_str())
            .collect();
        check!(emits.contains(&"chat:message"));
        check!(emits.contains(&"typing"));
        // Reserved `connection` and non-socket `button.on` are excluded.
        check!(
            !evs.iter()
                .any(|e| e.event == "connection" || e.event == "click")
        );
    }

    #[test]
    fn socketio_namespaces_keyed_distinctly() {
        let src = r#"
const adminNs = io.of("/admin");
adminNs.on("msg", onAdminMsg);          // namespaced handler
const sock = io("/admin");
sock.emit("msg", payload);              // namespaced client emit
socket.emit("msg", other);              // default namespace: bare key
const userNs = io.of("/user");
userNs.on("msg", onUserMsg);            // same name, different namespace
"#;
        let evs = extract_ws_events(src, &Language::JavaScript);
        // The /admin handler and client emit share the namespaced key (they link),
        // and the handler symbol is captured even though the receiver is a var.
        check!(evs.iter().any(|e| e.kind == WsEventKind::On
            && e.event == "/admin#msg"
            && e.handler == "onAdminMsg"));
        check!(
            evs.iter()
                .any(|e| e.kind == WsEventKind::Emit && e.event == "/admin#msg")
        );
        // The default-namespace emit stays a separate, bare key.
        check!(
            evs.iter()
                .any(|e| e.kind == WsEventKind::Emit && e.event == "msg")
        );
        // The /user `msg` handler does not collide with /admin.
        check!(evs.iter().any(|e| e.event == "/user#msg"));
        check!(
            !evs.iter()
                .any(|e| e.event == "/user#msg" && e.handler == "onAdminMsg")
        );
    }

    #[test]
    fn ws_events_skip_other_languages() {
        check!(extract_ws_events("socket.emit(\"x\", y)", &Language::Rust).is_empty());
    }

    #[test]
    fn extracts_signalr_invoke_send_and_on() {
        let src = r#"
connection.on("ReceiveMessage", onReceive);
connection.invoke("SendMessage", user, msg);
hubConnection.send("Notify", payload);
connection.start();                  // not a verb we track
widget.invoke("x", 1);               // non-signalr receiver: skipped
"#;
        let evs = extract_ws_events(src, &Language::TypeScript);
        let on = evs.iter().find(|e| e.kind == WsEventKind::On).expect("on");
        check!(on.event == "ReceiveMessage" && on.handler == "onReceive");
        let emits: Vec<&str> = evs
            .iter()
            .filter(|e| e.kind == WsEventKind::Emit)
            .map(|e| e.event.as_str())
            .collect();
        check!(emits.contains(&"SendMessage")); // invoke
        check!(emits.contains(&"Notify")); // send
        check!(!evs.iter().any(|e| e.event == "x")); // non-signalr receiver excluded
    }

    #[test]
    fn extracts_centrifugo_subscribe_and_publish() {
        let src = r#"
const sub = centrifuge.newSubscription("news");
centrifuge.subscribe("chat", onChat);
centrifuge.publish("chat", payload);
sub.publish(data);                   // no channel arg: skipped
other.subscribe("x", h);             // non-centrifugo receiver: skipped
"#;
        let evs = extract_ws_events(src, &Language::JavaScript);
        // Subscribers (listeners) become `On`, keyed by channel.
        let subs: Vec<&str> = evs
            .iter()
            .filter(|e| e.kind == WsEventKind::On)
            .map(|e| e.event.as_str())
            .collect();
        check!(subs.contains(&"news")); // newSubscription
        check!(subs.contains(&"chat")); // subscribe
        check!(
            evs.iter()
                .any(|e| e.event == "chat" && e.handler == "onChat")
        );
        // Publisher becomes `Emit` on the same channel.
        check!(
            evs.iter()
                .any(|e| e.kind == WsEventKind::Emit && e.event == "chat")
        );
        // A non-centrifugo receiver is excluded.
        check!(!evs.iter().any(|e| e.event == "x"));
    }

    #[test]
    fn extracts_phoenix_js_channel_push_and_on() {
        let src = r#"
channel.push("new_msg", { body: "hi" });
channel.on("new_msg", onNewMsg);
other.push("x", 1);                  // non-phoenix receiver: skipped
"#;
        let evs = extract_ws_events(src, &Language::JavaScript);
        check!(
            evs.iter()
                .any(|e| e.kind == WsEventKind::Emit && e.event == "new_msg")
        ); // push
        check!(
            evs.iter().any(|e| e.kind == WsEventKind::On
                && e.event == "new_msg"
                && e.handler == "onNewMsg")
        ); // on
        check!(!evs.iter().any(|e| e.event == "x"));
    }

    #[test]
    fn extracts_phoenix_elixir_handle_in_and_sends() {
        let src = r#"defmodule App.RoomChannel do
  def handle_in("new_msg", payload, socket) do
    broadcast(socket, "room_update", payload)
    push(socket, "ack", %{})
    {:noreply, socket}
  end
end"#;
        let evs = extract_ws_events(src, &Language::Elixir);
        // The handler keyed by event, plus its broadcast/push sends.
        check!(evs.iter().any(|e| e.kind == WsEventKind::On
            && e.event == "new_msg"
            && e.handler == "handle_in"));
        let emits: Vec<&str> = evs
            .iter()
            .filter(|e| e.kind == WsEventKind::Emit)
            .map(|e| e.event.as_str())
            .collect();
        check!(emits.contains(&"room_update")); // broadcast
        check!(emits.contains(&"ack")); // push
    }
}
