//! A structural trace of the call control plane.
//!
//! The group-call offer is answered by the call service with a stanza this app never sees: the
//! library parses it internally and hands back an error string (`call service response failed:
//! missing group-call integer attribute`), so the one fact needed to fix it — which attribute the
//! server does and does not send — is invisible from here. `Event::RawNode` is the library's public
//! observation point for every decoded stanza, so this module subscribes to it and writes the
//! stanza's *shape*: tags, attribute names, child counts and byte lengths.
//!
//! It writes no identifying value. No JID user, no call id, no key material, no message content —
//! an attribute is printed by name, and a value only when it is a protocol enum or a small integer
//! (`error`, `media`, `rate`, `medium`, `ver`, …), by byte or string length otherwise. A `jid` is
//! printed only as its address family and device number, so a `self` entry that the server stripped
//! a device from can be told apart from one that never had it. A roster `<user>` is additionally
//! marked `self` when its jid is one of this account's own identities, which names the rejected
//! participant without printing the jid.
//!
//! Both directions are read: `Event::RawNode` for what the server sends, and `Event::SentFrame`
//! for the offer this app builds. A rejection names a request whose shape is only observable on
//! the way out.
//!
//! Off unless `ZAPFAST_CALL_TRACE` is set, and narrowed to stanzas that carry `group_info` or are a
//! `<call>`, so an ordinary session's acks do not flood the log.

use std::fmt::Write as _;

use whatsapp_rust::wacore_binary::{NodeContentRef, NodeRef};

/// How deep the description walks, so a deep or hostile stanza cannot produce an unbounded log.
const MAX_DEPTH: usize = 5;

/// How many nodes one description may render in total. Depth alone does not bound a wide stanza,
/// and the formatter runs on the worker's event loop: a large `<call>` must not delay call
/// signaling while the line is built. The remainder is collapsed into a counted marker.
const MAX_NODES: usize = 64;

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

/// The protocol attributes whose value is an enum or a small integer — never a name, a JID or free
/// text — so their exact value can be shown. Everything else stays a name only. This is what lets a
/// rate pair, a media mode or a capability version be compared against the captured offer rather
/// than guessed from presence.
/// The attributes whose value is a JID and nothing else, so they are safe to render by address
/// family and device number. Named explicitly rather than "anything that parses": a call id or a
/// message id can parse as a bare user, and rendering it would put opaque identifiers in the log.
const JID_ATTRS: &[&str] = &["jid", "call-creator", "from", "to", "user_pn"];

const PROTOCOL_ATTRS: &[&str] = &[
    "class",
    "type",
    "error",
    "media",
    "rate",
    "medium",
    "ver",
    "keygen",
    "orientation",
    "device_orientation",
    "protocol",
    "mute-state",
    "reason",
    "priority",
];

/// One attribute rendered for the trace: the exact value for the protocol enums and integers above
/// and for an `error`/`jid`, otherwise just the name. Every other attribute stays a name, so a JID
/// user, a call id, or free text can never be written.
fn render_attr(name: &str, value: &whatsapp_rust::wacore_binary::node::ValueRef<'_>) -> String {
    if JID_ATTRS.contains(&name) {
        return render_jid(name, value);
    }
    if name == "error" || PROTOCOL_ATTRS.contains(&name) {
        let value = value.as_str();
        if safe_token(&value) {
            return format!("{name}={value}");
        }
    }
    format!("{name}=?")
}

