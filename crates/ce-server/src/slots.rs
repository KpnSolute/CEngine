//! Free booking slots for one resource (`GET .../_slots`), wired to `ce-calendar`.
//!
//! Uses the `bookings` tool's entities: `resource` (with `timezone`, `buffer_minutes` and `hours`)
//! and `booking` (`resource`, `starts_at`, `ends_at`, `recurrence`, `status`). Requested and
//! confirmed bookings are busy; cancelled, completed and no-show ones are not. A recurring booking
//! is expanded inside the window.

use ce_calendar::{
    available_slots, expand, parse_tz, CalendarError, DateTime, Duration, Interval, NaiveTime,
    Recurrence, Utc, Weekday, WorkingHours,
};
use ce_manifest::Manifest;
use serde_json::Value;

/// Whether the manifest carries the entities the endpoint needs.
pub(crate) fn enabled(m: &Manifest) -> bool {
    m.value["entities"]["resource"]["fields"]["hours"].is_object()
        && m.value["entities"]["booking"]["fields"]["recurrence"].is_object()
}

/// `(path, detail)` for a value that cannot be used.
pub(crate) type Bad = (String, String);

fn bad(path: &str, detail: impl Into<String>) -> Bad {
    (path.to_string(), detail.into())
}

pub(crate) fn instant(path: &str, text: &str) -> Result<DateTime<Utc>, Bad> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| {
            bad(
                path,
                "an RFC 3339 date and time such as 2026-10-12T09:00:00Z",
            )
        })
}

fn cal(path: &str, e: CalendarError) -> Bad {
    let why = match e {
        CalendarError::EmptyInterval => "the end must be after the start".to_string(),
        CalendarError::BadTimeZone(z) => format!("unknown time zone {z}"),
        CalendarError::BadRule(r) => format!("bad recurrence rule: {r}"),
        CalendarError::WindowTooLarge => format!(
            "the window may span at most {} days",
            ce_calendar::MAX_WINDOW_DAYS
        ),
        CalendarError::BadStep => "duration and step are 1 to 1440 minutes".to_string(),
        CalendarError::BadBuffer => "the buffer is 0 to 1440 minutes".to_string(),
    };
    bad(path, why)
}

/// Opening hours from `{"mon": [["09:00", "12:00"], ["13:00", "17:00"]], ...}`.
pub(crate) fn hours(v: &Value) -> Result<WorkingHours, Bad> {
    let p = "/hours";
    let days = v
        .as_object()
        .ok_or_else(|| bad(p, "the resource has no opening hours"))?;
    let mut out = WorkingHours::new();
    for (name, ranges) in days {
        let day = match name.as_str() {
            "mon" => Weekday::Mon,
            "tue" => Weekday::Tue,
            "wed" => Weekday::Wed,
            "thu" => Weekday::Thu,
            "fri" => Weekday::Fri,
            "sat" => Weekday::Sat,
            "sun" => Weekday::Sun,
            _ => {
                return Err(bad(
                    p,
                    format!("{name} is not mon, tue, wed, thu, fri, sat or sun"),
                ))
            }
        };
        let ranges = ranges
            .as_array()
            .ok_or_else(|| bad(p, format!("{name} must be a list of [from, to] pairs")))?;
        for r in ranges {
            let time = |i: usize| {
                r.get(i)
                    .and_then(Value::as_str)
                    .and_then(|s| NaiveTime::parse_from_str(s, "%H:%M").ok())
                    .ok_or_else(|| bad(p, format!("{name}: times are HH:MM")))
            };
            out = out
                .open(day, time(0)?, time(1)?)
                .map_err(|_| bad(p, format!("{name}: a range is empty or overlaps another")))?;
        }
    }
    Ok(out)
}

/// The free slots for `resource` (its record data) given its `bookings` (record data).
pub(crate) fn free(
    resource: &Value,
    bookings: &[Value],
    window: &Interval,
    duration: Duration,
    step: Duration,
) -> Result<Vec<Interval>, Bad> {
    let tz =
        parse_tz(resource["timezone"].as_str().unwrap_or("")).map_err(|e| cal("/resource", e))?;
    let working = hours(&resource["hours"])?;
    let buffer = match resource["buffer_minutes"].as_i64().unwrap_or(0) {
        m @ 0..=1440 => Duration::minutes(m),
        _ => return Err(cal("/buffer_minutes", CalendarError::BadBuffer)),
    };
    let mut busy = Vec::new();
    for b in bookings {
        let (Some(s), Some(e)) = (b["starts_at"].as_str(), b["ends_at"].as_str()) else {
            continue;
        };
        let (start, end) = (instant("/booking", s)?, instant("/booking", e)?);
        let Ok(first) = Interval::new(start, end) else {
            continue;
        };
        match b["recurrence"].as_str().filter(|r| !r.is_empty()) {
            None => busy.push(first),
            Some(text) => {
                let rule = Recurrence::parse(text).map_err(|e| cal("/booking", e))?;
                // Occurrences starting inside the window, plus a short look-back for ones that
                // began earlier and still run into it (each call stays within the window limit).
                let max = Duration::days(ce_calendar::MAX_WINDOW_DAYS);
                let back = (first.duration() + buffer).min(max);
                let looks = [
                    window.clone(),
                    Interval::new(window.start - back, window.start)
                        .map_err(|e| cal("/window", e))?,
                ];
                for w in &looks {
                    for t in expand(&rule, start.with_timezone(&tz).naive_local(), tz, w)
                        .map_err(|e| cal("/booking", e))?
                    {
                        busy.push(Interval {
                            start: t,
                            end: t + first.duration(),
                        });
                    }
                }
            }
        }
    }
    available_slots(&working, tz, window, &busy, duration, step, buffer)
        .map_err(|e| cal("/duration", e))
}
