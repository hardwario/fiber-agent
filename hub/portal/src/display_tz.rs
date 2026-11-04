//! The installation's display timezone (plan/decisions.md #25).
//!
//! Every stored instant is UTC; this is the zone one is shown in when it
//! becomes text for a person — pages, e-mails, chart axes. One per
//! installation, so it is process state rather than something threaded
//! through every render path: `main` loads it from `portal_settings` at
//! startup and the settings page replaces it when an admin saves. Until then
//! it is UTC, which is also what a test process sees.
//!
//! The zone database is bundled into the binary (`jiff`'s
//! `tzdb-bundle-always`), so a Hub image without `tzdata` still knows
//! `Europe/Prague`.

use std::sync::RwLock;

use jiff::tz::TimeZone;

static ZONE: RwLock<Option<(String, TimeZone)>> = RwLock::new(None);

/// Resolves an IANA name against the bundled database.
pub fn parse(name: &str) -> Option<TimeZone> {
    TimeZone::get(name).ok()
}

/// Makes `name` the display zone. `false` (and no change) when the name is
/// not a zone.
pub fn set(name: &str) -> bool {
    let Some(tz) = parse(name) else {
        return false;
    };
    *ZONE.write().unwrap_or_else(|p| p.into_inner()) = Some((name.to_string(), tz));
    true
}

/// The current display zone.
pub fn current() -> TimeZone {
    ZONE.read()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|(_, tz)| tz.clone())
        .unwrap_or(TimeZone::UTC)
}

/// The current zone's IANA name, for the settings form and the chart axes.
pub fn name() -> String {
    ZONE.read()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "UTC".to_string())
}

/// Every zone the settings form offers, sorted.
pub fn available() -> Vec<String> {
    let mut names: Vec<String> = jiff::tz::db().available().map(|n| n.to_string()).collect();
    names.sort();
    names
}

/// A `datetime-local` form value (`2026-09-26T07:00`, seconds optional)
/// read as wall-clock time in the display zone, as the person typed it
/// against a page that shows that zone (UX-042). A value that carries an
/// offset (`…Z`, `…+02:00`) is taken as it says. A time skipped by a
/// daylight-saving jump resolves forward, jiff's "compatible" rule.
pub fn parse_local(s: &str) -> Option<i64> {
    parse_local_in(s, &current())
}

/// [`parse_local`] against an explicit zone, for a caller that tests it.
pub(crate) fn parse_local_in(s: &str, tz: &TimeZone) -> Option<i64> {
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts.as_second());
    }
    let dt: jiff::civil::DateTime = s.parse().ok()?;
    dt.to_zoned(tz.clone())
        .ok()
        .map(|z| z.timestamp().as_second())
}

/// The `datetime-local` value for an instant in the display zone — what a
/// prefilled input shows, and what [`parse_local`] reads back.
pub fn datetime_local(unix_secs: i64) -> String {
    datetime_local_in(unix_secs, &current())
}

fn datetime_local_in(unix_secs: i64, tz: &TimeZone) -> String {
    match jiff::Timestamp::from_second(unix_secs) {
        Ok(ts) => ts
            .to_zoned(tz.clone())
            .strftime("%Y-%m-%dT%H:%M")
            .to_string(),
        Err(_) => String::new(),
    }
}

/// The UTC instant (SQLite `YYYY-MM-DD HH:MM:SS`) at which a calendar day
/// of the display zone starts, or — `end` — its last second. For filters
/// that take a day and compare against stored UTC stamps: a Prague day
/// starts at 22:00 or 23:00 UTC of the day before. `None` if `day` is not
/// a `YYYY-MM-DD` date.
pub fn day_bound_utc(day: &str, end: bool) -> Option<String> {
    day_bound_utc_in(day, end, &current())
}

fn day_bound_utc_in(day: &str, end: bool, tz: &TimeZone) -> Option<String> {
    // jiff's date parser also takes a datetime and drops the time; only a
    // bare date names a day.
    if day.len() != 10 {
        return None;
    }
    let date: jiff::civil::Date = day.parse().ok()?;
    let date = if end { date.tomorrow().ok()? } else { date };
    let start = date.to_zoned(tz.clone()).ok()?.timestamp();
    let ts = if end {
        start.checked_sub(jiff::SignedDuration::from_secs(1)).ok()?
    } else {
        start
    };
    Some(ts.strftime("%Y-%m-%d %H:%M:%S").to_string())
}

/// `2026-09-25 18:02 CEST` in the display zone — for a time inside a
/// sentence, where the language-specific [`crate::render::display_dt`] shape
/// is not known yet.
pub fn format_minutes(unix_secs: i64) -> String {
    match jiff::Timestamp::from_second(unix_secs) {
        Ok(ts) => ts
            .to_zoned(current())
            .strftime("%Y-%m-%d %H:%M %Z")
            .to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UX-042: 07:00 typed on a Prague page in summer is 05:00 UTC, and the
    /// prefill shows the instant the way the page does.
    #[test]
    fn a_form_time_is_read_and_written_in_the_display_zone() {
        let prague = parse("Europe/Prague").expect("zone");
        let at_0500_utc = 1_790_398_800; // 2026-09-26T05:00:00Z
        assert_eq!(
            parse_local_in("2026-09-26T07:00", &prague),
            Some(at_0500_utc)
        );
        assert_eq!(
            parse_local_in("2026-09-26T07:00:00", &prague),
            Some(at_0500_utc)
        );
        assert_eq!(datetime_local_in(at_0500_utc, &prague), "2026-09-26T07:00");
        // Winter: CET is +01:00.
        assert_eq!(
            parse_local_in("2026-01-13T15:30", &prague),
            Some(1_768_314_600)
        );
        // An explicit offset wins over the zone.
        assert_eq!(
            parse_local_in("2026-09-26T05:00:00Z", &prague),
            Some(at_0500_utc)
        );
        assert_eq!(parse_local_in("not a time", &prague), None);
        // A Prague day, as a UTC range for the audit filter.
        assert_eq!(
            day_bound_utc_in("2026-09-26", false, &prague).as_deref(),
            Some("2026-09-25 22:00:00")
        );
        assert_eq!(
            day_bound_utc_in("2026-09-26", true, &prague).as_deref(),
            Some("2026-09-26 21:59:59")
        );
        assert_eq!(day_bound_utc_in("yesterday", false, &prague), None);
        // UTC (a test process's zone) keeps the old reading.
        assert_eq!(
            parse_local_in("2026-01-13T14:30", &TimeZone::UTC),
            Some(1_768_314_600)
        );
    }
}
