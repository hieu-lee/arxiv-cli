//! Tiny UTC date helpers (no chrono): relative/partial date parsing for --since/--until.

use std::time::{SystemTime, UNIX_EPOCH};

/// (year, month 1..=12, day 1..=31)
pub type Ymd = (i64, u32, u32);

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> Ymd {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

pub fn today() -> Ymd {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    civil_from_days(secs.div_euclid(86400))
}

pub fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn shift_months(y: i64, m: u32, delta: i64) -> (i64, u32) {
    let idx = y * 12 + (m as i64 - 1) - delta;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

/// Parse a bound into a date. `end` = inclusive end of the period for partial dates.
/// Accepts: YYYY, YYYY-MM, YYYY-MM-DD, YYYYMMDD, today/now, Nd, Nw, Nm, Ny (relative to `now`).
pub fn parse_bound(s: &str, end: bool, now: Ymd) -> Result<Ymd, String> {
    let t = s.trim().to_ascii_lowercase();
    let bad = || format!("bad date {s:?} (use 2024, 2024-03, 2024-03-05, 30d, 6m, 2y)");
    if t == "today" || t == "now" {
        return Ok(now);
    }
    if t.len() >= 2 && t.chars().last().map_or(false, |c| matches!(c, 'd' | 'w' | 'm' | 'y')) {
        if let Ok(n) = t[..t.len() - 1].parse::<i64>() {
            if !(0..=100_000).contains(&n) {
                return Err(bad());
            }
            let unit = t.chars().last().unwrap();
            let r = match unit {
                'd' => civil_from_days(days_from_civil(now.0, now.1, now.2) - n),
                'w' => civil_from_days(days_from_civil(now.0, now.1, now.2) - 7 * n),
                'm' => {
                    let (y, m) = shift_months(now.0, now.1, n);
                    (y, m, now.2.min(days_in_month(y, m)))
                }
                _ => {
                    let y = now.0 - n;
                    (y, now.1, now.2.min(days_in_month(y, now.1)))
                }
            };
            // arXiv starts in 1991; clamp absurd lookbacks
            return Ok(if r.0 < 1991 { (1991, 1, 1) } else { r });
        }
    }
    let nums: Vec<&str> = t.split('-').collect();
    let all_digits = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    // each component is None when omitted; a supplied 0 is invalid, not "omitted"
    let (y, m, d): (i64, Option<u32>, Option<u32>) = if nums.len() == 1 && all_digits(&t) {
        match t.len() {
            4 => (t.parse().map_err(|_| bad())?, None, None),
            6 => (t[..4].parse().map_err(|_| bad())?, Some(t[4..].parse().map_err(|_| bad())?), None),
            8 => (t[..4].parse().map_err(|_| bad())?, Some(t[4..6].parse().map_err(|_| bad())?), Some(t[6..].parse().map_err(|_| bad())?)),
            _ => return Err(bad()),
        }
    } else if nums.len() <= 3 && nums.iter().all(|p| all_digits(p)) {
        (
            nums[0].parse::<i64>().map_err(|_| bad())?,
            nums.get(1).map(|p| p.parse::<u32>()).transpose().map_err(|_| bad())?,
            nums.get(2).map(|p| p.parse::<u32>()).transpose().map_err(|_| bad())?,
        )
    } else {
        return Err(bad());
    };
    if !(1990..=2100).contains(&y) || m.is_some_and(|m| !(1..=12).contains(&m)) || d == Some(0) {
        return Err(bad());
    }
    let (m, d) = match (m, d) {
        (None, _) => (if end { 12 } else { 1 }, if end { 31 } else { 1 }),
        (Some(m), None) => (m, if end { days_in_month(y, m) } else { 1 }),
        (Some(m), Some(d)) => (m, d),
    };
    if d > days_in_month(y, m) {
        return Err(bad());
    }
    Ok((y, m, d))
}

/// arXiv API timestamp: YYYYMMDDHHMM
pub fn api_stamp(d: Ymd, end: bool) -> String {
    format!("{:04}{:02}{:02}{}", d.0, d.1, d.2, if end { "2359" } else { "0000" })
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: Ymd = (2026, 10, 23);
    #[test]
    fn civil_roundtrip() {
        for z in [-1000, 0, 1, 11016, 20000, 20654] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
    #[test]
    fn partial_dates() {
        assert_eq!(parse_bound("2024", false, NOW), Ok((2024, 1, 1)));
        assert_eq!(parse_bound("2024", true, NOW), Ok((2024, 12, 31)));
        assert_eq!(parse_bound("2024-02", true, NOW), Ok((2024, 2, 29)));
        assert_eq!(parse_bound("2024-03-05", false, NOW), Ok((2024, 3, 5)));
        assert_eq!(parse_bound("20240305", false, NOW), Ok((2024, 3, 5)));
    }
    #[test]
    fn relative() {
        assert_eq!(parse_bound("30d", false, NOW), Ok((2026, 9, 23)));
        assert_eq!(parse_bound("2w", false, NOW), Ok((2026, 10, 9)));
        assert_eq!(parse_bound("8m", false, NOW), Ok((2026, 2, 23)));
        assert_eq!(parse_bound("2y", false, NOW), Ok((2024, 10, 23)));
        assert_eq!(parse_bound("12m", false, (2026, 3, 31)), Ok((2025, 3, 31)));
        assert_eq!(parse_bound("1m", false, (2026, 3, 31)), Ok((2026, 2, 28)));
    }
    #[test]
    fn rejects() {
        assert!(parse_bound("9223372036854775807w", false, NOW).is_err());
        assert_eq!(parse_bound("100000y", false, NOW), Ok((1991, 1, 1)));
        assert!(parse_bound("2024-13", false, NOW).is_err());
        assert!(parse_bound("2024-00-99", true, NOW).is_err());
        assert!(parse_bound("2024-02-00", true, NOW).is_err());
        assert!(parse_bound("2024-00", false, NOW).is_err());
        assert!(parse_bound("202400", false, NOW).is_err());
        assert!(parse_bound("abc", false, NOW).is_err());
        assert!(parse_bound("2024-02-30", false, NOW).is_err());
        assert!(parse_bound("", false, NOW).is_err());
    }
    #[test]
    fn stamp() {
        assert_eq!(api_stamp((2024, 3, 5), true), "202403052359");
    }
}
