//! The session's reasoning effort: what a model is sent for it, and what the
//! engine tells a surface when it changes.

use titi_providers::{Effort, RequestOpts, resolve_reasoning};

use crate::protocol::EngineEvent;
use crate::runtime::TransportResolver;

/// The level `model` is sent for `effort`, or `None` when nothing is: no
/// effort asked for, a model that takes none, or one that cannot be resolved
/// (a turn on it fails with its own, better error).
pub(crate) fn applied(
    resolver: &dyn TransportResolver,
    model: &str,
    effort: Option<Effort>,
) -> Option<Effort> {
    let resolved = resolver.resolve(model).ok()?;
    resolve_reasoning(
        resolved.transport.api(),
        resolved.reasoning.as_ref(),
        &RequestOpts { effort },
    )
    .map(|reasoning| reasoning.effort)
}

/// The report for moving a session's effort from `previous` to `next` while
/// `model` is primary. `has_history` is whether a request has anything cached
/// to lose: a change that alters nothing on the wire, or one made before the
/// first request, costs nothing.
pub(crate) fn changed(
    resolver: &dyn TransportResolver,
    model: &str,
    previous: Option<Effort>,
    next: Option<Effort>,
    has_history: bool,
) -> EngineEvent {
    let before = applied(resolver, model, previous);
    let after = applied(resolver, model, next);
    EngineEvent::EffortChanged {
        effort: next,
        applied: after,
        cache_miss: has_history && before != after,
    }
}
