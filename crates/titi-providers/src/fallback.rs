//! Fallback routing between models, decided at a turn boundary.
//!
//! Two shapes share one type:
//!
//! - **one-shot** (the default, unchanged): hand out the first backup once,
//!   never cycle, no clock involved;
//! - **chained**: walk an ordered chain, park a model that just failed for a
//!   cooldown, and revert to the highest-priority model whose cooldown has
//!   expired.
//!
//! In both shapes:
//!
//! - advancing is legal **only at a turn boundary** (between turns), never
//!   mid-stream: switching after visible deltas would break replay of
//!   thinking blocks;
//! - only retryable failures (429, server errors, stalls) advance the chain,
//!   a permanent failure stops the turn where it is.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smol_str::SmolStr;

use crate::transport::{ApiKind, TransportError};

/// One candidate in the chain.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub provider: SmolStr,
    pub model: SmolStr,
    pub api: ApiKind,
}

impl ModelRef {
    pub fn new(provider: impl Into<SmolStr>, model: impl Into<SmolStr>, api: ApiKind) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            api,
        }
    }
}

/// Monotonic time source behind the cooldowns, injected so a test can move
/// time without sleeping.
pub trait Clock: fmt::Debug + Send + Sync {
    fn now(&self) -> Instant;
}

/// Wall-clock source used in production.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Hand-advanced clock: time only moves when [`ManualClock::advance`] is
/// called.
#[derive(Debug)]
pub struct ManualClock {
    base: Instant,
    offset_millis: AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset_millis: AtomicU64::new(0),
        }
    }

    /// Move the clock forward. Saturates instead of wrapping.
    pub fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        let _ = self
            .offset_millis
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(millis))
            });
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let offset = Duration::from_millis(self.offset_millis.load(Ordering::Relaxed));
        self.base.checked_add(offset).unwrap_or(self.base)
    }
}

/// Why a chained fallback could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackChainError {
    /// A chain needs a positive cooldown: with a zero one a rate-limited
    /// model would be back in rotation on the very next turn.
    ZeroCooldown,
}

impl fmt::Display for FallbackChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FallbackChainError::ZeroCooldown => {
                write!(f, "fallback cooldown must be greater than zero")
            }
        }
    }
}

impl std::error::Error for FallbackChainError {}

/// How the chain decides where to go next.
#[derive(Debug, Clone)]
enum Mode {
    /// Today's behaviour: at most one advance, no cooldown, no revert.
    OneShot { activated: bool },
    /// Multi-step: cool a failed entry down and prefer the highest-priority
    /// entry that is not cooling.
    Chain {
        cooldown: Duration,
        /// Per-entry cooldown expiry, indexed like [`FallbackChain::entry`].
        cooling: Vec<Option<Instant>>,
        clock: Arc<dyn Clock>,
    },
}

/// Ordered fallback chain over model handles.
///
/// The default entry type is [`ModelRef`]; the engine drives the same chain
/// over bare model ids.
#[derive(Debug, Clone)]
pub struct FallbackChain<T = ModelRef> {
    primary: T,
    backups: Vec<T>,
    /// `0` is the primary, `i + 1` is `backups[i]`.
    active: usize,
    mode: Mode,
}

impl<T: Clone> FallbackChain<T> {
    /// One-shot chain: the historical behaviour, and the default when no
    /// chain is configured.
    pub fn new(primary: T, backups: Vec<T>) -> Self {
        Self {
            primary,
            backups,
            active: 0,
            mode: Mode::OneShot { activated: false },
        }
    }

