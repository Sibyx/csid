//! Small dependency-free helpers: UTC timestamp formatting and shell-out.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch (the wallclock anchor stamped on records).
///
/// Only the Linux capture path stamps records, so this is dead code on the
/// development builds used on other platforms.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Civil date from a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_time(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// `2026-07-22T09:31:07Z` — the sidecar timestamp format.
pub fn rfc3339_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = split_time(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `20260722-093107` — the compact stamp used in session ids.
pub fn compact_stamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = split_time(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Run a command, returning trimmed stdout. Fails if the command fails.
pub fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawning `{program} {}`", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a command, returning `None` instead of an error (for best-effort probes
/// that must never fail a session, e.g. sidecar environment capture).
pub fn run_opt(program: &str, args: &[&str]) -> Option<String> {
    run(program, args).ok()
}

/// Lowercase colon-separated MAC, the form every sidecar and log line uses.
///
/// One implementation on purpose: a census that renders `02:6D:6F:6E:00:13`
/// and an analysis that scopes to `02:6d:6f:6e:00:13` silently select nothing.
pub fn format_mac(m: &[u8; 6]) -> String {
    let mut s = String::with_capacity(17);
    for (i, b) in m.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epochs() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // Cross-checked against `date -u -r <epoch>`.
        assert_eq!(rfc3339_utc(1_784_712_667), "2026-07-22T09:31:07Z");
        assert_eq!(compact_stamp(1_784_712_667), "20260722-093107");
        assert_eq!(rfc3339_utc(1_784_799_067), "2026-07-23T09:31:07Z");
    }

    #[test]
    fn handles_leap_day() {
        // 2024-02-29T00:00:00Z
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
