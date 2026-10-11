use crate::{
    resolve_local, CalendarError, DateTime, Duration, Interval, NaiveDate, NaiveDateTime, Tz, Utc,
    Weekday, MAX_RESULTS, MAX_WINDOW_DAYS,
};
use chrono::Datelike;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
}

/// The supported subset of an RRULE: `FREQ`, `INTERVAL`, `BYDAY` (weekly), `BYMONTHDAY`
/// (monthly), and at most one of `COUNT` or `UNTIL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub freq: Freq,
    pub interval: u32,
    pub by_day: Vec<Weekday>,
    pub by_month_day: Option<u32>,
    pub count: Option<u32>,
    pub until: Option<Until>,
}

/// End of a recurrence. A date is inclusive by the event's local date; an instant (UTC
/// timestamp) is inclusive of an occurrence starting exactly at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    Date(NaiveDate),
    Instant(DateTime<Utc>),
}

fn bad(msg: impl Into<String>) -> CalendarError {
    CalendarError::BadRule(msg.into())
}

fn weekday(s: &str) -> Result<Weekday, CalendarError> {
    Ok(match s {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return Err(bad(format!("BYDAY {s}"))),
    })
}

impl Recurrence {
    /// Parses text such as `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10`.
    pub fn parse(text: &str) -> Result<Recurrence, CalendarError> {
        let (mut freq, mut interval, mut count, mut until) = (None, 1u32, None, None);
        let (mut by_day, mut by_month_day): (Vec<Weekday>, Option<u32>) = (Vec::new(), None);
        let mut seen = Vec::new();
        for part in text.split(';') {
            let (key, value) = part.split_once('=').ok_or_else(|| bad(part))?;
            if seen.contains(&key) {
                return Err(bad(format!("{key} repeated")));
            }
            seen.push(key);
            match key {
                "FREQ" => {
                    freq = Some(match value {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        _ => return Err(bad(format!("FREQ {value}"))),
                    })
                }
                "INTERVAL" => {
                    interval = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (1..=366).contains(n))
                        .ok_or_else(|| bad("INTERVAL must be 1 to 366"))?
                }
                "COUNT" => {
                    count = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| (1..=MAX_RESULTS as u32).contains(n))
                            .ok_or_else(|| bad("COUNT must be 1 to 1000"))?,
                    )
                }
                "UNTIL" => {
                    until = Some(if value.len() == 8 {
                        Until::Date(
                            NaiveDate::parse_from_str(value, "%Y%m%d")
                                .map_err(|_| bad("UNTIL must be YYYYMMDD or YYYYMMDDTHHMMSSZ"))?,
                        )
                    } else {
                        Until::Instant(
                            NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
                                .map_err(|_| bad("UNTIL must be YYYYMMDD or YYYYMMDDTHHMMSSZ"))?
                                .and_utc(),
                        )
                    })
                }
                "BYDAY" => {
                    for d in value.split(',') {
                        let wd = weekday(d)?;
                        if by_day.contains(&wd) {
                            return Err(bad("BYDAY repeats a day"));
                        }
                        by_day.push(wd);
                    }
                }
                "BYMONTHDAY" => {
                    by_month_day = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| (1..=31).contains(n))
                            .ok_or_else(|| bad("BYMONTHDAY must be 1 to 31"))?,
                    )
                }
                other => return Err(bad(format!("{other} is not supported"))),
            }
        }
        let freq = freq.ok_or_else(|| bad("FREQ is required"))?;
        if count.is_some() && until.is_some() {
            return Err(bad("COUNT and UNTIL cannot both be set"));
        }
        if !by_day.is_empty() && freq != Freq::Weekly {
            return Err(bad("BYDAY only applies to WEEKLY"));
        }
        if by_month_day.is_some() && freq != Freq::Monthly {
            return Err(bad("BYMONTHDAY only applies to MONTHLY"));
        }
        let rule = Recurrence {
            freq,
            interval,
            by_day,
            by_month_day,
            count,
            until,
        };
        rule.validate()?;
        Ok(rule)
    }

    /// Checks the constraints `parse` enforces; `expand` calls it so a hand-built value cannot
    /// bypass them.
    pub fn validate(&self) -> Result<(), CalendarError> {
        if !(1..=366).contains(&self.interval) {
            return Err(bad("INTERVAL must be 1 to 366"));
        }
        if self
            .count
            .is_some_and(|c| !(1..=MAX_RESULTS as u32).contains(&c))
        {
            return Err(bad("COUNT must be 1 to 1000"));
        }
        if self.count.is_some() && self.until.is_some() {
            return Err(bad("COUNT and UNTIL cannot both be set"));
        }
        if !self.by_day.is_empty() && self.freq != Freq::Weekly {
            return Err(bad("BYDAY only applies to WEEKLY"));
        }
        for (i, d) in self.by_day.iter().enumerate() {
            if self.by_day[..i].contains(d) {
                return Err(bad("BYDAY repeats a day"));
            }
        }
        if self.by_month_day.is_some() && self.freq != Freq::Monthly {
            return Err(bad("BYMONTHDAY only applies to MONTHLY"));
        }
        if self.by_month_day.is_some_and(|n| !(1..=31).contains(&n)) {
            return Err(bad("BYMONTHDAY must be 1 to 31"));
        }
        Ok(())
    }
}