    /// Multi-step chain with a cooldown per entry.
    pub fn chained(
        primary: T,
        backups: Vec<T>,
        cooldown: Duration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, FallbackChainError> {
        if cooldown.is_zero() {
            return Err(FallbackChainError::ZeroCooldown);
        }
        let cooling = vec![None; 1 + backups.len()];
        Ok(Self {
            primary,
            backups,
            active: 0,
            mode: Mode::Chain {
                cooldown,
                cooling,
                clock,
            },
        })
    }

    fn total(&self) -> usize {
        1 + self.backups.len()
    }

    /// Entry at `idx`, where `0` is the primary. An out-of-range index is
    /// impossible by construction and reads as the primary.
    fn entry(&self, idx: usize) -> &T {
        match idx.checked_sub(1) {
            None => &self.primary,
            Some(i) => self.backups.get(i).unwrap_or(&self.primary),
        }
    }

    /// Currently selected entry (primary until the chain moves).
    pub fn current(&self) -> &T {
        self.entry(self.active)
    }

    /// Whether the chain is running on something other than the primary.
    pub fn is_activated(&self) -> bool {
        match &self.mode {
            Mode::OneShot { activated } => *activated,
            Mode::Chain { .. } => self.active != 0,
        }
    }

    /// Entry to use for the next turn, called at a turn boundary before the
    /// turn starts.
    ///
    /// A chain reverts here: as soon as a higher-priority entry's cooldown
    /// has expired, routing goes back to it. When every entry is still
    /// cooling the current one is kept — something has to carry the turn.
    /// A one-shot chain never moves on its own.
    pub fn select(&mut self) -> &T {
        let total = self.total();
        let target = match &self.mode {
            Mode::OneShot { .. } => None,
            Mode::Chain { cooling, clock, .. } => {
                first_available(total, cooling, clock.now(), None)
            }
        };
        if let Some(idx) = target {
            self.active = idx;
        }
        self.current()
    }

    /// Called at a turn boundary after a failed turn. Returns the next entry
    /// to try, or `None` when the failure is permanent or nothing else is
    /// available.
    ///
    /// In a chain the entry that just failed is parked for the cooldown, and
    /// the highest-priority entry that is not cooling takes over — which is
    /// the primary again once its own cooldown has expired.
    pub fn next(&mut self, err: &TransportError) -> Option<T> {
        if !err.is_retryable() {
            return None;
        }
        let failed = self.active;
        let total = self.total();
        let next_idx = match &mut self.mode {
            Mode::OneShot { activated } => {
                if *activated || failed + 1 >= total {
                    None
                } else {
                    *activated = true;
                    Some(failed + 1)
                }
            }
            Mode::Chain {
                cooldown,
                cooling,
                clock,
            } => {
                let now = clock.now();
                if let Some(slot) = cooling.get_mut(failed) {
                    *slot = Some(now.checked_add(*cooldown).unwrap_or(now));
                }
                first_available(total, cooling, now, Some(failed))
            }
        }?;
        self.active = next_idx;
        Some(self.entry(next_idx).clone())
    }
}

/// Highest-priority index that is not cooling at `now`, skipping `skip`.
fn first_available(
    total: usize,
    cooling: &[Option<Instant>],
    now: Instant,
    skip: Option<usize>,
) -> Option<usize> {
    (0..total).find(|idx| {
        Some(*idx) != skip
            && cooling
                .get(*idx)
                .copied()
                .flatten()
                .is_none_or(|until| until <= now)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn chain() -> FallbackChain {
        FallbackChain::new(
            ModelRef::new("primary", "m1", ApiKind::OpenAiCompletions),
            vec![
                ModelRef::new("backup-a", "m2", ApiKind::AnthropicMessages),
                ModelRef::new("backup-b", "m3", ApiKind::OpenAiResponses),
            ],
        )
    }

    const COOLDOWN: Duration = Duration::from_secs(60);

    fn cooling_chain() -> (FallbackChain<SmolStr>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let chain = FallbackChain::chained(
            SmolStr::new("primary"),
            vec![SmolStr::new("backup-a"), SmolStr::new("backup-b")],
            COOLDOWN,
            clock.clone(),
        )
        .expect("positive cooldown");
        (chain, clock)
    }

    fn rate_limited() -> TransportError {
        TransportError::Retryable {
            status: Some(429),
            message: "rate limited".into(),
        }
    }

    fn fatal() -> TransportError {
        TransportError::Fatal {
            status: Some(401),
            message: "bad key".into(),
        }
    }

    #[test]
    fn rate_limit_advances_to_first_backup() {
        let mut c = chain();
        assert_eq!(c.current().provider, SmolStr::new("primary"));
        let next = c.next(&rate_limited()).expect("should advance");
        assert_eq!(next.provider, SmolStr::new("backup-a"));
        assert_eq!(c.current().provider, SmolStr::new("backup-a"));
        assert!(c.is_activated());
    }

    #[test]
    fn fatal_error_does_not_advance() {
        let mut c = chain();
        assert!(c.next(&fatal()).is_none());
        assert_eq!(c.current().provider, SmolStr::new("primary"));
        assert!(!c.is_activated());
    }

    #[test]
    fn one_shot_never_cycles() {
        let mut c = chain();
        assert!(c.next(&rate_limited()).is_some());
        // One-shot: even a retryable error never advances again.
        assert!(c.next(&rate_limited()).is_none());
        assert_eq!(c.current().provider, SmolStr::new("backup-a"));
    }

    #[test]
    fn exhausted_chain_returns_none() {
        let mut c = FallbackChain::new(
            ModelRef::new("p", "m", ApiKind::OpenAiCompletions),
            vec![ModelRef::new("b", "m", ApiKind::OpenAiCompletions)],
        );
        assert!(c.next(&rate_limited()).is_some());
        assert!(c.next(&rate_limited()).is_none());
    }

    #[test]
    fn empty_backups_never_advance() {
        let mut c = FallbackChain::new(
            ModelRef::new("p", "m", ApiKind::OpenAiCompletions),
            Vec::new(),
        );
        assert!(c.next(&rate_limited()).is_none());
        assert!(!c.is_activated());
    }

    #[test]
    fn stall_error_is_retryable() {
        let mut c = chain();
        let stalled = TransportError::Stalled {
            phase: crate::transport::StallPhase::Idle,
        };
        assert!(c.next(&stalled).is_some());
    }

    #[test]
    fn mid_stream_refusal_is_caller_contract() {
        // The chain itself has no clock; the turn-boundary rule is that a
        // caller which saw deltas must NOT call `next()` — surfaced here by
        // documenting + exercising the fatal-after-content path: providers
        // translate mid-stream breakage into a non-retryable turn error.
        let mut c = chain();
        let mid_stream = TransportError::Fatal {
            status: None,
            message: "stream broke after deltas".into(),
        };
        assert!(c.next(&mid_stream).is_none());
        assert_eq!(c.current().provider, SmolStr::new("primary"));
    }

    #[test]
    fn rate_limit_advances_step_by_step_through_the_chain() {
        let (mut c, _clock) = cooling_chain();
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-a")));
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-b")));
        // Every entry is cooling now: nothing left to hand out.
        assert_eq!(c.next(&rate_limited()), None);
        assert_eq!(c.current(), &SmolStr::new("backup-b"));
    }

    #[test]
    fn an_entry_still_cooling_down_is_skipped() {
        let (mut c, clock) = cooling_chain();
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-a")));
        clock.advance(COOLDOWN / 2);
        // The primary is still cooling, so the chain moves on instead of back.
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-b")));
        assert_eq!(c.select(), &SmolStr::new("backup-b"));
    }

    #[test]
    fn an_expired_cooldown_reverts_to_the_primary() {
        let (mut c, clock) = cooling_chain();
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-a")));
        clock.advance(COOLDOWN + Duration::from_secs(1));
        assert_eq!(c.select(), &SmolStr::new("primary"));
        assert!(!c.is_activated());
    }

    #[test]
    fn a_permanent_error_neither_advances_nor_cools() {
        let (mut c, _clock) = cooling_chain();
        assert_eq!(c.next(&fatal()), None);
        assert_eq!(c.current(), &SmolStr::new("primary"));
        // Nothing was parked, so the next turn still starts on the primary.
        assert_eq!(c.select(), &SmolStr::new("primary"));
    }

    #[test]
    fn a_chain_without_backups_is_one_shot() {
        let clock = Arc::new(ManualClock::new());
        let mut c =
            FallbackChain::chained(SmolStr::new("solo"), Vec::new(), COOLDOWN, clock.clone())
                .expect("positive cooldown");
        assert_eq!(c.next(&rate_limited()), None);
        clock.advance(COOLDOWN * 2);
        assert_eq!(c.select(), &SmolStr::new("solo"));
    }

    #[test]
    fn a_zero_cooldown_is_rejected() {
        let clock = Arc::new(ManualClock::new());
        let built = FallbackChain::chained(
            SmolStr::new("primary"),
            vec![SmolStr::new("backup-a")],
            Duration::ZERO,
            clock,
        );
        assert_eq!(built.unwrap_err(), FallbackChainError::ZeroCooldown);
    }

    #[test]
    fn everything_cooling_keeps_the_current_entry() {
        let (mut c, clock) = cooling_chain();
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-a")));
        assert_eq!(c.next(&rate_limited()), Some(SmolStr::new("backup-b")));
        assert_eq!(c.next(&rate_limited()), None);
        clock.advance(COOLDOWN / 2);
        assert_eq!(c.select(), &SmolStr::new("backup-b"));
    }
}
