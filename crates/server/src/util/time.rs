//! Date formats of JavaScript's `Date`: `toUTCString()` (e-mail templates, RFC 7231 HTTP dates)
//! next to `toISOString()` ([`crate::log::iso_time`]).

use crate::log::civil_from_days;

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `new Date(ms).toUTCString()`: `Thu, 01 Jan 1970 00:00:00 GMT`.
pub fn utc_string(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let weekday = DAYS[days.rem_euclid(7) as usize];
    let year = if y < 0 { format!("-{:04}", -y) } else { format!("{y:04}") };
    format!(
        "{weekday}, {d:02} {} {year} {:02}:{:02}:{:02} GMT",
        MONTHS[(m - 1) as usize],
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_string_matches_javascript() {
        assert_eq!(utc_string(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(utc_string(1_790_604_185_000), "Mon, 28 Sep 2026 14:03:05 GMT");
        assert_eq!(utc_string(-62_198_755_200_000), "Fri, 01 Jan -0001 00:00:00 GMT");
        assert_eq!(utc_string(-1), "Wed, 31 Dec 1969 23:59:59 GMT");
    }
}
