//! Turns a time period the model picked into date bounds. The model reads
//! dates as text and cannot do arithmetic, so the calendar maths lives here.

use time::{Date, Duration, Month};

use crate::query::Op;

pub(super) const WITHIN_LAST_N: &str = "within_last_n";
pub(super) const MORE_THAN_N_AGO: &str = "more_than_n_ago";
pub(super) const ON_DATE: &str = "on_date";
pub(super) const BEFORE_DATE: &str = "before_date";
pub(super) const AFTER_DATE: &str = "after_date";
pub(super) const SINCE_DATE: &str = "since_date";

/// Every period a request can name, with its meaning in words.
pub(super) const PERIODS: [(&str, &str); 14] = [
    ("today", "during today"),
    ("yesterday", "during yesterday"),
    ("this_week", "during the current week"),
    ("last_week", "during the previous week"),
    ("this_month", "during the current month"),
    ("last_month", "during the previous month"),
    ("this_year", "during the current year"),
    ("last_year", "during the previous year"),
    (
        WITHIN_LAST_N,
        "within the last or past N days, weeks, months or years, counted back from now",
    ),
    (
        MORE_THAN_N_AGO,
        "longer ago than N days, weeks, months or years: more than N ago, over N ago",
    ),
    (
        ON_DATE,
        "on one specific calendar date that the request writes out",
    ),
    (
        BEFORE_DATE,
        "before a specific calendar date that the request writes out",
    ),
    (
        AFTER_DATE,
        "after a specific calendar date that the request writes out",
    ),
    (
        SINCE_DATE,
        "on or after a specific calendar date that the request writes out: since, from",
    ),
];
pub(super) const UNITS: [&str; 4] = ["days", "weeks", "months", "years"];

/// The comparisons that keep a date inside a period.
pub(super) type Bounds = Vec<(Op, Date)>;

/// Bounds for a period that needs only today's date, such as `last_month`.
pub(super) fn named(period: &str, today: Date) -> Option<Bounds> {
    let monday = plus(today, -i64::from(today.weekday().number_days_from_monday()))?;
    let (year, month) = (today.year(), today.month());
    match period {
        "today" => Some(between(today, plus(today, 1)?)),
        "yesterday" => Some(between(plus(today, -1)?, today)),
        "this_week" => Some(between(monday, plus(monday, 7)?)),
        "last_week" => Some(between(plus(monday, -7)?, monday)),
        "this_month" => month_bounds(year, month),
        "last_month" if month == Month::January => {
            month_bounds(year.checked_sub(1)?, Month::December)
        }
        "last_month" => month_bounds(year, month.previous()),
        "this_year" => year_bounds(year),
        "last_year" => year_bounds(year.checked_sub(1)?),
        _ => None,
    }
}

/// Bounds for a period counted back from today, such as the last 7 days.
pub(super) fn counted(period: &str, today: Date, count: u32, unit: &str) -> Option<Bounds> {
    let edge = back(today, i64::from(count), unit)?;
    match period {
        WITHIN_LAST_N => Some(vec![(Op::Gte, edge)]),
        MORE_THAN_N_AGO => Some(vec![(Op::Lt, edge)]),
        _ => None,
    }
}

/// Bounds for a period set by a date the request writes out.
pub(super) fn dated(period: &str, date: Date) -> Option<Bounds> {
    match period {
        ON_DATE => Some(between(date, plus(date, 1)?)),
        BEFORE_DATE => Some(vec![(Op::Lt, date)]),
        AFTER_DATE => Some(vec![(Op::Gte, plus(date, 1)?)]),
        SINCE_DATE => Some(vec![(Op::Gte, date)]),
        _ => None,
    }
}

fn between(start: Date, end: Date) -> Bounds {
    vec![(Op::Gte, start), (Op::Lt, end)]
}

fn plus(date: Date, days: i64) -> Option<Date> {
    date.checked_add(Duration::days(days))
}

/// The date `count` units before `date`. A day that does not exist in the
/// target month, such as 31 February, becomes that month's last day.
fn back(date: Date, count: i64, unit: &str) -> Option<Date> {
    let months = match unit {
        "days" => return plus(date, count.checked_neg()?),
        "weeks" => return plus(date, count.checked_mul(-7)?),
        "months" => count,
        "years" => count.checked_mul(12)?,
        _ => return None,
    };
    let index = i64::from(date.year())
        .checked_mul(12)?
        .checked_add(i64::from(u8::from(date.month())))?
        .checked_sub(1)?
        .checked_sub(months)?;
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = Month::try_from(u8::try_from(index.rem_euclid(12)).ok()?.checked_add(1)?).ok()?;
    (1..=date.day())
        .rev()
        .find_map(|day| Date::from_calendar_date(year, month, day).ok())
}

fn first_of(year: i32, month: Month) -> Option<Date> {
    Date::from_calendar_date(year, month, 1).ok()
}

fn month_bounds(year: i32, month: Month) -> Option<Bounds> {
    let next_year = if month == Month::December {
        year.checked_add(1)?
    } else {
        year
    };
    Some(between(
        first_of(year, month)?,
        first_of(next_year, month.next())?,
    ))
}

fn year_bounds(year: i32) -> Option<Bounds> {
    Some(between(
        first_of(year, Month::January)?,
        first_of(year.checked_add(1)?, Month::January)?,
    ))
}

#[cfg(test)]
mod tests {
    use time::{Date, Month};

    use super::{MORE_THAN_N_AGO, WITHIN_LAST_N, counted, named};
    use crate::query::Op;

    fn day(year: i32, month: u8, day: u8) -> Date {
        Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap()
    }

    #[test]
    fn a_week_runs_from_monday_even_when_today_is_sunday() {
        let sunday = day(2026, 10, 4);
        assert_eq!(
            named("this_week", sunday).unwrap(),
            [(Op::Gte, day(2026, 9, 28)), (Op::Lt, day(2026, 10, 5))]
        );
        assert_eq!(
            named("last_week", sunday).unwrap(),
            [(Op::Gte, day(2026, 9, 21)), (Op::Lt, day(2026, 9, 28))]
        );
    }

    #[test]
    fn months_and_years_roll_over_at_the_turn_of_the_year() {
        assert_eq!(
            named("last_month", day(2026, 1, 15)).unwrap(),
            [(Op::Gte, day(2025, 12, 1)), (Op::Lt, day(2026, 1, 1))]
        );
        assert_eq!(
            named("this_month", day(2026, 12, 31)).unwrap(),
            [(Op::Gte, day(2026, 12, 1)), (Op::Lt, day(2027, 1, 1))]
        );
    }

    #[test]
    fn counting_back_months_lands_on_the_last_day_of_a_shorter_month() {
        assert_eq!(
            counted(WITHIN_LAST_N, day(2024, 3, 31), 1, "months").unwrap(),
            [(Op::Gte, day(2024, 2, 29))]
        );
        assert_eq!(
            counted(MORE_THAN_N_AGO, day(2024, 2, 29), 1, "years").unwrap(),
            [(Op::Lt, day(2023, 2, 28))]
        );
        assert_eq!(
            counted(WITHIN_LAST_N, day(2026, 1, 10), 2, "weeks").unwrap(),
            [(Op::Gte, day(2025, 12, 27))]
        );
    }

    #[test]
    fn an_unknown_period_or_unit_has_no_bounds() {
        assert_eq!(named("next_week", day(2026, 10, 3)), None);
        assert_eq!(counted(WITHIN_LAST_N, day(2026, 10, 3), 2, "decades"), None);
    }
}
