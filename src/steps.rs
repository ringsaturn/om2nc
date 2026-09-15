//! Forecast-step specification parsing.
//!
//! Grammar (hours, decimals allowed): `0..144:3`, `0..48` (1 h stride),
//! `0,6,12`, `3`, and comma-separated combinations. Steps are kept in minutes
//! so that 15-minute models work without floating point surprises.

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
#[error("invalid step spec {spec:?}: {reason}")]
pub struct StepError {
    spec: String,
    reason: String,
}

/// Parse a step spec into a sorted, de-duplicated list of minutes since init.
pub fn parse_steps(spec: &str) -> Result<Vec<i64>, StepError> {
    let err = |reason: &str| StepError { spec: spec.to_string(), reason: reason.to_string() };
    let mut out = Vec::new();
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(err("empty element"));
        }
        if let Some((range, stride)) = item.split_once("..") {
            let (end, stride) = match stride.split_once(':') {
                Some((e, s)) => (e, hours_to_minutes(s).ok_or_else(|| err("bad stride"))?),
                None => (stride, 60),
            };
            let start = hours_to_minutes(range).ok_or_else(|| err("bad range start"))?;
            let end = hours_to_minutes(end).ok_or_else(|| err("bad range end"))?;
            if stride <= 0 {
                return Err(err("stride must be > 0"));
            }
            if end < start {
                return Err(err("range end before start"));
            }
            let mut m = start;
            while m <= end {
                out.push(m);
                m += stride;
            }
        } else {
            out.push(hours_to_minutes(item).ok_or_else(|| err("bad value"))?);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn hours_to_minutes(s: &str) -> Option<i64> {
    let h: f64 = s.trim().parse().ok()?;
    if !h.is_finite() || h < 0.0 {
        return None;
    }
    Some((h * 60.0).round() as i64)
}

/// Format minutes as a human readable hour value (`3`, `0.25`).
pub fn minutes_to_hours_string(m: i64) -> String {
    if m % 60 == 0 { format!("{}", m / 60) } else { format!("{}", m as f64 / 60.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_with_stride() {
        assert_eq!(parse_steps("0..12:3").unwrap(), vec![0, 180, 360, 540, 720]);
    }

    #[test]
    fn range_default_stride_and_list() {
        assert_eq!(parse_steps("0..2").unwrap(), vec![0, 60, 120]);
        assert_eq!(parse_steps("6,0,3,3").unwrap(), vec![0, 180, 360]);
        assert_eq!(parse_steps("0..1:0.25").unwrap(), vec![0, 15, 30, 45, 60]);
        assert_eq!(parse_steps("0..3:1, 12").unwrap(), vec![0, 60, 120, 180, 720]);
    }

    #[test]
    fn errors() {
        assert!(parse_steps("").is_err());
        assert!(parse_steps("5..1").is_err());
        assert!(parse_steps("0..5:0").is_err());
        assert!(parse_steps("-3").is_err());
        assert!(parse_steps("a..b").is_err());
    }

    #[test]
    fn hours_string() {
        assert_eq!(minutes_to_hours_string(180), "3");
        assert_eq!(minutes_to_hours_string(15), "0.25");
    }
}
