use croner::parser::{CronParser, Seconds, Year};
use jiff::Zoned;

use super::Schedule;

impl Schedule {
    /// The next `count` fire times strictly after `after`, in `after`'s time zone.
    /// Empty when the schedule never fires.
    pub fn next_runs(&self, after: &Zoned, count: usize) -> Vec<Zoned> {
        if self.never_fires().is_some() {
            return Vec::new();
        }
        let [_, _, dom, _, dow] = &self.fields;
        // Cron ANDs the day fields when either starts with `*`; croner only
        // does so when one is exactly `*`.
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .dom_and_dow(dom.is_star() || dow.is_star())
            .build()
            .parse(&self.to_string())
            .unwrap_or_else(|e| panic!("croner rejected validated schedule '{self}': {e}"));
        cron.iter_after(after.clone()).take(count).collect()
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::{Date, DateTime};

    use super::*;

    /// `2026-03-08T01:15[America/New_York]`, with the zone taken from jiff's
    /// bundled database so the tests don't depend on the system's tzdata.
    fn zoned(s: &str) -> Zoned {
        let (datetime, zone) = s.trim_end_matches(']').split_once('[').unwrap();
        let tz = jiff::tz::TimeZoneDatabase::bundled().get(zone).unwrap();
        datetime.parse::<DateTime>().unwrap().to_zoned(tz).unwrap()
    }

    fn runs(schedule: &str, after: &str, count: usize) -> Vec<String> {
        let schedule: Schedule = schedule.parse().unwrap();
        schedule
            .next_runs(&zoned(after), count)
            .iter()
            .map(|z| z.strftime("%a %Y-%m-%d %H:%M %:z").to_string())
            .collect()
    }

    const SUNDAY: &str = "2026-10-25T10:07[UTC]";

    #[test]
    fn next_times_in_utc() {
        for (schedule, expected) in [
            (
                "*/15 * * * *",
                &[
                    "Sun 2026-10-25 10:15 +00:00",
                    "Sun 2026-10-25 10:30 +00:00",
                    "Sun 2026-10-25 10:45 +00:00",
                ][..],
            ),
            (
                "30 2 * * 1-5",
                &[
                    "Mon 2026-10-26 02:30 +00:00",
                    "Tue 2026-10-27 02:30 +00:00",
                    "Wed 2026-10-28 02:30 +00:00",
                ],
            ),
            (
                "@monthly",
                &[
                    "Sun 2026-11-01 00:00 +00:00",
                    "Tue 2026-12-01 00:00 +00:00",
                    "Fri 2027-01-01 00:00 +00:00",
                ],
            ),
            (
                "0 0 29 2 *",
                &[
                    "Tue 2028-02-29 00:00 +00:00",
                    "Sun 2032-02-29 00:00 +00:00",
                    "Fri 2036-02-29 00:00 +00:00",
                ],
            ),
            (
                "every 2h",
                &[
                    "Sun 2026-10-25 12:00 +00:00",
                    "Sun 2026-10-25 14:00 +00:00",
                    "Sun 2026-10-25 16:00 +00:00",
                ],
            ),
            (
                "mon,thu at 18:15",
                &[
                    "Mon 2026-10-26 18:15 +00:00",
                    "Thu 2026-10-29 18:15 +00:00",
                    "Mon 2026-11-02 18:15 +00:00",
                ],
            ),
        ] {
            assert_eq!(runs(schedule, SUNDAY, 3), expected, "{schedule:?}");
        }
    }

    #[test]
    fn day_of_month_and_day_of_week() {
        for (schedule, expected) in [
            (
                "0 0 1 * 1",
                &[
                    "Mon 2026-10-26 00:00 +00:00",
                    "Sun 2026-11-01 00:00 +00:00",
                    "Mon 2026-11-02 00:00 +00:00",
                ],
            ),
            (
                "0 0 */2 * 1",
                &[
                    "Mon 2026-11-09 00:00 +00:00",
                    "Mon 2026-11-23 00:00 +00:00",
                    "Mon 2026-12-07 00:00 +00:00",
                ],
            ),
            (
                "0 0 1 * */2",
                &[
                    "Sun 2026-11-01 00:00 +00:00",
                    "Tue 2026-12-01 00:00 +00:00",
                    "Thu 2027-04-01 00:00 +00:00",
                ],
            ),
            (
                "0 0 * * 1",
                &[
                    "Mon 2026-10-26 00:00 +00:00",
                    "Mon 2026-11-02 00:00 +00:00",
                    "Mon 2026-11-09 00:00 +00:00",
                ],
            ),
        ] {
            assert_eq!(runs(schedule, SUNDAY, 3), expected, "{schedule:?}");
        }
    }

    #[test]
    fn never_fires_is_empty() {
        assert!(runs("0 0 30 2 *", SUNDAY, 5).is_empty());
    }

    #[test]
    fn dst_gap() {
        assert_eq!(
            runs("30 2 * * *", "2026-03-07T12:00[America/New_York]", 2),
            ["Sun 2026-03-08 03:00 -04:00", "Mon 2026-03-09 02:30 -04:00"]
        );
        assert_eq!(
            runs("*/30 * * * *", "2026-03-08T01:15[America/New_York]", 3),
            [
                "Sun 2026-03-08 01:30 -05:00",
                "Sun 2026-03-08 03:00 -04:00",
                "Sun 2026-03-08 03:30 -04:00"
            ]
        );
    }

    #[test]
    fn dst_overlap() {
        assert_eq!(
            runs("30 1 * * *", "2026-10-31T12:00[America/New_York]", 2),
            ["Sun 2026-11-01 01:30 -04:00", "Mon 2026-11-02 01:30 -05:00"]
        );
        assert_eq!(
            runs("*/30 * * * *", "2026-11-01T00:45[America/New_York]", 5),
            [
                "Sun 2026-11-01 01:00 -04:00",
                "Sun 2026-11-01 01:30 -04:00",
                "Sun 2026-11-01 01:00 -05:00",
                "Sun 2026-11-01 01:30 -05:00",
                "Sun 2026-11-01 02:00 -05:00"
            ]
        );
    }

    /// Scans every minute with our own fields and Vixie cron's day rule.
    fn brute_force(schedule: &Schedule, start: Date, end: Date, limit: usize) -> Vec<DateTime> {
        let [minute, hour, dom, month, dow] = &schedule.fields;
        let mut found = Vec::new();
        let mut date = start;
        while date < end && found.len() < limit {
            let in_dom = dom.contains(date.day() as u8);
            let in_dow = dow.contains(date.weekday().to_sunday_zero_offset() as u8);
            let day_matches = if dom.is_star() || dow.is_star() {
                in_dom && in_dow
            } else {
                in_dom || in_dow
            };
            if day_matches && month.contains(date.month() as u8) {
                for h in hour.values() {
                    for m in minute.values() {
                        found.push(date.at(h as i8, m as i8, 0, 0));
                    }
                }
            }
            date = date.tomorrow().unwrap();
        }
        found.truncate(limit);
        found
    }

    #[test]
    fn croner_agrees_with_cron_rules() {
        let (start, end) = (Date::constant(2026, 1, 1), Date::constant(2030, 1, 1));
        let after = zoned("2025-12-31T23:59[UTC]");
        for input in [
            "* * * * *",
            "*/7 * * * *",
            "0-50/7 9-17 * * *",
            "5,10 1-3 * * *",
            "30 2 * * MON-FRI",
            "0 0 * * 0,7",
            "0 0 * * sun",
            "0 12 1 jan,jul *",
            "0 0 1 * 1",
            "0 0 13 * 5",
            "0 0 */2 * 1",
            "0 0 1 * */2",
            "0 0 */10 * 1-5",
            "0 0 29 2 *",
            "0 0 31 * *",
            "0 0 1 */5 *",
            "0 */5 * * *",
            "@hourly",
            "@weekly",
            "@yearly",
            "every 15m",
            "every 3h",
            "weekdays at 9:00",
            "mon,thu at 18:15",
        ] {
            let schedule: Schedule = input.parse().unwrap();
            let expected = brute_force(&schedule, start, end, 500);
            let actual: Vec<DateTime> = schedule
                .next_runs(&after, 500)
                .iter()
                .map(Zoned::datetime)
                .filter(|dt| dt.date() < end)
                .collect();
            assert_eq!(actual, expected, "{input:?}");
        }
    }
}
