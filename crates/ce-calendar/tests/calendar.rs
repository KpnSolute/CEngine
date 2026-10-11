use ce_calendar::*;
use chrono::{TimeZone, Utc};

fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, mo, d)
        .unwrap()
        .and_hms_opt(h, mi, 0)
        .unwrap()
}

fn win(a: DateTime<Utc>, b: DateTime<Utc>) -> Interval {
    Interval::new(a, b).unwrap()
}

fn ny() -> Tz {
    parse_tz("America/New_York").unwrap()
}

fn expand_text(rule: &str, first: NaiveDateTime, tz: Tz, w: &Interval) -> Vec<DateTime<Utc>> {
    expand(&Recurrence::parse(rule).unwrap(), first, tz, w).unwrap()
}

// The expected values in the recurrence tests were produced by python-dateutil and zoneinfo,
// independent implementations of RFC 5545 and the IANA database.

#[test]
fn weekly_stays_at_nine_local_across_the_spring_dst_change() {
    let w = win(t(2026, 3, 1, 0, 0), t(2026, 4, 1, 0, 0));
    let got = expand_text(
        "FREQ=WEEKLY;BYDAY=MO;COUNT=6",
        local(2026, 3, 2, 9, 0),
        ny(),
        &w,
    );
    assert_eq!(
        got,
        [
            t(2026, 3, 2, 14, 0),
            t(2026, 3, 9, 13, 0),
            t(2026, 3, 16, 13, 0),
            t(2026, 3, 23, 13, 0),
            t(2026, 3, 30, 13, 0)
        ]
    );
}

#[test]
fn weekly_interval_and_several_days() {
    let w = win(t(2026, 3, 1, 0, 0), t(2026, 4, 1, 0, 0));
    let got = expand_text(
        "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=6",
        local(2026, 3, 4, 9, 0),
        ny(),
        &w,
    );
    assert_eq!(
        got,
        [
            t(2026, 3, 4, 14, 0),
            t(2026, 3, 16, 13, 0),
            t(2026, 3, 18, 13, 0),
            t(2026, 3, 30, 13, 0)
        ]
    );
}

#[test]
fn a_time_inside_the_spring_gap_keeps_the_earlier_offset() {
    let w = win(t(2026, 3, 1, 0, 0), t(2026, 4, 1, 0, 0));
    let got = expand_text("FREQ=DAILY;COUNT=4", local(2026, 3, 7, 2, 30), ny(), &w);
    // 2026-03-08 02:30 does not exist in New York: it becomes 03:30 EDT = 07:30Z.
    assert_eq!(
        got,
        [
            t(2026, 3, 7, 7, 30),
            t(2026, 3, 8, 7, 30),
            t(2026, 3, 9, 6, 30),
            t(2026, 3, 10, 6, 30)
        ]
    );
}

#[test]
fn an_ambiguous_fall_back_time_uses_the_first_occurrence() {
    let w = win(t(2026, 10, 30, 0, 0), t(2026, 11, 5, 0, 0));
    let got = expand_text("FREQ=DAILY;COUNT=4", local(2026, 10, 31, 1, 30), ny(), &w);
    assert_eq!(
        got,
        [
            t(2026, 10, 31, 5, 30),
            t(2026, 11, 1, 5, 30),
            t(2026, 11, 2, 6, 30),
            t(2026, 11, 3, 6, 30)
        ]
    );
}

#[test]
fn monthly_skips_months_without_that_day() {
    let w = win(t(2026, 1, 1, 0, 0), t(2027, 1, 1, 0, 0));
    let utc = parse_tz("UTC").unwrap();
    let got = expand_text(
        "FREQ=MONTHLY;BYMONTHDAY=31;COUNT=7",
        local(2026, 1, 31, 12, 0),
        utc,
        &w,
    );
    assert_eq!(
        got,
        [
            t(2026, 1, 31, 12, 0),
            t(2026, 3, 31, 12, 0),
            t(2026, 5, 31, 12, 0),
            t(2026, 7, 31, 12, 0),
            t(2026, 8, 31, 12, 0),
            t(2026, 10, 31, 12, 0),
            t(2026, 12, 31, 12, 0)
        ]
    );
    let got = expand_text(
        "FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=15;COUNT=5",
        local(2026, 1, 15, 12, 0),
        utc,
        &w,
    );
    assert_eq!(
        got,
        [
            t(2026, 1, 15, 12, 0),
            t(2026, 3, 15, 12, 0),
            t(2026, 5, 15, 12, 0),
            t(2026, 7, 15, 12, 0),
            t(2026, 9, 15, 12, 0)
        ]
    );
}

