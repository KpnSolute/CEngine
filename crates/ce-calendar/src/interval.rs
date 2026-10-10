use crate::{CalendarError, DateTime, Duration, Utc};

/// A half-open span of time `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Interval {
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Interval, CalendarError> {
        if end <= start {
            return Err(CalendarError::EmptyInterval);
        }
        Ok(Interval { start, end })
    }

    pub fn overlaps(&self, other: &Interval) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn duration(&self) -> Duration {
        self.end - self.start
    }
}

/// Indexes of `existing` intervals that clash with `proposed`, treating each existing
/// interval as extended by `buffer` on both sides (travel or cleanup time).
pub fn conflicts(
    proposed: &Interval,
    existing: &[Interval],
    buffer: Duration,
) -> Result<Vec<usize>, CalendarError> {
    check_buffer(buffer)?;
    Ok(existing
        .iter()
        .enumerate()
        .filter(|(_, e)| proposed.start < e.end + buffer && e.start - buffer < proposed.end)
        .map(|(i, _)| i)
        .collect())
}

/// A buffer is between zero and one day.
pub(crate) fn check_buffer(buffer: Duration) -> Result<(), CalendarError> {
    if buffer < Duration::zero() || buffer > Duration::days(1) {
        return Err(CalendarError::BadBuffer);
    }
    Ok(())
}
