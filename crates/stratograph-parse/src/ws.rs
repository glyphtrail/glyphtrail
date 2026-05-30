//! WebSocket boundary extraction (#51), both boundaries:
//!
//! - **Connection** — browser-native `new WebSocket(url)` (JS/TS/TSX) opens via
//!   an HTTP `GET` upgrade at `url`; the connection path links to the server's
//!   upgrade route (a REST `GET` endpoint) since a WebSocket connection
//!   [`OperationKey`](stratograph_core::OperationKey) shares a REST `GET` signature.
//! - **Message** — matched by event/channel/method name across the wire:
//!   socket.io `socket.on("event", h)` (handler) and `socket.emit("event", …)`
//!   (client); SignalR `connection.on("Method", h)` (handler) and
//!   `connection.invoke("Method", …)` / `.send(...)` (client); and Centrifugo
//!   pub/sub `centrifuge.subscribe("channel", h)` / `newSubscription("channel")`
//!   (subscriber) and `centrifuge.publish("channel", …)` (publisher). To avoid
//!   matching unrelated `.on`/`.emit` emitters, only socket-like
//!   ([`SOCKETIO_RECEIVERS`]), SignalR ([`SIGNALR_RECEIVERS`]) and Centrifugo
//!   ([`CENTRIFUGO_RECEIVERS`]) receiver identifiers are considered, with verbs
//!   gated per family and reserved lifecycle events skipped.
//!
//! Only string/template-literal URLs and string-literal event/channel names are
//! extracted; dynamic values are out of scope. Native `send`/`onmessage` framing
//! (no event name to key on) and Phoenix Channels remain follow-ups.

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

/// socket.io reserved/lifecycle events that are not user message channels.
const RESERVED_EVENTS: [&str; 6] = [
    "connect",
    "connection",
    "disconnect",
    "disconnecting",
    "error",
    "reconnect",
];

/// Extract message-boundary sites from JS/TS/TSX `source`: socket.io
/// (`on`/`emit`), SignalR (`on`/`invoke`/`send`), and Centrifugo
/// (`subscribe`/`newSubscription`/`publish`, keyed by channel). Empty on parse
/// failure or other languages.
pub fn extract_ws_events(source: &str, lang: &Language) -> Vec<RawWsEvent> {
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
        if n.kind() == "call_expression"
            && let Some(ev) = ws_event(n, src)
        {
            out.push(ev);
        }
    });
    out
}

/// A `socket.on("ev", handler)` / `socket.emit("ev", …)` call as a [`RawWsEvent`],
/// else `None`. Gated on a socket-like receiver and a string-literal event name.
fn ws_event(call: Node, src: &[u8]) -> Option<RawWsEvent> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let receiver = text(func.child_by_field_name("object")?, src);
    let property = text(func.child_by_field_name("property")?, src);
    // Allowed verbs depend on the receiver family: socket.io uses on/emit;
    // SignalR uses on plus invoke/send (call a hub method).
    let kind = if SOCKETIO_RECEIVERS.contains(&receiver.as_str()) {
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
    } else {
        return None;
    };
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let named: Vec<Node> = args.named_children(&mut cursor).collect();
    let event = string_literal(*named.first()?, src)?;
    if RESERVED_EVENTS.contains(&event.as_str()) {
        return None;
    }
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
}
