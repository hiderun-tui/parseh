//! Agent fork lineage.
//!
//! When a contributor improves, translates, or specialises another
//! contributor's agent, they declare the relationship in
//! [`crate::AgentDefinition::parents`] as a list of [`ParentRef`]s.
//!
//! ## What this enables (V0.2 scope)
//!
//! - Provenance traversal: a peer can walk back from a fork to its
//!   origin to audit "where did this prompt template / knowledge ref
//!   come from?"
//! - Multiple-parents (merge of two forks) is supported. The
//!   receiving network checks for cycles when traversing the
//!   lineage DAG — this crate stores the edges only.
//! - Translation marking: a forks-for-language-port can be
//!   distinguished from a forks-for-improvement claim, so reviewers
//!   know whether the relevant comparison is "did it translate
//!   accurately" or "did it execute faster on the same task."
//!
//! ## What this DOES NOT enable (deferred per maintainer note)
//!
//! - Automatic royalty flow from descendant to ancestor.
//! - Quality-weighted reward distribution.
//! - "Better agents get more PARSEH" mechanics from the user's
//!   original brief — that's V0.3+ work blocked behind adversarial
//!   testing per
//!   [the project notes](the project notes).
//!
//! **Future-maintainer warning:** lineage tracking is the kind of
//! primitive that LOOKS like derivative-work attribution and might
//! tempt a V0.3 implementer to wire it directly into an economic
//! attribution layer. That conflation is the conflict flagged in the
//! design doc — lineage is causal-history metadata, not a royalty
//! contract. Any future economic layer that reads `parents` needs a
//! separate, explicit attribution policy with its own governance
//! review (Rule 8 footnote 4 applies).

use crate::definition::{AgentId, AgentVersion};
use serde::{Deserialize, Serialize};

/// A reference to a parent agent definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentRef {
    /// Parent's content-hash identity.
    pub agent_id: AgentId,
    /// Pinned parent version. Stored explicitly so a parent's
    /// later version bump does not silently re-interpret the
    /// child's lineage claim.
    pub version: AgentVersion,
    /// Why this fork exists. Drives reviewer expectations (a
    /// `Translation` fork should preserve semantics; an
    /// `Improvement` fork claims a measurable delta).
    pub fork_reason: ForkReason,
}

/// Why a fork was made. The variants are coarse on purpose — fine
/// taxonomy belongs in the agent's metadata.description, not in a
/// closed enum that constrains future reasons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkReason {
    /// The fork claims to be faster, more accurate, or more
    /// robust than the parent on the same task. V0.2 does NOT
    /// verify this claim — verification is V0.3+ (the
    /// quality-scoring layer that's gated behind adversarial
    /// testing). Reviewers should treat the claim as
    /// self-described until then.
    Improvement,
    /// The fork translates the parent into a different language
    /// (e.g., Persian port of an English agent). The
    /// `metadata.languages` field should reflect the change.
    Translation,
    /// The fork narrows the parent's scope (e.g., a general
    /// "summarize_text" parent forked into "summarize_persian_news"
    /// with a Persian news corpus pinned into knowledge_refs).
    Specialisation,
    /// The fork fixes a defect in the parent. No claim of new
    /// capability — strictly the same semantics, more correctly
    /// implemented.
    BugFix,
    /// The fork is a deployment-specific customisation — e.g.,
    /// different model_requirements, different default seeds.
    Customisation,
    /// Free-text escape hatch. Use sparingly — closed enums are
    /// easier to filter on.
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{from_cbor_bytes, to_cbor_bytes, ContentHash};

    #[test]
    fn parent_ref_roundtrip_through_cbor() {
        let p = ParentRef {
            agent_id: AgentId(ContentHash::zero()),
            version: AgentVersion::new(1, 2, 3),
            fork_reason: ForkReason::Improvement,
        };
        let bytes = to_cbor_bytes(&p).unwrap();
        let back: ParentRef = from_cbor_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn fork_reason_other_carries_payload_through_cbor() {
        let p = ParentRef {
            agent_id: AgentId(ContentHash::zero()),
            version: AgentVersion::new(0, 1, 0),
            fork_reason: ForkReason::Other("audit-trail rebuild".into()),
        };
        let bytes = to_cbor_bytes(&p).unwrap();
        let back: ParentRef = from_cbor_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }
}