#[test]
fn count_counts_from_the_first_occurrence_not_the_window() {
    // 10 daily occurrences starting 2026-01-01; a window in the middle sees only part of them.
    let utc = parse_tz("UTC").unwrap();
    let w = win(t(2026, 1, 8, 0, 0), t(2026, 2, 1, 0, 0));
    let got = expand_text("FREQ=DAILY;COUNT=10", local(2026, 1, 1, 8, 0), utc, &w);
    assert_eq!(
        got,
        [
            t(2026, 1, 8, 8, 0),
            t(2026, 1, 9, 8, 0),
            t(2026, 1, 10, 8, 0)
        ]
    );
}

#[test]
fn until_is_inclusive_and_open_rules_end_at_the_window() {
    let utc = parse_tz("UTC").unwrap();
    let w = win(t(2026, 1, 1, 0, 0), t(2026, 2, 1, 0, 0));
    let got = expand_text(
        "FREQ=DAILY;UNTIL=20260103",
        local(2026, 1, 1, 8, 0),
        utc,
        &w,
    );
    assert_eq!(got.len(), 3);
    let got = expand_text("FREQ=DAILY", local(2026, 1, 1, 8, 0), utc, &w);
    assert_eq!(got.len(), 31);
}

#[test]
fn results_and_windows_are_bounded() {
    let utc = parse_tz("UTC").unwrap();
    let rule = Recurrence::parse("FREQ=DAILY").unwrap();
    let too_big = win(t(2026, 1, 1, 0, 0), t(2027, 1, 3, 0, 0));
    assert_eq!(
        expand(&rule, local(2026, 1, 1, 8, 0), utc, &too_big),
        Err(CalendarError::WindowTooLarge)
    );
    // A rule that began long ago still expands without error.
    let w = win(t(2026, 1, 1, 0, 0), t(2026, 1, 8, 0, 0));
    assert_eq!(
        expand(&rule, local(2000, 1, 1, 8, 0), utc, &w)
            .unwrap()
            .len(),
        7
    );
}

