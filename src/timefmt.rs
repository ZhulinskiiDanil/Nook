//! Time conversions without extra dependencies: FILETIME ↔ SystemTime, RFC 3339 (UTC).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 100-ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

pub fn filetime_to_system(ft: u64) -> Option<SystemTime> {
    let since_unix = ft.checked_sub(EPOCH_DIFF_100NS)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix.checked_mul(100)?))
}

/// `YYYY-MM-DDTHH:MM:SSZ`; times before 1970 clamp to the epoch.
pub fn rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's days → civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Whole days between `t` and `now` (0 if in the future).
pub fn age_days(t: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(t)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_790_380_860)),
            "2026-09-26T00:01:00Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_system(EPOCH_DIFF_100NS), Some(UNIX_EPOCH));
        assert_eq!(filetime_to_system(0), None);
        assert_eq!(filetime_to_system(u64::MAX), None);
    }

    #[test]
    fn ages() {
        let now = UNIX_EPOCH + Duration::from_secs(20 * 86_400 + 5);
        assert_eq!(
            age_days(UNIX_EPOCH + Duration::from_secs(5 * 86_400), now),
            15
        );
        assert_eq!(age_days(now + Duration::from_secs(10), now), 0);
    }
}
