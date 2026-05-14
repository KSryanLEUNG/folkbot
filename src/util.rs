//! Tiny shared utilities. Date / time formatting + the process-wide
//! timezone offset used by `fmt_ts` / `fmt_date` / `summaries::yesterday_window`.
//!
//! The offset is configurable via `[agent] timezone_offset_secs` in
//! folkbot.toml. Default is +28800 (Asia/Hong_Kong, UTC+8) for backwards
//! compatibility — if the field is omitted we behave exactly like v1.2.

use std::sync::OnceLock;

/// Default timezone offset (Asia/Hong_Kong, UTC+8) in seconds.
pub const DEFAULT_TZ_OFFSET_SECS: i64 = 8 * 3600;

static TZ_OFFSET: OnceLock<i64> = OnceLock::new();

/// Initialize the process-wide timezone offset. Called once during
/// bootstrap from `[agent].timezone_offset_secs`. Subsequent calls are
/// no-ops (OnceLock semantics) — restart to change the offset.
pub fn set_tz_offset(secs: i64) {
    let _ = TZ_OFFSET.set(secs);
}

/// Read the configured offset, or fall back to the HK default if `set_tz_offset`
/// hasn't been called (background tasks running before bootstrap, tests, etc.).
pub fn tz_offset() -> i64 {
    *TZ_OFFSET.get().unwrap_or(&DEFAULT_TZ_OFFSET_SECS)
}

/// "YYYY-MM-DD HH:MM" in the configured local timezone.
/// Howard Hinnant civil-from-days algorithm; no external deps.
pub fn fmt_ts(ts: i64) -> String {
    let local = ts + tz_offset();
    let days = local / 86400;
    let secs_of_day = local % 86400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let (year, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", year, mo, d, h, m)
}

/// "YYYY-MM-DD" only.
pub fn fmt_date(ts: i64) -> String {
    let local = ts + tz_offset();
    let days = local / 86400;
    let (year, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", year, mo, d)
}

fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mo <= 2 { y + 1 } else { y };
    (year, mo, d)
}
