//! CEngine calendar and booking logic (draft v1). Pure functions on instants and wall-clock
//! times; no I/O and no clock reads, so every host gets the same answers.
//!
//! Rules, all covered by tests:
//! 1. Instants are UTC. Recurrence and working hours are wall-clock times in an IANA time
//!    zone, so "every Monday 09:00" stays at 09:00 local across daylight-saving changes.
//! 2. A wall-clock time that does not exist (spring forward) keeps the offset from before the
//!    gap, so it moves forward by the gap's length; one that happens twice (fall back) uses
//!    the first occurrence.
//! 3. Intervals are half-open `[start, end)`: a booking that ends when another starts does
//!    not conflict with it.
//! 4. Recurrence accepts a strict subset of RFC 5545 RRULE; unknown or conflicting parts are
//!    errors, never ignored. Every expansion is bounded by a window and a result cap.

mod interval;
mod rrule;
mod slots;

pub use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc, Weekday};
pub use chrono_tz::Tz;
pub use interval::{conflicts, Interval};
pub use rrule::{expand, Freq, Recurrence, Until};
pub use slots::{available_slots, WorkingHours};

/// Longest window an expansion or slot search may cover.
pub const MAX_WINDOW_DAYS: i64 = 366;
/// Most results one call returns.
pub const MAX_RESULTS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    /// `end` is not after `start`.
    EmptyInterval,
    BadTimeZone(String),
    /// An RRULE problem: the part that is wrong.
    BadRule(String),
    WindowTooLarge,
    BadStep,
    BadBuffer,
}

/// Looks up an IANA time zone name such as `America/New_York`.
pub fn parse_tz(name: &str) -> Result<Tz, CalendarError> {
    name.parse::<Tz>()
        .map_err(|_| CalendarError::BadTimeZone(name.to_string()))
}

/// Resolves a local wall-clock time in `tz` to an instant (rule 2).
pub(crate) fn resolve_local(tz: Tz, local: NaiveDateTime) -> DateTime<Utc> {
    use chrono::offset::LocalResult;
    use chrono::TimeZone;
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => t.with_timezone(&Utc),
        LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
        LocalResult::None => {
            // In the gap: keep the offset that was in force before it, which moves the time
            // forward by the length of the gap (02:30 becomes 03:30 when 02:00 jumps to 03:00).
            // Gaps are usually one hour but can be a whole day (Pacific/Apia, 2011), so keep
            // stepping back until a valid time is found.
            use chrono::Offset;
            let mut probe = local;
            for _ in 0..(14 * 6) {
                probe -= Duration::hours(4);
                if let Some(before) = tz.from_local_datetime(&probe).earliest() {
                    return local.and_utc()
                        - Duration::seconds(i64::from(before.offset().fix().local_minus_utc()));
                }
            }
            // Unreachable for real zones; fall back to reading the time as UTC.
            local.and_utc()
        }
    }
}
