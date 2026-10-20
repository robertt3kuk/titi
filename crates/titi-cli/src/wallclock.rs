//! Local wall-clock time for the transcript's speaker rows.
//!
//! The standard library knows the time in UTC and nothing about the user's
//! zone, and the workspace forbids `unsafe` (no `localtime_r`) and takes no
//! time crate. The zone's offset therefore comes from the system's own
//! `date +%z`, asked once and kept for a few minutes: a row stamped a minute
//! after a daylight-saving change is the only thing that can be wrong, and the
//! refresh bounds that. A system without `date` leaves the stamp out instead
//! of showing a time in the wrong zone.

use std::fmt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a zone offset read from `date` is trusted.
const OFFSET_TTL: Duration = Duration::from_secs(600);

/// A local time of day to the minute: `14:02`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub hour: u8,
    pub minute: u8,
}

impl Stamp {
    /// The time of day `epoch_secs` is at, in a zone `offset_secs` east of UTC.
    pub fn from_epoch(epoch_secs: i64, offset_secs: i64) -> Self {
        let day = epoch_secs.saturating_add(offset_secs).rem_euclid(86_400);
        Self {
            hour: (day / 3_600) as u8,
            minute: (day % 3_600 / 60) as u8,
        }
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A zone offset as `date +%z` prints it — `+0530`, `-0800` — in seconds.
pub fn parse_offset(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, digits) = match text.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    if minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// The local time now, or `None` when the system will not say which zone it is
/// in.
pub fn now() -> Option<Stamp> {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let offset = local_offset()?;
    Some(Stamp::from_epoch(
        i64::try_from(epoch.as_secs()).ok()?,
        offset,
    ))
}

/// The cached zone offset. The failure is cached too, so a machine without
/// `date` does not spawn a process per transcript line.
fn local_offset() -> Option<i64> {
    static CACHE: Mutex<Option<(Instant, Option<i64>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    if let Some((read_at, offset)) = *cache
        && read_at.elapsed() < OFFSET_TTL
    {
        return offset;
    }
    let offset = Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| parse_offset(&String::from_utf8_lossy(&output.stdout)));
    *cache = Some((Instant::now(), offset));
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_is_the_local_time_of_day() {
        // 2026-10-21T12:02:30Z
        let epoch = 1_792_584_150;
        assert_eq!(Stamp::from_epoch(epoch, 0).to_string(), "12:02");
        assert_eq!(Stamp::from_epoch(epoch, 2 * 3_600).to_string(), "14:02");
        assert_eq!(Stamp::from_epoch(epoch, -(8 * 3_600)).to_string(), "04:02");
        // Half-hour zones and the wrap past midnight.
        assert_eq!(
            Stamp::from_epoch(epoch, 5 * 3_600 + 1_800).to_string(),
            "17:32"
        );
        assert_eq!(Stamp::from_epoch(epoch, 13 * 3_600).to_string(), "01:02");
    }

    #[test]
    fn the_zone_offset_is_read_the_way_date_prints_it() {
        assert_eq!(parse_offset("+0200\n"), Some(7_200));
        assert_eq!(parse_offset("-0830"), Some(-(8 * 3_600 + 1_800)));
        assert_eq!(parse_offset("+0000"), Some(0));
        for bad in ["", "0200", "+02", "+02:00", "+0261", "UTC", "+02a0"] {
            assert_eq!(parse_offset(bad), None, "{bad:?}");
        }
    }
}
