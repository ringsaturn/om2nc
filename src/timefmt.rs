//! Time parsing/formatting for init times, Open-Meteo JSON timestamps and object keys.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};

/// Parse an init/valid time. Accepts `2026-09-15T00Z`, `2026-09-15T0000Z`,
/// `2026-09-15T00:00Z`, `2026-09-15T00:00:00Z` (a trailing `Z` is optional).
pub fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    let t = s.trim().trim_end_matches('Z');
    // chrono needs at least hour and minute; accept a bare hour ("...T00").
    let t = if t.len() == "2026-09-15T00".len() { format!("{t}00") } else { t.to_string() };
    const FORMATS: &[&str] =
        &["%Y-%m-%dT%H%M", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"];
    for f in FORMATS {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(&t, f) {
            return Ok(ndt.and_utc());
        }
    }
    bail!("cannot parse time {s:?} (expected e.g. 2026-09-15T00Z)")
}

/// Object key prefix of one model run: `data_spatial/{model}/YYYY/MM/DD/HHMMZ/`.
pub fn run_prefix(model: &str, init: DateTime<Utc>) -> String {
    format!("{}/{model}/{}/", crate::store::SPATIAL_PREFIX, init.format("%Y/%m/%d/%H%MZ"))
}

/// Object key of one forecast step file inside a run.
pub fn step_key(model: &str, init: DateTime<Utc>, step_minutes: i64) -> String {
    let valid = init + TimeDelta::minutes(step_minutes);
    format!("{}{}.om", run_prefix(model, init), valid.format("%Y-%m-%dT%H%M"))
}

/// Minutes between `valid` and `init` (may be negative for bogus input).
pub fn minutes_since(init: DateTime<Utc>, valid: DateTime<Utc>) -> i64 {
    (valid - init).num_minutes()
}

/// Parse a JSON timestamp from `latest.json` (`2026-09-15T00:00:00Z` / `2026-09-15T03:00Z`).
pub fn parse_json_time(s: &str) -> Result<DateTime<Utc>> {
    parse_utc(s).with_context(|| format!("bad timestamp in JSON: {s:?}"))
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants() {
        let want = parse_utc("2026-09-15T00:00:00Z").unwrap();
        for s in ["2026-09-15T00Z", "2026-09-15T0000Z", "2026-09-15T00:00Z", "2026-09-15T00"] {
            assert_eq!(parse_utc(s).unwrap(), want, "{s}");
        }
        assert!(parse_utc("yesterday").is_err());
    }

    #[test]
    fn keys() {
        let init = parse_utc("2026-09-15T00Z").unwrap();
        assert_eq!(run_prefix("ecmwf_ifs025", init), "data_spatial/ecmwf_ifs025/2026/09/15/0000Z/");
        assert_eq!(
            step_key("ecmwf_ifs025", init, 27 * 60),
            "data_spatial/ecmwf_ifs025/2026/09/15/0000Z/2026-09-16T0300.om"
        );
        assert_eq!(step_key("x", init, 15), "data_spatial/x/2026/09/15/0000Z/2026-09-15T0015.om");
    }
}