/// Local dates of the rule's occurrences from `first` onward, in order, ending when `stop`
/// says so. `first` is always the first occurrence, as RFC 5545 requires.
fn dates(
    rule: &Recurrence,
    first: NaiveDate,
    mut keep_going: impl FnMut(NaiveDate) -> bool,
) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut push = |d: NaiveDate, out: &mut Vec<NaiveDate>| -> bool {
        if rule.count.is_some_and(|c| out.len() as u32 >= c) {
            return false;
        }
        if !keep_going(d) {
            return false;
        }
        out.push(d);
        true
    };
    match rule.freq {
        Freq::Daily => {
            let mut d = first;
            while push(d, &mut out) {
                match d.checked_add_signed(Duration::days(i64::from(rule.interval))) {
                    Some(n) => d = n,
                    None => break,
                }
            }
        }
        Freq::Weekly => {
            let days: Vec<Weekday> = if rule.by_day.is_empty() {
                vec![first.weekday()]
            } else {
                rule.by_day.clone()
            };
            // Weeks start on Monday; the first week is the one containing `first`.
            let week0 = first - Duration::days(i64::from(first.weekday().num_days_from_monday()));
            let mut week = 0i64;
            'outer: loop {
                let monday = week0 + Duration::days(week * 7 * i64::from(rule.interval));
                let mut in_week: Vec<NaiveDate> = days
                    .iter()
                    .map(|wd| monday + Duration::days(i64::from(wd.num_days_from_monday())))
                    .filter(|d| *d >= first)
                    .collect();
                in_week.sort();
                for d in in_week {
                    if !push(d, &mut out) {
                        break 'outer;
                    }
                }
                week += 1;
                if week > 100_000 {
                    break;
                }
            }
        }
        Freq::Monthly => {
            let day = rule.by_month_day.unwrap_or(first.day());
            let (mut y, mut m) = (first.year(), first.month());
            let mut guard = 0;
            loop {
                // Months without that day (for example the 31st) are skipped, as in RFC 5545.
                if let Some(d) = NaiveDate::from_ymd_opt(y, m, day) {
                    if d >= first && !push(d, &mut out) {
                        break;
                    }
                }
                let total = (y * 12 + m as i32 - 1) + rule.interval as i32;
                y = total.div_euclid(12);
                m = total.rem_euclid(12) as u32 + 1;
                guard += 1;
                if guard > 100_000 || y > 9999 {
                    break;
                }
            }
        }
    }
    out
}

/// Occurrences of `rule` starting at local time `first_local` in `tz`, whose instants fall
/// inside `window`, in order, at most [`MAX_RESULTS`]. Each result is the start instant; add
/// the event's duration for its end. The window may span at most [`MAX_WINDOW_DAYS`].
pub fn expand(
    rule: &Recurrence,
    first_local: NaiveDateTime,
    tz: Tz,
    window: &Interval,
) -> Result<Vec<DateTime<Utc>>, CalendarError> {
    rule.validate()?;
    if window.duration() > Duration::days(MAX_WINDOW_DAYS) {
        return Err(CalendarError::WindowTooLarge);
    }
    let time = first_local.time();
    let end = window.end;
    // Count and until apply from the first occurrence, so expansion always starts there and
    // stops once past the window.
    let past_until = |d: NaiveDate, t: DateTime<Utc>| match rule.until {
        Some(Until::Date(u)) => d > u,
        Some(Until::Instant(u)) => t > u,
        None => false,
    };
    let dates = dates(rule, first_local.date(), |d| {
        let t = resolve_local(tz, d.and_time(time));
        t < end && !past_until(d, t)
    });
    Ok(dates
        .into_iter()
        .map(|d| resolve_local(tz, d.and_time(time)))
        .filter(|t| *t >= window.start && *t < window.end)
        .take(MAX_RESULTS)
        .collect())
}
