//! Pacing a streamed answer so it arrives at a readable rate.
//!
//! A provider sends an answer in bursts: a fast model, a coalesced flush and a
//! tool round all land as a wall of text at once, and a screen that draws every
//! delta the moment it arrives jumps. This paces the *reveal* — how much of the
//! text received so far is on screen — at omp's rate
//! (`pi-coding-agent/src/modes/controllers/streaming-reveal.ts`).
//!
//! The core is one pure function, [`reveal`]: how much of the buffer is shown
//! after a given time, from what was shown before. Nothing here reads a clock,
//! so a test drives it with explicit durations and a late or coalesced tick
//! catches up instead of falling behind.
//!
//! The unit is a **character**, and that is the invariant it can honestly hold:
//! the revealed text is always a prefix of the received text, cut at a `char`
//! boundary, so a multi-byte character is never split. A grapheme cluster of
//! several characters (a ZWJ emoji) can still be revealed in parts — titi
//! tracks no grapheme iterator, and omp's own cluster counter is not something
//! this build has.
//!
//! `display.smoothStreaming` is **off** unless the config asks for it, where
//! omp defaults it on: an unset key here never moves a pixel, so the goldens
//! and every existing frame stay byte for byte what they were.

use std::time::Duration;

/// The frame the reveal paces at: 30 fps, omp's `STREAMING_REVEAL_FRAME_MS`.
pub(crate) const FRAME: Duration = Duration::from_millis(33);

/// The least a frame reveals (omp's `MIN_STEP`).
const MIN_STEP: usize = 3;

/// The frames a backlog takes to drain (omp's `CATCHUP_FRAMES`): a burst is
/// caught up within about eight frames, which is also the lag bound —
/// [`reveal`] never trails the buffer by more than this many frames.
const CATCHUP_FRAMES: usize = 8;

/// Characters one frame reveals for a backlog of `backlog`.
///
/// omp's formula, kept as it is: `max(3, ceil(backlog / 8))`. A longer backlog
/// reveals proportionally more, so the rate follows the stream instead of
/// crawling at a constant.
pub(crate) fn step(backlog: usize) -> usize {
    backlog.div_ceil(CATCHUP_FRAMES).max(MIN_STEP).max(1)
}

/// How many characters of `buffered` are on screen after `elapsed`, given that
/// `revealed` characters were before.
///
/// `elapsed` is time since the last frame and is read for its *frames*: a tick
/// that arrives late, or two that coalesce, reveals as many frames' worth as
/// they were, so the reveal cannot fall behind a fast stream. The result never
/// exceeds the buffer and never drops below what was already revealed.
pub(crate) fn reveal(buffered: &str, revealed: usize, elapsed: Duration) -> usize {
    let total = buffered.chars().count();
    let revealed = revealed.min(total);
    let backlog = total - revealed;
    if backlog == 0 {
        return revealed;
    }
    let millis = FRAME.as_millis().max(1);
    let frames = (elapsed.as_millis() / millis).max(1) as usize;
    (revealed + step(backlog).saturating_mul(frames)).min(total)
}

