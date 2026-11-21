//! UTC timestamps as `2026-11-17T09:30:00Z`, for frontmatter a person reads.

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn format(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (y, mo, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// Microseconds since the epoch as RFC 3339 with six fraction digits
/// (`2026-11-17T09:30:00.000123Z`), the form Kosmonaut's `timestamp()`
/// reads.
#[cfg(feature = "kosmonaut")]
pub fn format_us(unix_us: i64) -> String {
    let secs = unix_us.div_euclid(1_000_000);
    let (y, mo, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60,
        unix_us.rem_euclid(1_000_000)
    )
}

/// `YYYY-MM-DD`, or that with `THH:MM:SS`, optional fraction, and `Z`.
pub fn parse(text: &str) -> Option<u64> {
    let t = text.trim();
    let num = |s: &str| s.parse::<u32>().ok();
    let date = t.get(..10)?;
    let mut parts = date.split('-');
    let (y, mo, d) = (
        num(parts.next()?)?,
        num(parts.next()?)?,
        num(parts.next()?)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days(i64::from(y), mo, d) * 86_400;
    let rest = &t[10..];
    if !rest.is_empty() {
        let rest = rest.strip_prefix(['T', ' '])?.strip_suffix('Z')?;
        let clock = rest.split('.').next()?;
        let mut hms = clock.split(':');
        let (h, mi, s) = (num(hms.next()?)?, num(hms.next()?)?, num(hms.next()?)?);
        if h > 23 || mi > 59 || s > 60 {
            return None;
        }
        secs += i64::from(h * 3600 + mi * 60 + s);
    }
    u64::try_from(secs).ok().map(|s| s * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_seconds() {
        for ms in [0, 951_782_400_000, 1_794_907_800_000, 4_102_444_799_000] {
            assert_eq!(parse(&format(ms)), Some(ms), "{}", format(ms));
        }
        assert_eq!(format(1_794_907_800_000), "2026-11-17T09:30:00Z");
        assert_eq!(parse("2026-11-17"), Some(1_794_873_600_000));
        assert_eq!(parse("2026-11-17T09:30:00.250Z"), Some(1_794_907_800_000));
        assert_eq!(parse("yesterday"), None);
        assert_eq!(parse("2026-13-01"), None);
    }
}