#[test]
fn rules_that_are_not_understood_are_errors() {
    for bad in [
        "",
        "FREQ=YEARLY",
        "FREQ=DAILY;COUNT=0",
        "FREQ=DAILY;COUNT=1001",
        "FREQ=DAILY;COUNT=3;UNTIL=20260101",
        "FREQ=DAILY;BYDAY=MO",
        "FREQ=WEEKLY;BYMONTHDAY=3",
        "FREQ=DAILY;INTERVAL=0",
        "FREQ=DAILY;INTERVAL=367",
        "FREQ=DAILY;FREQ=WEEKLY",
        "FREQ=WEEKLY;BYDAY=MO,MO",
        "FREQ=WEEKLY;BYDAY=XX",
        "FREQ=DAILY;BYSETPOS=1",
        "FREQ=DAILY;UNTIL=2026",
        "INTERVAL=2",
        "FREQ",
    ] {
        assert!(
            matches!(Recurrence::parse(bad), Err(CalendarError::BadRule(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn intervals_are_half_open() {
    let a = win(t(2026, 1, 1, 9, 0), t(2026, 1, 1, 10, 0));
    let b = win(t(2026, 1, 1, 10, 0), t(2026, 1, 1, 11, 0));
    assert!(!a.overlaps(&b), "ends when the other starts: no clash");
    let c = win(t(2026, 1, 1, 9, 59), t(2026, 1, 1, 10, 30));
    assert!(a.overlaps(&c) && c.overlaps(&b));
    assert_eq!(
        Interval::new(t(2026, 1, 1, 9, 0), t(2026, 1, 1, 9, 0)),
        Err(CalendarError::EmptyInterval)
    );
}

#[test]
fn conflicts_report_which_bookings_clash_and_respect_buffers() {
    let existing = [
        win(t(2026, 1, 1, 9, 0), t(2026, 1, 1, 10, 0)),
        win(t(2026, 1, 1, 12, 0), t(2026, 1, 1, 13, 0)),
    ];
    let p = win(t(2026, 1, 1, 10, 0), t(2026, 1, 1, 11, 0));
    assert_eq!(
        conflicts(&p, &existing, Duration::zero()).unwrap(),
        Vec::<usize>::new()
    );
    assert_eq!(
        conflicts(&p, &existing, Duration::minutes(15)).unwrap(),
        vec![0]
    );
    assert_eq!(
        conflicts(&p, &existing, Duration::minutes(61)).unwrap(),
        vec![0, 1]
    );
    assert_eq!(
        conflicts(&p, &existing, Duration::minutes(-1)),
        Err(CalendarError::BadBuffer)
    );
    assert_eq!(
        conflicts(&p, &existing, Duration::days(2)),
        Err(CalendarError::BadBuffer)
    );
}

fn nine_to_noon_weekdays() -> WorkingHours {
    let (from, to) = (
        NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
    );
    let mut h = WorkingHours::new();
    for d in [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
    ] {
        h = h.open(d, from, to).unwrap();
    }
    h
}

#[test]
fn slots_follow_working_hours_in_local_time_and_skip_busy_time() {
    // Monday 2026-03-02, New York (EST, UTC-5): 09:00-12:00 local = 14:00-17:00Z.
    let w = win(t(2026, 3, 2, 0, 0), t(2026, 3, 3, 0, 0));
    let busy = [win(t(2026, 3, 2, 15, 0), t(2026, 3, 2, 15, 30))];
    let got = available_slots(
        &nine_to_noon_weekdays(),
        ny(),
        &w,
        &busy,
        Duration::minutes(60),
        Duration::minutes(30),
        Duration::zero(),
    )
    .unwrap();
    let starts: Vec<_> = got.iter().map(|s| s.start).collect();
    // 14:00, 14:30, 15:00(busy), 15:30 ok, 16:00 ok; 16:30 would run past 17:00.
    // 14:30-15:30 and 15:00-16:00 overlap the busy 15:00-15:30.
    assert_eq!(
        starts,
        [
            t(2026, 3, 2, 14, 0),
            t(2026, 3, 2, 15, 30),
            t(2026, 3, 2, 16, 0)
        ]
    );
    assert!(got.iter().all(|s| s.duration() == Duration::minutes(60)));
}

#[test]
fn slots_shift_with_daylight_saving() {
    // Monday 2026-03-09 is after the US spring change: 09:00 local = 13:00Z.
    let w = win(t(2026, 3, 9, 0, 0), t(2026, 3, 10, 0, 0));
    let got = available_slots(
        &nine_to_noon_weekdays(),
        ny(),
        &w,
        &[],
        Duration::hours(3),
        Duration::hours(1),
        Duration::zero(),
    )
    .unwrap();
    assert_eq!(got, [win(t(2026, 3, 9, 13, 0), t(2026, 3, 9, 16, 0))]);
}

#[test]
fn closed_days_have_no_slots_and_buffers_widen_busy_time() {
    // 2026-03-07 is a Saturday: closed.
    let w = win(t(2026, 3, 7, 0, 0), t(2026, 3, 8, 0, 0));
    assert!(available_slots(
        &nine_to_noon_weekdays(),
        ny(),
        &w,
        &[],
        Duration::minutes(30),
        Duration::minutes(30),
        Duration::zero()
    )
    .unwrap()
    .is_empty());
    let w = win(t(2026, 3, 2, 14, 0), t(2026, 3, 2, 17, 0));
    let busy = [win(t(2026, 3, 2, 15, 0), t(2026, 3, 2, 16, 0))];
    let plain = available_slots(
        &nine_to_noon_weekdays(),
        ny(),
        &w,
        &busy,
        Duration::hours(1),
        Duration::hours(1),
        Duration::zero(),
    )
    .unwrap();
    let buffered = available_slots(
        &nine_to_noon_weekdays(),
        ny(),
        &w,
        &busy,
        Duration::hours(1),
        Duration::hours(1),
        Duration::minutes(5),
    )
    .unwrap();
    assert_eq!(plain.len(), 2);
    assert!(buffered.is_empty());
}

#[test]
fn slot_inputs_are_validated() {
    let w = win(t(2026, 3, 2, 0, 0), t(2026, 3, 3, 0, 0));
    let h = nine_to_noon_weekdays();
    assert_eq!(
        available_slots(
            &h,
            ny(),
            &w,
            &[],
            Duration::zero(),
            Duration::minutes(30),
            Duration::zero()
        ),
        Err(CalendarError::BadStep)
    );
    assert_eq!(
        available_slots(
            &h,
            ny(),
            &w,
            &[],
            Duration::minutes(30),
            Duration::seconds(10),
            Duration::zero()
        ),
        Err(CalendarError::BadStep)
    );
    let huge = win(t(2026, 1, 1, 0, 0), t(2027, 6, 1, 0, 0));
    assert_eq!(
        available_slots(
            &h,
            ny(),
            &huge,
            &[],
            Duration::minutes(30),
            Duration::minutes(30),
            Duration::zero()
        ),
        Err(CalendarError::WindowTooLarge)
    );
    let n = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
    assert!(WorkingHours::new()
        .open(Weekday::Mon, n(12, 0), n(9, 0))
        .is_err());
    assert!(WorkingHours::new()
        .open(Weekday::Mon, n(9, 0), n(12, 0))
        .unwrap()
        .open(Weekday::Mon, n(11, 0), n(13, 0))
        .is_err());
    assert!(matches!(
        parse_tz("Mars/Olympus"),
        Err(CalendarError::BadTimeZone(_))
    ));
}

#[test]
fn until_timestamp_is_enforced_to_the_second() {
    let utc = parse_tz("UTC").unwrap();
    let w = win(t(2026, 1, 1, 0, 0), t(2026, 2, 1, 0, 0));
    // Day 3 at 08:00 is after 07:59:59, so only two occurrences remain.
    let got = expand_text(
        "FREQ=DAILY;UNTIL=20260103T075959Z",
        local(2026, 1, 1, 8, 0),
        utc,
        &w,
    );
    assert_eq!(got.len(), 2);
    let got = expand_text(
        "FREQ=DAILY;UNTIL=20260103T080000Z",
        local(2026, 1, 1, 8, 0),
        utc,
        &w,
    );
    assert_eq!(got.len(), 3);
    for bad in [
        "FREQ=DAILY;UNTIL=20260103T99Z",
        "FREQ=DAILY;UNTIL=20260103T256161Z",
        "FREQ=DAILY;UNTIL=20260103X080000Z",
        "FREQ=DAILY;UNTIL=20260103T080000",
    ] {
        assert!(matches!(
            Recurrence::parse(bad),
            Err(CalendarError::BadRule(_))
        ));
    }
}

#[test]
fn hand_built_rules_are_validated_on_expand() {
    let utc = parse_tz("UTC").unwrap();
    let w = win(t(2026, 1, 1, 0, 0), t(2026, 1, 8, 0, 0));
    let mut rule = Recurrence::parse("FREQ=DAILY").unwrap();
    rule.interval = 0;
    assert!(matches!(
        expand(&rule, local(2026, 1, 1, 8, 0), utc, &w),
        Err(CalendarError::BadRule(_))
    ));
    let mut rule = Recurrence::parse("FREQ=DAILY").unwrap();
    rule.count = Some(5);
    rule.until = Some(Until::Date(NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()));
    assert!(matches!(
        expand(&rule, local(2026, 1, 1, 8, 0), utc, &w),
        Err(CalendarError::BadRule(_))
    ));
}

#[test]
fn a_day_long_gap_resolves_to_the_pre_gap_offset() {
    // Samoa skipped 2011-12-30 entirely (UTC-10 to UTC+14).
    let apia = parse_tz("Pacific/Apia").unwrap();
    let w = win(t(2011, 12, 29, 0, 0), t(2011, 12, 31, 12, 0));
    let got = expand_text("FREQ=DAILY;COUNT=3", local(2011, 12, 29, 12, 0), apia, &w);
    assert_eq!(got[0], t(2011, 12, 29, 22, 0));
    assert_eq!(got[1], t(2011, 12, 30, 22, 0));
}

#[test]
fn ambiguous_hour_slots_are_offered_once() {
    let tz = ny();
    let mut hours = WorkingHours::new();
    hours = hours
        .open(
            Weekday::Sun,
            NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            NaiveTime::from_hms_opt(4, 0, 0).unwrap(),
        )
        .unwrap();
    // 2026-11-01 is the fall-back Sunday in New York.
    let w = win(t(2026, 11, 1, 0, 0), t(2026, 11, 1, 12, 0));
    let got = available_slots(
        &hours,
        tz,
        &w,
        &[],
        Duration::hours(1),
        Duration::hours(1),
        Duration::zero(),
    )
    .unwrap();
    let starts: Vec<_> = got.iter().map(|s| s.start).collect();
    let mut dedup = starts.clone();
    dedup.dedup();
    assert_eq!(starts, dedup);
    assert_eq!(
        starts.len(),
        4,
        "00:00, 01:00, 02:00, 03:00 local, each once"
    );
    assert!(starts.windows(2).all(|p| p[0] < p[1]));
}

#[test]
fn bad_buffer_is_rejected_even_on_closed_days() {
    let hours = WorkingHours::new();
    let w = win(t(2026, 1, 1, 0, 0), t(2026, 1, 2, 0, 0));
    assert_eq!(
        available_slots(
            &hours,
            ny(),
            &w,
            &[],
            Duration::hours(1),
            Duration::hours(1),
            Duration::minutes(-1),
        ),
        Err(CalendarError::BadBuffer)
    );
}
