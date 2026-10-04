//! Minimal five-field cron expression support for the scheduler.
//!
//! Fields: `minute hour day-of-month month day-of-week` with Vixie-style
//! semantics — `*`, `*/step`, `a`, `a-b`, `a-b/step`, comma lists; `7` in the
//! weekday field folds onto Sunday (`0`). No name fields, no seconds, no
//! `@preset` shorthands: the scheduler stores exactly what the UI typed and
//! the UI owns pretty-printing.
//!
//! Semantics pinned here (Vixie cron, "commands are run when either the
//! day-of-month or the day-of-week matches, but only when both restricted
//! fields agree"): when BOTH dom and dow are restricted the day matches on
//! EITHER; when one is `*` only the restricted one decides; both `*` = every
//! day. `next_after` walks day-by-day (month mismatches jump a whole month)
//! and scans the matching day minute-by-minute; nonexistent local times
//! inside a DST spring-forward gap are skipped rather than guessed — the
//! walk continues to the next candidate, so a fire time that lands in the
//! gap on one day never silences the schedule.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, TimeZone, Timelike};

/// Day-walk ceiling. The widest legal gap a fireable shape produces is the
/// dom-only leap day across a non-leap century edge (`0 0 29 2 *`:
/// 2096-02-29 → 2104-02-29, ~8 years ≈ 330 steps); 20_000 keeps a wide
/// safety margin and still terminates never-firing shapes (`0 0 31 2 *`)
/// in a cheap pure memory loop.
const MAX_DAY_STEPS: u32 = 20_000;

/// One parsed cron expression. Bits are set at the value's own index
/// (minute bit 7 == minute 7), so `contains` is a single AND.
#[derive(Debug, Clone, PartialEq)]
pub struct CronExpr {
    minutes: u64,
    hours: u64,
    days_of_month: u64,
    months: u64,
    days_of_week: u64,
    dom_any: bool,
    dow_any: bool,
}