/// A `jid` rendered by address family and device number only: `jid=?@lid` for a bare user jid and
/// `jid=?@lid:14` for one that names a specific device. The family comes from the parsed enum and
/// the device is a small index, so nothing the server controls reaches the log and the user is
/// never written — but a creator or roster device that the server expects to be device-specific,
/// and at which number, becomes visible.
fn render_jid(name: &str, value: &whatsapp_rust::wacore_binary::node::ValueRef<'_>) -> String {
    let Some(jid) = value.to_jid() else {
        return format!("{name}=?");
    };
    if jid.device == 0 {
        format!("{name}=?@{}", jid.server.as_str())
    } else {
        format!("{name}=?@{}:{}", jid.server.as_str(), jid.device)
    }
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

/// Protocol metadata for the top-level `ack` envelope: the values of the three attributes whose
/// values are protocol enums or numeric codes (`class`, `type`, `error`) and no others.
///
/// A normal ack for a call carries `class="call"` and `type="<action>"`. An ack that names an
/// `error` code is a rejection, which the group-offer parser currently reads as a malformed
/// snapshot instead of a refusal; seeing the code separates "the server rejected the offer" from
/// "the server's snapshot omits an integer". The value is rendered only when it is a short opaque
/// token — never a JID, a name, or free text — so no PII can pass through.
pub(super) fn ack_metadata(node: &NodeRef<'_>) -> Option<String> {
    if node.tag != "ack" {
        return None;
    }
    let mut out = String::new();
    for name in ["class", "type", "error"] {
        let Some(value) = node.get_attr(name) else {
            continue;
        };
        let value = value.as_str();
        if !safe_token(&value) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{name}={value}");
    }
    (!out.is_empty()).then_some(out)
}

/// The address family and device presence of one of this account's own identities, for the
/// startup line. A group offer's creator entry is addressed by this account's LID, so whether that
/// LID carries the linked device's suffix — or is bare, i.e. device zero — is the one fact the
/// creator entry depends on and no other log states. The user is not written.
pub(super) fn identity_shape(jid: Option<&whatsapp_rust::Jid>) -> String {
    match jid {
        Some(jid) if jid.device != 0 => format!("@{}:{}", jid.server.as_str(), jid.device),
        Some(jid) => format!("@{}", jid.server.as_str()),
        None => "none".to_string(),
    }
}

/// Whether a protocol value is safe to render: a short run of lowercase letters, digits, `_` or `-`.
/// `@`, spaces and any longer or mixed string (names, JIDs, reason text) are refused.
fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// This account's own addressable identities, so a roster entry can be marked `self` without any
/// jid being written. Empty before pairing completes.
pub(super) struct OwnIdentities {
    pub lid: Option<whatsapp_rust::Jid>,
    pub pn: Option<whatsapp_rust::Jid>,
}

impl OwnIdentities {
    /// Whether a node's `jid` is this account's LID or phone number, ignoring the device suffix.
    fn is_self(&self, node: &NodeRef<'_>) -> bool {
        let Some(jid) = node.get_attr("jid").and_then(|value| value.to_jid()) else {
            return false;
        };
        let jid = jid.to_non_ad();
        [self.lid.as_ref(), self.pn.as_ref()]
            .into_iter()
            .flatten()
            .map(|own| own.to_non_ad())
            .any(|own| own == jid)
    }
}

/// Decode one outgoing marshaled stanza and describe it when it is call-plane control.
///
/// The offer this app builds is the request a rejection names, and [`describe`] alone only sees
/// what arrives. `Event::SentFrame` hands over the plaintext of every frame the transport accepted
/// — the outbound counterpart of `Event::RawNode` — so the device the server refuses can be
/// compared against the roster it echoes back.
pub(super) fn describe_sent_frame(plaintext: &[u8], own: Option<&OwnIdentities>) -> Option<String> {
    let node = whatsapp_rust::wacore_binary::marshal::unmarshal_packed_ref(plaintext).ok()?;
    is_call_control(&node).then(|| describe(&node, 0, own))
}

/// The node's structure as text: tag, attribute names and child tags with their own names. A value
/// is rendered only when it is a protocol enum or a small integer (an `error` code, a `media` mode,
/// a `rate`, a `medium`, a `ver`, …) or a `jid`, which is rendered as address family and device
/// number only. No JID user, call id, name or free text is ever written. A `<user>` whose jid is the
/// account's own is marked `self`.
pub(super) fn describe(node: &NodeRef<'_>, depth: usize, own: Option<&OwnIdentities>) -> String {
    let mut out = String::new();
    let mut budget = MAX_NODES;
    write_node(node, depth, &mut budget, &mut out, own);
    out
}

fn write_node(
    node: &NodeRef<'_>,
    depth: usize,
    budget: &mut usize,
    out: &mut String,
    own: Option<&OwnIdentities>,
) {
    if depth > MAX_DEPTH || *budget == 0 {
        let _ = writeln!(out, "{}…", "  ".repeat(depth));
        return;
    }
    *budget -= 1;
    let _ = write!(out, "{}<{}", "  ".repeat(depth), node.tag);
    let mut names: Vec<(&str, String)> = node
        .attrs_iter()
        .map(|(name, value)| (&**name, render_attr(name, value)))
        .collect();
    // Sorted, so two stanzas compare by reading rather than by remembering the wire order.
    names.sort_unstable_by_key(|(name, _)| *name);
    for (_, rendered) in &names {
        let _ = write!(out, " {rendered}");
    }
    out.push('>');
    if node.tag == "user" && own.is_some_and(|own| own.is_self(node)) {
        out.push_str(" self");
    }
    match &node.content {
        Some(NodeContentRef::Nodes(children)) => {
            let _ = write!(out, " children={}", children.len());
            let mut skipped = 0usize;
            for child in children.iter() {
                if *budget == 0 {
                    skipped += 1;
                    continue;
                }
                out.push('\n');
                write_node(child, depth + 1, budget, out, own);
            }
            if skipped > 0 {
                let _ = write!(
                    out,
                    "\n{}… {skipped} more not shown",
                    "  ".repeat(depth + 1)
                );
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::{Jid, NodeBuilder, Server, wacore_binary::Node};

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

    /// The description names every attribute the parser looks at: a protocol enum or integer shows
    /// its value, and everything else (the call id) is a name only, so the missing integer is
    /// visible as an absent name while the call id stays out of the log.
    #[test]
    fn the_description_names_attributes_and_renders_only_protocol_values() {
        let ack = group_ack(false);
        let text = describe(&ack.as_node_ref(), 0, None);
        assert!(text.contains("<ack"), "{text}");
        assert!(text.contains("<group_info"), "{text}");
        // The media mode is a protocol token and is rendered exactly.
        assert!(text.contains("media=audio"), "{text}");
        // The call id is not: its value never appears and it stays a name.
        assert!(text.contains("call-id=?"), "{text}");
        assert!(!text.contains("x"), "the call id leaked: {text}");
        assert!(
            !text.contains("connected-limit"),
            "the missing integer must be absent by name: {text}"
        );

        let with_limit = group_ack(true);
        let text = describe(&with_limit.as_node_ref(), 0, None);
        assert!(
            text.contains("connected-limit=?"),
            "a present integer is visible by name: {text}"
        );
    }

    /// The ack metadata names the envelope's protocol enums and codes, and refuses anything that
    /// could be an identifier or free text, so a rejection code is visible without a JID leaking.
    #[test]
    fn ack_metadata_shows_protocol_tokens_and_redacts_identities() {
        let ack = NodeBuilder::new("ack")
            .attr("class", "call")
            .attr("type", "offer")
            .attr("error", "439")
            .attr("id", "3EB0ABCDEF")
            .attr("from", "100001@s.whatsapp.net")
            .build();
        assert_eq!(
            ack_metadata(&ack.as_node_ref()).as_deref(),
            Some("class=call type=offer error=439")
        );

        // A free-text error (a reason string, possibly with a name) is refused entirely.
        let text_error = NodeBuilder::new("ack")
            .attr("class", "call")
            .attr("type", "offer")
            .attr("error", "Bad Request")
            .build();
        assert_eq!(
            ack_metadata(&text_error.as_node_ref()).as_deref(),
            Some("class=call type=offer")
        );

        let jid_error = NodeBuilder::new("ack")
            .attr("error", "100001@s.whatsapp.net")
            .build();
        assert_eq!(ack_metadata(&jid_error.as_node_ref()), None);

        // Not an ack envelope: no metadata at all.
        let group_info = NodeBuilder::new("group_info")
            .attr("media", "audio")
            .build();
        assert_eq!(ack_metadata(&group_info.as_node_ref()), None);
    }

    /// A server error code is the one value the trace renders, on any node, and only when it is a
    /// short opaque token: a free-text or JID-shaped error stays masked.
    #[test]
    fn only_a_safe_error_code_is_rendered() {
        let ack = NodeBuilder::new("ack")
            .attr("type", "offer")
            .attr("error", "427")
            .children([NodeBuilder::new("group_info")
                .attr("media", "audio")
                .children([NodeBuilder::new("user")
                    .attr("error", "427")
                    .attr("jid", "x")
                    .build()])
                .build()])
            .build();
        let text = describe(&ack.as_node_ref(), 0, None);
        assert!(text.contains("error=427"), "{text}");
        // `type` is a protocol enum and is rendered; a jid never is beyond family/device.
        assert!(text.contains("type=offer"), "{text}");
        assert!(text.contains("jid=?"), "{text}");
        // The code appears on both the envelope and the participant the server named.
        assert_eq!(text.matches("error=427").count(), 2, "{text}");

        let text_error = NodeBuilder::new("ack").attr("error", "Bad Reason").build();
        assert!(describe(&text_error.as_node_ref(), 0, None).contains("error=?"));
    }

    /// A roster entry that is this account is marked `self`, and no jid is written for it or for
    /// anyone else.
    #[test]
    fn a_roster_entry_matching_this_account_is_marked_self() {
        let own = OwnIdentities {
            lid: Some(Jid::new("555", Server::Lid)),
            pn: Some(Jid::new("15550000001", Server::Pn)),
        };
        let ack = NodeBuilder::new("ack")
            .children([NodeBuilder::new("group_info")
                .attr("media", "audio")
                .children([
                    NodeBuilder::new("user")
                        .attr("jid", Jid::new("555", Server::Lid).with_device(3))
                        .attr("error", "427")
                        .build(),
                    NodeBuilder::new("user")
                        .attr("jid", Jid::new("999", Server::Lid))
                        .build(),
                ])
                .build()])
            .build();
        let text = describe(&ack.as_node_ref(), 0, Some(&own));
        assert_eq!(
            text.matches(" self").count(),
            1,
            "only the own entry: {text}"
        );
        for jid in ["555", "999"] {
            assert!(!text.contains(jid), "a jid leaked: {text}");
        }
    }

    /// A jid is named by its address family and whether it carries a device, never by its user, so
    /// a self device that the server refuses for lacking a device suffix is distinguishable.
    #[test]
    fn a_jid_is_rendered_by_family_and_device_presence_only() {
        let ack = NodeBuilder::new("ack")
            .children([
                NodeBuilder::new("user")
                    .attr("jid", Jid::lid_device("555", 3))
                    .build(),
                NodeBuilder::new("user")
                    .attr("jid", Jid::lid("555"))
                    .build(),
                NodeBuilder::new("user")
                    .attr("jid", Jid::pn("15550000001"))
                    .build(),
                NodeBuilder::new("user")
                    .attr("jid", Jid::new("1234", Server::Group))
                    .build(),
            ])
            .build();
        let text = describe(&ack.as_node_ref(), 0, None);
        assert!(text.contains("jid=?@lid:3"), "{text}");
        assert_eq!(text.matches("jid=?@lid>").count(), 1, "{text}");
        assert!(text.contains("jid=?@s.whatsapp.net"), "{text}");
        assert!(text.contains("jid=?@g.us"), "{text}");
        for user in ["555", "15550000001", "1234"] {
            assert!(!text.contains(user), "a jid user leaked: {text}");
        }
    }

    /// A JID-valued protocol attribute (`call-creator`, `from`, `to`) is rendered by family and
    /// device number, while a call id that could parse as a bare user is not rendered at all.
    #[test]
    fn a_jid_valued_attribute_is_rendered_and_a_call_id_is_not() {
        let offer = NodeBuilder::new("call")
            .attr("to", "00DD63A26643DC3496FCBD161E6E2AB1@call")
            .attr("id", "20350.27209-809")
            .children([NodeBuilder::new("offer")
                .attr("call-id", "00DD63A26643DC3496FCBD161E6E2AB1")
                .attr("call-creator", "156535032389744:14@lid")
                .build()])
            .build();
        let text = describe(&offer.as_node_ref(), 0, None);
        assert!(text.contains("call-creator=?@lid:14"), "{text}");
        assert!(text.contains("to=?@call"), "{text}");
        assert!(text.contains("call-id=?"), "{text}");
        assert!(text.contains("id=?"), "{text}");
        for opaque in ["00DD63A26643DC3496FCBD161E6E2AB1", "20350.27209-809"] {
            assert!(!text.contains(opaque), "an identifier leaked: {text}");
        }
    }

    /// The outbound offer is decoded and described when it is call-plane control, and a sent
    /// stanza that is not is dropped — the send side is otherwise invisible.
    #[test]
    fn a_sent_offer_is_described_and_a_sent_non_call_is_not() {
        let offer = NodeBuilder::new("call")
            .attr("to", "x@call")
            .children([NodeBuilder::new("offer")
                .attr("call-id", "x")
                .attr("call-creator", "555@lid")
                .children([NodeBuilder::new("group_info")
                    .attr("media", "audio")
                    .children([NodeBuilder::new("user")
                        .attr("jid", Jid::lid("555"))
                        .children([NodeBuilder::new("device")
                            .attr("jid", Jid::lid("555"))
                            .build()])
                        .build()])
                    .build()])
                .build()])
            .build();
        let packed = whatsapp_rust::wacore_binary::marshal::marshal(&offer).expect("marshal offer");
        let text = describe_sent_frame(&packed, None).expect("a sent offer is traced");
        assert!(text.contains("<call"), "{text}");
        assert!(text.contains("<device"), "{text}");

        let plain = NodeBuilder::new("message").attr("id", "1").build();
        let packed =
            whatsapp_rust::wacore_binary::marshal::marshal(&plain).expect("marshal message");
        assert_eq!(describe_sent_frame(&packed, None), None);
    }

    /// The startup line names the family and device presence of an own identity — bare versus
    /// device-suffixed — and says `none` before pairing.
    #[test]
    fn identity_shape_names_family_and_device_presence() {
        assert_eq!(identity_shape(None), "none");
        assert_eq!(
            identity_shape(Some(&Jid::lid("555"))),
            "@lid",
            "a bare LID must be visible as such"
        );
        assert_eq!(identity_shape(Some(&Jid::lid_device("555", 3))), "@lid:3");
        assert_eq!(
            identity_shape(Some(&Jid::pn("15550000001"))),
            "@s.whatsapp.net"
        );
    }

    /// A wide stanza is collapsed once the node budget is spent, so a hostile or merely large
    /// `<call>` cannot make the worker build an unbounded log line on the signaling path.
    #[test]
    fn a_wide_stanza_is_truncated_and_marked() {
        let wide: Vec<Node> = (0..(MAX_NODES * 2))
            .map(|index| {
                NodeBuilder::new("user")
                    .attr("jid", format!("p{index}"))
                    .build()
            })
            .collect();
        let ack = NodeBuilder::new("ack").children(wide).build();
        let text = describe(&ack.as_node_ref(), 0, None);
        assert!(text.contains("more not shown"), "{text}");
        assert!(
            text.matches("<user").count() < MAX_NODES + 1,
            "the budget must cap rendered nodes: {text}"
        );
    }
}