/// The byte offset `chars` characters into `text`, clamped to its end: the
/// boundary [`reveal`]'s count is cut at, so a prefix can be taken safely.
pub(crate) fn byte_at(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(at, _)| at)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The step follows the backlog: at least three, and proportional once the
    /// backlog is bigger than the catch-up window.
    #[test]
    fn the_step_follows_the_backlog() {
        assert_eq!(step(0), MIN_STEP, "an empty backlog still steps");
        assert_eq!(step(1), MIN_STEP);
        assert_eq!(step(8), MIN_STEP);
        assert_eq!(step(80), 10);
        assert_eq!(step(800), 100);
    }

    /// The reveal, driven entirely by the durations a caller passes: nothing
    /// here reads a clock.
    #[test]
    fn the_reveal_is_a_pure_function_of_its_three_inputs() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        // One frame of this backlog: the step the formula gives it.
        assert_eq!(reveal(text, 0, FRAME), step(text.len()));
        // No time at all is still one frame: a tick that arrives early must
        // not stall the reveal.
        assert_eq!(reveal(text, 0, Duration::ZERO), step(text.len()));
        // Elapsed time is frames: three frames of the same backlog.
        assert_eq!(reveal(text, 0, FRAME * 3), step(text.len()) * 3);
        // A backlog small enough for the minimum steps the minimum.
        assert_eq!(reveal("abcde", 0, FRAME), MIN_STEP);
        // A long backlog catches up faster, and never past the end.
        assert_eq!(reveal(text, 0, FRAME * CATCHUP_FRAMES as u32), text.len());
        assert_eq!(reveal(text, 0, FRAME * 100), text.len());
        // It never goes backwards, and never past what there is.
        assert_eq!(reveal(text, 10, FRAME), 10 + MIN_STEP);
        assert_eq!(reveal(text, text.len(), FRAME), text.len());
        assert_eq!(reveal("ab", 0, FRAME * 100), 2);
        assert_eq!(reveal("", 0, FRAME), 0);
        // A prefix already revealed stays where it is when there is no more.
        assert_eq!(reveal("abc", 3, FRAME), 3);
    }

    /// The invariant the whole thing rests on: the revealed text is a prefix
    /// of the buffer, cut at a character boundary — never half a character.
    #[test]
    fn the_revealed_text_is_always_a_character_prefix() {
        let text = "héllo — 世界 🎉 done";
        let mut revealed = 0;
        let mut elapsed = Duration::ZERO;
        while revealed < text.chars().count() {
            revealed = reveal(text, revealed, FRAME);
            let at = byte_at(text, revealed);
            assert!(text.is_char_boundary(at), "cut inside a character: {at}");
            let prefix = &text[..at];
            assert!(text.starts_with(prefix), "not a prefix: {prefix:?}");
            assert_eq!(prefix.chars().count(), revealed);
            elapsed += FRAME;
            assert!(elapsed < Duration::from_secs(10), "the loop always ends");
        }
        assert_eq!(byte_at(text, revealed), text.len());
    }

    /// The lag bound, stated as it really is: the reveal never trails the
    /// buffer by more than a catch-up window's worth of characters, and a
    /// burst is therefore on screen within about a second of arriving.
    ///
    /// The bound is not "eight frames in total": the step follows the backlog,
    /// so it shrinks as the backlog does and the drain is geometric — a factor
    /// of 8/7 a frame. For 400 characters that is 28 frames, ~0.9 s at 30 fps,
    /// which is what this holds the formula to.
    #[test]
    fn a_fast_stream_never_lags_past_the_catch_up_window() {
        // 400 characters arrive at once and nothing more does.
        let text = "x".repeat(400);
        let mut revealed = 0;
        let mut frames = 0;
        while revealed < text.len() && frames < 200 {
            // The lag the parent of this loop would see: the step covers the
            // whole backlog within the window, in one frame or several.
            assert!(
                step(text.len() - revealed) * CATCHUP_FRAMES >= text.len() - revealed,
                "a backlog is never worth more than the window: {revealed}"
            );
            revealed = reveal(&text, revealed, FRAME);
            frames += 1;
        }
        assert_eq!(revealed, text.len(), "the buffer is fully revealed");
        assert!(
            frames <= 32,
            "a burst of {} drained in {frames} frames (~{:.1}s)",
            text.len(),
            frames as f64 * 0.033
        );
    }

    /// A character cut in the middle is impossible even when the step lands on
    /// a multi-byte one: the boundary helper is what the screen slices with.
    #[test]
    fn the_boundary_helper_clamps() {
        let text = "aé世";
        assert_eq!(byte_at(text, 0), 0);
        assert_eq!(byte_at(text, 1), 1);
        assert_eq!(byte_at(text, 2), 3, "é is two bytes");
        assert_eq!(byte_at(text, 3), text.len());
        assert_eq!(byte_at(text, 99), text.len());
    }
}
