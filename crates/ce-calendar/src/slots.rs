use crate::{
    conflicts, interval::check_buffer, resolve_local, CalendarError, Duration, Interval, NaiveTime,
    Tz, Weekday, MAX_RESULTS, MAX_WINDOW_DAYS,
};
use chrono::Datelike;
use std::collections::BTreeMap;

/// Opening hours as local wall-clock ranges per weekday. A day with no entry is closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingHours {
    days: BTreeMap<u32, Vec<(NaiveTime, NaiveTime)>>,
}

impl WorkingHours {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an opening range `[from, to)` on `day`. Ranges must be non-empty and must not
    /// overlap one already on that day.
    pub fn open(
        mut self,
        day: Weekday,
        from: NaiveTime,
        to: NaiveTime,
    ) -> Result<Self, CalendarError> {
        if to <= from {
            return Err(CalendarError::EmptyInterval);
        }
        let ranges = self.days.entry(day.num_days_from_monday()).or_default();
        if ranges.iter().any(|(a, b)| from < *b && *a < to) {
            return Err(CalendarError::EmptyInterval);
        }
        ranges.push((from, to));
        ranges.sort();
        Ok(self)
    }
}

/// Bookable slots of length `duration` inside `window`, starting every `step`, within the
/// working hours of `tz`, and not clashing with `busy` (each busy interval extended by
/// `buffer`). Slots are in time order and at most [`MAX_RESULTS`].
pub fn available_slots(
    hours: &WorkingHours,
    tz: Tz,
    window: &Interval,
    busy: &[Interval],
    duration: Duration,
    step: Duration,
    buffer: Duration,
) -> Result<Vec<Interval>, CalendarError> {
    if window.duration() > Duration::days(MAX_WINDOW_DAYS) {
        return Err(CalendarError::WindowTooLarge);
    }
    if step < Duration::minutes(1)
        || step > Duration::days(1)
        || duration < Duration::minutes(1)
        || duration > Duration::days(1)
    {
        return Err(CalendarError::BadStep);
    }
    check_buffer(buffer)?;
    let mut out = Vec::new();
    // One day of slack either side: the local date can differ from the UTC date.
    let first = window.start.with_timezone(&tz).date_naive() - Duration::days(1);
    let last = window.end.with_timezone(&tz).date_naive() + Duration::days(1);
    let mut date = first;
    while date <= last {
        if let Some(ranges) = hours.days.get(&date.weekday().num_days_from_monday()) {
            for (from, to) in ranges {
                let close = resolve_local(tz, date.and_time(*to));
                // Candidates advance in local wall-clock time and are resolved one by one, so
                // a repeated hour (fall back) is offered once, at its first occurrence. A
                // candidate that resolves to an instant already offered (the spring gap) is
                // skipped.
                let mut local = date.and_time(*from);
                let mut last: Option<crate::DateTime<chrono::Utc>> = None;
                loop {
                    let start = resolve_local(tz, local);
                    if start + duration > close {
                        break;
                    }
                    if last.is_none_or(|l| start > l) {
                        last = Some(start);
                        let slot = Interval {
                            start,
                            end: start + duration,
                        };
                        if slot.start >= window.start
                            && slot.end <= window.end
                            && conflicts(&slot, busy, buffer)?.is_empty()
                        {
                            out.push(slot);
                            if out.len() >= MAX_RESULTS {
                                return Ok(out);
                            }
                        }
                    }
                    local += step;
                }
            }
        }
        date += Duration::days(1);
    }
    out.sort_by_key(|s| s.start);
    Ok(out)
}
