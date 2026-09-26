//! Surface fold — port of deepseek `packages/core/session/src/surface.ts`.
//!
//! `foldSurface` (`surface.ts:600`) reduces the event log to surface nodes +
//! the projected model-visible messages. Replace semantics
//! (`types.ts:453-464`): `{op:'replace', startSeq, endSeq}` replaces surface
//! nodes startSeq..endSeq INCLUSIVE with this node; both must currently
//! exist; tool/result replacements must rewrite exactly one node
//! (`assertToolResultRewrite`, `surface.ts:465`).

use super::event::{Seq, SessionEvent, SurfaceOp};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SurfaceError {
    #[error("replace range [{start},{end}] references missing surface nodes")]
    MissingReplacementTarget { start: Seq, end: Seq },
    #[error("tool/result replace must rewrite exactly one node (got range [{start},{end}])")]
    ToolResultRewriteNotSingle { start: Seq, end: Seq },
}

/// One model-visible surface node.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceNode {
    pub seq: Seq,
    pub event: SessionEvent,
}

/// `SurfaceFoldResult` (`surface.ts:241-249`).
#[derive(Debug, Clone, Default)]
pub struct SurfaceFoldResult {
    pub nodes: Vec<SurfaceNode>,
    /// (replacement_seq, [replaced_seqs]) pairs, in log order.
    pub replacements: Vec<(Seq, Vec<Seq>)>,
}

impl SurfaceFoldResult {
    /// The projected model-visible messages, in surface order.
    pub fn projected_messages(&self) -> Vec<&SessionEvent> {
        self.nodes.iter().map(|n| &n.event).collect()
    }
}

/// `foldSurface` (`surface.ts:600`): reduce events to surface nodes.
pub fn fold_surface(events: &[SessionEvent]) -> Result<SurfaceFoldResult, SurfaceError> {
    let mut result = SurfaceFoldResult::default();
    for event in events {
        match &event.surface_op {
            None => {
                if is_surface(&event.event_type) {
                    // surface events always append without an op is invalid,
                    // but validation happens upstream; treat defensively as append
                    result.nodes.push(SurfaceNode { seq: event.seq, event: event.clone() });
                }
            }
            Some(SurfaceOp::Append) => {
                result.nodes.push(SurfaceNode { seq: event.seq, event: event.clone() });
            }
            Some(SurfaceOp::Replace { start_seq, end_seq }) => {
                let start_idx = result
                    .nodes
                    .iter()
                    .position(|n| n.seq == *start_seq)
                    .ok_or(SurfaceError::MissingReplacementTarget {
                        start: *start_seq,
                        end: *end_seq,
                    })?;
                let end_idx = result
                    .nodes
                    .iter()
                    .position(|n| n.seq == *end_seq)
                    .ok_or(SurfaceError::MissingReplacementTarget {
                        start: *start_seq,
                        end: *end_seq,
                    })?;
                if start_idx > end_idx {
                    return Err(SurfaceError::MissingReplacementTarget {
                        start: *start_seq,
                        end: *end_seq,
                    });
                }
                if event.event_type == "tool/result" && end_idx != start_idx {
                    // assertToolResultRewrite (surface.ts:465)
                    return Err(SurfaceError::ToolResultRewriteNotSingle {
                        start: *start_seq,
                        end: *end_seq,
                    });
                }
                let replaced: Vec<Seq> = result.nodes[start_idx..=end_idx]
                    .iter()
                    .map(|n| n.seq)
                    .collect();
                result.nodes.splice(
                    start_idx..=end_idx,
                    std::iter::once(SurfaceNode { seq: event.seq, event: event.clone() }),
                );
                result.replacements.push((event.seq, replaced));
            }
        }
    }
    Ok(result)
}

fn is_surface(ty: &str) -> bool {
    super::event::is_surface_event_type(ty)
}