impl CronExpr {
    /// Parses a five-field expression; the error names the offending field.
    pub fn parse(text: &str) -> Result<CronExpr, String> {
        let text = text.trim();
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron needs exactly 5 fields (minute hour day-of-month month day-of-week), got {fields_len}: '{text}'",
                fields_len = fields.len()
            ));
        }
        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let days_of_month = parse_field(fields[2], 1, 31, "day-of-month")?;
        let months = parse_field(fields[3], 1, 12, "month")?;
        // Weekday accepts 0-7 with 7 folding onto Sunday (Vixie convention).
        let mut days_of_week = parse_field(fields[4], 0, 7, "day-of-week")?;
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        Ok(CronExpr {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_any: fields[2].trim() == "*",
            dow_any: fields[4].trim() == "*",
        })
    }

    /// Next fire strictly after `from` (minute resolution, local time), or
    /// `None` when nothing fires within the cap.
    pub fn next_after(&self, from: DateTime<Local>) -> Option<DateTime<Local>> {
        self.next_after_in(&Local, from)
    }

    /// Timezone-parameterized core of [`CronExpr::next_after`]; exposed for
    /// tests, which need a synthetic zone with a DST gap (`Local` cannot be
    /// re-zoned in-process).
    pub(crate) fn next_after_in<T: TimeZone>(
        &self,
        tz: &T,
        from: DateTime<T>,
    ) -> Option<DateTime<T>> {
        let mut naive = truncate_to_minute(from.naive_local())? + Duration::minutes(1);
        for _ in 0..MAX_DAY_STEPS {
            let date = naive.date();
            if self.months & (1 << date.month()) == 0 {
                // Fast path: jump to the 1st of the next month, 00:00.
                naive = first_of_next_month(date)?;
                continue;
            }
            if !self.day_matches(date) {
                naive = date.succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if let Some(found) = self.scan_day(naive) {
                if let Some(zoned) = tz.from_local_datetime(&found).earliest() {
                    return Some(zoned);
                }
                // DST spring-forward gap: this wall-clock time does not
                // exist today. Skip it and keep scanning — a fire minute
                // inside the gap must silence that one fire, not the task.
                naive = found + Duration::minutes(1);
                continue;
            }
            naive = date.succ_opt()?.and_hms_opt(0, 0, 0)?;
        }
        None
    }

    /// Millis convenience wrapper over [`CronExpr::next_after`]; `now` is
    /// unix epoch millis and the answer is the next fire in the same unit.
    pub fn next_fire_millis(&self, now: u64) -> Option<u64> {
        let from = DateTime::from_timestamp_millis(i64::try_from(now).ok()?)?.with_timezone(&Local);
        self.next_after(from)
            .and_then(|next| u64::try_from(next.timestamp_millis()).ok())
    }

    /// Vixie day rule: both restricted → either matches; one `*` → only the
    /// restricted field decides; both `*` → every day.
    fn day_matches(&self, date: chrono::NaiveDate) -> bool {
        let dom = self.days_of_month & (1 << date.day()) != 0;
        let dow = self.days_of_week & (1 << date.weekday().num_days_from_sunday()) != 0;
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// Minute-scan one matching day from `start` (which sits inside it) to
    /// midnight; returns the first matching naive local timestamp. Conversion
    /// to a zoned instant (and DST-gap skipping) happens in
    /// [`CronExpr::next_after_in`].
    fn scan_day(&self, start: NaiveDateTime) -> Option<NaiveDateTime> {
        let date = start.date();
        let mut cursor = start;
        let day_end = date.and_hms_opt(23, 59, 59)?;
        while cursor <= day_end {
            if self.hours & (1 << cursor.hour()) != 0 && self.minutes & (1 << cursor.minute()) != 0
            {
                return Some(cursor);
            }
            cursor += Duration::minutes(1);
        }
        None
    }
}

/// Truncates to the whole minute; `None` on the (unrepresentable) far edge.
fn truncate_to_minute(naive: NaiveDateTime) -> Option<NaiveDateTime> {
    naive
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
}

/// `date`'s month has no fire day left: jump to the 1st of the next month.
fn first_of_next_month(date: chrono::NaiveDate) -> Option<NaiveDateTime> {
    let (year, month) = if date.month() == 12 {
        (date.year() + 1, 1)
    } else {
        (date.year(), date.month() + 1)
    };
    chrono::NaiveDate::from_ymd_opt(year, month, 1)?.and_hms_opt(0, 0, 0)
}

/// One comma-separated field → bitmask over `min..=max`. `*` spans the full
/// range; `a/n` means `a..=max` step n (cronie behavior); a malformed or
/// empty field is a hard error naming the field.
fn parse_field(field: &str, min: u32, max: u32, what: &str) -> Result<u64, String> {
    let mut mask = 0u64;
    if field.trim().is_empty() {
        return Err(format!("cron {what} field is empty"));
    }
    for part in field.split(',') {
        let part = part.trim();
        let (range_text, step) = match part.split_once('/') {
            Some((range, step)) => (range, parse_step(step, what)?),
            None => (part, 1),
        };
        let (lo, hi) = if range_text == "*" {
            (min, max)
        } else if let Some((a, b)) = range_text.split_once('-') {
            (parse_value(a, min, max, what)?, parse_value(b, min, max, what)?)
        } else {
            let value = parse_value(range_text, min, max, what)?;
            (value, if step == 1 { value } else { max })
        };
        if lo > hi {
            return Err(format!("cron {what} range {lo}-{hi} is inverted"));
        }
        let mut value = lo;
        while value <= hi {
            mask |= 1 << value;
            value += step;
        }
    }
    if mask == 0 {
        return Err(format!("cron {what} field matches nothing: '{field}'"));
    }
    Ok(mask)
}

fn parse_value(text: &str, min: u32, max: u32, what: &str) -> Result<u32, String> {
    let value: u32 = text
        .trim()
        .parse()
        .map_err(|_| format!("cron {what} value '{text}' is not a number"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("cron {what} value {value} is out of range {min}-{max}"));
    }
    Ok(value)
}

fn parse_step(text: &str, what: &str) -> Result<u32, String> {
    let step: u32 = text
        .trim()
        .parse()
        .map_err(|_| format!("cron {what} step '{text}' is not a number"))?;
    if step == 0 {
        return Err(format!("cron {what} step must be at least 1"));
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("test timestamp exists in every timezone (mid-day times only)")
    }

    fn next(expr: &str, from: DateTime<Local>) -> Option<DateTime<Local>> {
        CronExpr::parse(expr).expect("valid test expression").next_after(from)
    }

    // -- parse errors -------------------------------------------------------

    #[test]
    fn rejects_wrong_field_count_and_garbage() {
        for bad in [
            "* * * *",
            "* * * * * *",
            "",
            "60 * * * *",
            "* 24 * * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "1-0 * * * *",
            "a * * * *",
            "1,,2 * * * *",
            "  ",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn weekday_seven_folds_onto_sunday() {
        let sunday = CronExpr::parse("0 12 * * 0").unwrap();
        let seven = CronExpr::parse("0 12 * * 7").unwrap();
        assert_eq!(sunday, seven);
    }

    // -- next_after ---------------------------------------------------------

    #[test]
    fn star_everything_fires_next_minute() {
        assert_eq!(
            next("* * * * *", local(2026, 10, 4, 10, 7)),
            Some(local(2026, 10, 4, 10, 8))
        );
        // Minute boundary: strictly after, never "now".
        assert_eq!(
            next("* * * * *", local(2026, 10, 4, 10, 8)),
            Some(local(2026, 10, 4, 10, 9))
        );
    }

    #[test]
    fn every_fifteen_minutes_snaps_to_grid() {
        let expr = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 7)), Some(local(2026, 10, 4, 10, 15)));
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 15)), Some(local(2026, 10, 4, 10, 30)));
        // Hour rollover picks 00 of the next hour.
        assert_eq!(expr.next_after(local(2026, 10, 4, 23, 50)), Some(local(2026, 10, 5, 0, 0)));
    }

    #[test]
    fn daily_time_rolls_to_next_day() {
        let expr = CronExpr::parse("30 3 * * *").unwrap();
        // 03:30 exactly → next day (strictly after).
        assert_eq!(expr.next_after(local(2026, 10, 4, 3, 30)), Some(local(2026, 10, 5, 3, 30)));
        assert_eq!(expr.next_after(local(2026, 10, 4, 3, 31)), Some(local(2026, 10, 5, 3, 30)));
        assert_eq!(expr.next_after(local(2026, 10, 4, 23, 0)), Some(local(2026, 10, 5, 3, 30)));
    }

    #[test]
    fn month_jump_lands_on_leap_day() {
        assert_eq!(
            next("0 0 29 2 *", local(2023, 6, 1, 12, 0)),
            Some(local(2024, 2, 29, 0, 0)),
            "2024 is a leap year"
        );
    }

    #[test]
    fn impossible_day_in_month_returns_none() {
        // Feb 31st never exists: the walk exhausts its month-jump cap.
        assert_eq!(next("0 0 31 2 *", local(2026, 10, 4, 10, 0)), None);
    }

    #[test]
    fn weekly_dow_finds_next_sunday() {
        // 2026-10-04 is a Sunday.
        assert_eq!(
            next("0 12 * * 0", local(2026, 10, 4, 13, 0)),
            Some(local(2026, 10, 11, 12, 0))
        );
        assert_eq!(
            next("0 12 * * 0", local(2026, 10, 4, 8, 0)),
            Some(local(2026, 10, 4, 12, 0))
        );
    }

    #[test]
    fn weekday_range_skips_the_weekend() {
        // 2026-10-04 is a Sunday; Fri 17:00 → Monday 04:30.
        assert_eq!(
            next("30 4 * * 1-5", local(2026, 10, 9, 17, 0)),
            Some(local(2026, 10, 12, 4, 30))
        );
    }

    #[test]
    fn dom_and_dow_both_restricted_match_either() {
        // Vixie rule: `0 0 13 * 5` = the 13th OR any Friday.
        // From Sun 2026-10-04: Friday the 9th comes before Tuesday the 13th.
        assert_eq!(
            next("0 0 13 * 5", local(2026, 10, 4, 1, 0)),
            Some(local(2026, 10, 9, 0, 0))
        );
        // From Fri 2026-10-09 (after the 00:00 fire): the 13th (Tuesday)
        // precedes the next Friday the 16th.
        assert_eq!(
            next("0 0 13 * 5", local(2026, 10, 9, 1, 0)),
            Some(local(2026, 10, 13, 0, 0))
        );
    }

    #[test]
    fn dom_restricted_with_dow_star_ignores_weekday() {
        // `0 0 1 * *` = the 1st of every month regardless of weekday.
        assert_eq!(
            next("0 0 1 * *", local(2026, 10, 4, 1, 0)),
            Some(local(2026, 11, 1, 0, 0))
        );
    }

    #[test]
    fn list_and_step_mix_in_one_field() {
        // 10:05, 10:35, 10:50, 10:55 within an hour.
        let expr = CronExpr::parse("5,35,50-59/5 * * * *").unwrap();
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 6)), Some(local(2026, 10, 4, 10, 35)));
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 36)), Some(local(2026, 10, 4, 10, 50)));
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 51)), Some(local(2026, 10, 4, 10, 55)));
        // Minute 5 also exists in the NEXT hour — the day must not be skipped.
        assert_eq!(expr.next_after(local(2026, 10, 4, 10, 56)), Some(local(2026, 10, 4, 11, 5)));
        // Past the last fire of the day (23:55), the next match is tomorrow 00:05.
        assert_eq!(expr.next_after(local(2026, 10, 4, 23, 56)), Some(local(2026, 10, 5, 0, 5)));
    }

    #[test]
    fn next_fire_millis_roundtrips_epoch_units() {
        let expr = CronExpr::parse("*/30 * * * *").unwrap();
        let now = local(2026, 10, 4, 10, 7).timestamp_millis();
        let got = expr.next_fire_millis(u64::try_from(now).unwrap()).unwrap();
        assert_eq!(got, u64::try_from(local(2026, 10, 4, 10, 30).timestamp_millis()).unwrap());
    }

    #[test]
    fn year_boundary_crosses_cleanly() {
        assert_eq!(
            next("0 0 1 1 *", local(2026, 12, 31, 12, 0)),
            Some(local(2027, 1, 1, 0, 0))
        );
    }

    // -- DST spring-forward gap ---------------------------------------------
    //
    // `Local` cannot be re-zoned in-process, so the gap semantics are pinned
    // against a synthetic zone whose 02:00–02:59 on 2026-03-08 does not exist.

    #[derive(Debug, Clone, Copy)]
    struct GapTz;

    impl TimeZone for GapTz {
        type Offset = chrono::FixedOffset;
        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> chrono::LocalResult<chrono::FixedOffset> {
            let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("fixed offset");
            let gap_day = chrono::NaiveDate::from_ymd_opt(2026, 3, 8).expect("date");
            if local.date() == gap_day && local.hour() == 2 {
                chrono::LocalResult::None
            } else {
                chrono::LocalResult::Single(offset)
            }
        }
        fn offset_from_utc_datetime(&self, _utc: &NaiveDateTime) -> chrono::FixedOffset {
            chrono::FixedOffset::east_opt(8 * 3600).expect("fixed offset")
        }
        fn from_offset(offset: &chrono::FixedOffset) -> Self {
            let _ = offset;
            GapTz
        }
        fn offset_from_local_date(
            &self,
            date: &chrono::NaiveDate,
        ) -> chrono::LocalResult<chrono::FixedOffset> {
            let naive = date.and_hms_opt(12, 0, 0).expect("noon");
            self.offset_from_local_datetime(&naive)
        }
        fn offset_from_utc_date(&self, _date: &chrono::NaiveDate) -> chrono::FixedOffset {
            chrono::FixedOffset::east_opt(8 * 3600).expect("fixed offset")
        }
    }

    fn gap_tz_time(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<GapTz> {
        GapTz
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("test time sits outside the synthetic gap")
    }

    fn next_in(expr: &str, from: DateTime<GapTz>) -> Option<DateTime<GapTz>> {
        CronExpr::parse(expr)
            .expect("valid test expression")
            .next_after_in(&GapTz, from)
    }

    #[test]
    fn dst_gap_candidate_skips_to_the_next_day_instead_of_dying() {
        // 02:30 does not exist on 2026-03-08; the schedule must move on to
        // the next day's fire rather than return None (which used to silence
        // the task permanently — hydrate/claim persist the None).
        assert_eq!(
            next_in("30 2 * * *", gap_tz_time(2026, 3, 8, 0, 0)),
            Some(gap_tz_time(2026, 3, 9, 2, 30))
        );
    }

    #[test]
    fn dst_gap_scan_finds_a_later_candidate_on_the_same_day() {
        // `0 2-3 * * *`: the 02:00 candidate is in the gap, the 03:00 one is
        // real — the day must not be abandoned after the skipped candidate.
        assert_eq!(
            next_in("0 2-3 * * *", gap_tz_time(2026, 3, 8, 0, 0)),
            Some(gap_tz_time(2026, 3, 8, 3, 0))
        );
    }

    #[test]
    fn gap_minute_range_is_skipped_minute_by_minute() {
        // The whole 02:xx hour is missing; `30 2-3 * * *` still lands on the
        // 03:30 fire the same day.
        assert_eq!(
            next_in("30 2-3 * * *", gap_tz_time(2026, 3, 8, 0, 0)),
            Some(gap_tz_time(2026, 3, 8, 3, 30))
        );
    }

    #[test]
    fn leap_day_crosses_the_non_leap_century_gap() {
        // 2096-02-29 is the last leap day before 2104 (2100 is not a leap
        // year) — an 8-year wall, the widest gap a legal expression can
        // legally produce; the walk must survive it.
        assert_eq!(
            next("0 0 29 2 *", local(2096, 3, 1, 12, 0)),
            Some(local(2104, 2, 29, 0, 0))
        );
    }

    #[test]
    fn dom_dow_both_restricted_fires_every_matching_february_weekday() {
        // `0 0 29 2 1` is NOT a once-in-decades shape: both fields being
        // restricted ORs them, so every February Monday fires too. From
        // 2016-03-01 the next fire is simply the first Monday of Feb 2017.
        assert_eq!(
            next("0 0 29 2 1", local(2016, 3, 1, 12, 0)),
            Some(local(2017, 2, 6, 0, 0))
        );
    }
}
