//! A structural trace of the call control plane.
//!
//! The group-call offer is answered by the call service with a stanza this app never sees: the
//! library parses it internally and hands back an error string (`call service response failed:
//! missing group-call integer attribute`), so the one fact needed to fix it — which attribute the
//! server does and does not send — is invisible from here. `Event::RawNode` is the library's public
//! observation point for every decoded stanza, so this module subscribes to it and writes the
//! stanza's *shape*: tags, attribute names, child counts and byte lengths.
//!
//! It never writes a value. No JID, no call id, no key material, no message content — an attribute
//! is printed by name and a value only by whether it is present, its byte length, or its string
//! length. That is enough to answer "which integer is missing" and nothing else.
//!
//! Off unless `ZAPFAST_CALL_TRACE` is set, and narrowed to stanzas that carry `group_info` or are a
//! `<call>`, so an ordinary session's acks do not flood the log.

use std::fmt::Write as _;

use whatsapp_rust::wacore_binary::{NodeContentRef, NodeRef};

/// How deep the description walks, so a deep or hostile stanza cannot produce an unbounded log.
const MAX_DEPTH: usize = 5;

/// Whether the trace was asked for. Read once, at the point the client is built.
pub(super) fn enabled() -> bool {
    std::env::var_os("ZAPFAST_CALL_TRACE").is_some()
}

/// Whether a decoded stanza belongs to the call control plane.
///
/// A `<call>` is one by definition. Anything else qualifies only by carrying a `group_info`
/// descendant, which is exactly what the failing parse reads: a stanza without one cannot be the
/// group snapshot the caller is refused.
pub(super) fn is_call_control(node: &NodeRef<'_>) -> bool {
    node.tag == "call" || carries(node, "group_info", 0)
}

fn carries(node: &NodeRef<'_>, tag: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    node.children().is_some_and(|children| {
        children
            .iter()
            .any(|child| child.tag == tag || carries(child, tag, depth + 1))
    })
}

/// The node's structure as text: tag, attribute names, child tags with their own names, and the
/// size of any content. A value is never rendered.
pub(super) fn describe(node: &NodeRef<'_>, depth: usize) -> String {
    let mut out = String::new();
    if depth > MAX_DEPTH {
        return format!("{}…\n", "  ".repeat(depth));
    }
    let _ = write!(out, "{}<{}", "  ".repeat(depth), node.tag);
    let mut names: Vec<&str> = node.attrs_iter().map(|(name, _)| &**name).collect();
    // Sorted, so two stanzas compare by reading rather than by remembering the wire order.
    names.sort_unstable();
    for name in names {
        let _ = write!(out, " {name}=?");
    }
    out.push('>');
    match &node.content {
        Some(NodeContentRef::Nodes(children)) => {
            let _ = write!(out, " children={}", children.len());
            for child in children.iter() {
                out.push('\n');
                out.push_str(&describe(child, depth + 1));
            }
        }
        Some(NodeContentRef::Bytes(bytes)) => {
            let _ = write!(out, " bytes={}", bytes.len());
        }
        Some(NodeContentRef::String(text)) => {
            let _ = write!(out, " str_len={}", text.len());
        }
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::{NodeBuilder, wacore_binary::Node};

    fn group_ack(with_limit: bool) -> Node {
        let mut group_info = NodeBuilder::new("group_info")
            .attr("call-id", "x")
            .attr("media", "audio")
            .attr("transaction-id", "1");
        if with_limit {
            group_info = group_info.attr("connected-limit", "32");
        }
        NodeBuilder::new("ack")
            .children([group_info.build()])
            .build()
    }

    /// A group snapshot is what the caller's refusal reads, so a stanza carrying one is traced and
    /// an ordinary ack is not.
    #[test]
    fn only_a_call_plane_stanza_is_traced() {
        let ack = group_ack(true);
        assert!(is_call_control(&ack.as_node_ref()));
        let plain = NodeBuilder::new("ack").attr("id", "1").build();
        assert!(!is_call_control(&plain.as_node_ref()));
        let call = NodeBuilder::new("call").attr("to", "x@call").build();
        assert!(is_call_control(&call.as_node_ref()));
    }

    /// The description names every attribute the parser looks at and never a value, so the missing
    /// integer is visible as an absent name while the call id and media stay out of the log.
    #[test]
    fn the_description_names_attributes_and_never_a_value() {
        let ack = group_ack(false);
        let text = describe(&ack.as_node_ref(), 0);
        assert!(text.contains("<ack"), "{text}");
        assert!(text.contains("<group_info"), "{text}");
        for name in ["call-id", "media", "transaction-id"] {
            assert!(text.contains(name), "{name} missing from {text}");
        }
        assert!(
            !text.contains("connected-limit"),
            "the missing integer must be absent by name: {text}"
        );
        // Every attribute is rendered as `name=?`, so the three of them account for the only
        // attribute `=` signs in the text and no value can have slipped through beside one.
        assert_eq!(text.matches("=?").count(), 3, "a value leaked: {text}");
        for value in ["audio", "\"x\""] {
            assert!(!text.contains(value), "a value leaked: {text}");
        }

        let with_limit = group_ack(true);
        let text = describe(&with_limit.as_node_ref(), 0);
        assert!(
            text.contains("connected-limit=?"),
            "a present integer is visible by name: {text}"
        );
    }
}
