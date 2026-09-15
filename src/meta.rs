//! `latest.json` / `in-progress.json` handling and request validation.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::steps::minutes_to_hours_string;
use crate::store::{SPATIAL_PREFIX, Store};
use crate::timefmt::{minutes_since, parse_json_time};

/// One model run as described by `latest.json` or `in-progress.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RunMeta {
    #[serde(default)]
    pub completed: bool,
    pub reference_time: String,
    #[serde(default)]
    pub valid_times: Vec<String>,
    #[serde(default)]
    pub variables: Vec<String>,
    #[serde(default)]
    pub crs_wkt: Option<String>,
    #[serde(default)]
    pub last_modified_time: Option<String>,
}

impl RunMeta {
    pub fn reference_time(&self) -> Result<DateTime<Utc>> {
        parse_json_time(&self.reference_time)
    }

    /// Available steps in minutes since the reference time, sorted.
    pub fn steps_minutes(&self) -> Result<Vec<i64>> {
        let init = self.reference_time()?;
        let mut v = self
            .valid_times
            .iter()
            .map(|t| parse_json_time(t).map(|vt| minutes_since(init, vt)))
            .collect::<Result<Vec<_>>>()?;
        v.sort_unstable();
        v.dedup();
        Ok(v)
    }
}

/// Both status files of a model. Either may be absent.
#[derive(Debug, Default)]
pub struct ModelStatus {
    pub latest: Option<RunMeta>,
    pub in_progress: Option<RunMeta>,
}

impl ModelStatus {
    pub async fn fetch(store: &Store, model: &str) -> Result<Self> {
        let latest = read_meta(store, model, "latest.json").await?;
        let in_progress = read_meta(store, model, "in-progress.json").await?;
        if latest.is_none() && in_progress.is_none() {
            bail!(
                "model {model:?} not found: neither latest.json nor in-progress.json exist under \
                 {SPATIAL_PREFIX}/{model}/ (run `om2nc models` to list available models)"
            );
        }
        Ok(Self { latest, in_progress })
    }

    /// Metadata for the run with the given reference time, if a status file describes it.
    pub fn run(&self, init: DateTime<Utc>) -> Result<Option<(&RunMeta, RunKind)>> {
        if let Some(m) = &self.latest
            && m.reference_time()? == init
        {
            return Ok(Some((m, RunKind::Latest)));
        }
        if let Some(m) = &self.in_progress
            && m.reference_time()? == init
        {
            return Ok(Some((m, RunKind::InProgress)));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Latest,
    InProgress,
}

async fn read_meta(store: &Store, model: &str, file: &str) -> Result<Option<RunMeta>> {
    let key = format!("{SPATIAL_PREFIX}/{model}/{file}");
    let Some(bytes) = store.read_opt(&key).await? else {
        return Ok(None);
    };
    let meta: RunMeta =
        serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {key}"))?;
    Ok(Some(meta))
}

/// Check that every requested step and variable is listed in `meta`.
pub fn validate_request(meta: &RunMeta, steps: &[i64], vars: &[String]) -> Result<()> {
    let have_steps: BTreeSet<i64> = meta.steps_minutes()?.into_iter().collect();
    let have_vars: BTreeSet<&str> = meta.variables.iter().map(String::as_str).collect();
    let missing_steps: Vec<String> = steps
        .iter()
        .filter(|s| !have_steps.contains(s))
        .map(|s| minutes_to_hours_string(*s))
        .collect();
    let missing_vars: Vec<&str> =
        vars.iter().map(String::as_str).filter(|v| !have_vars.contains(v)).collect();
    if missing_steps.is_empty() && missing_vars.is_empty() {
        return Ok(());
    }
    let mut msg = format!("run {} does not provide everything requested:", meta.reference_time);
    if !missing_steps.is_empty() {
        let avail: Vec<String> = have_steps.iter().map(|m| minutes_to_hours_string(*m)).collect();
        msg.push_str(&format!(
            "\n  missing steps (hours): {}\n  available steps: {}",
            missing_steps.join(","),
            summarise(&avail)
        ));
    }
    if !missing_vars.is_empty() {
        msg.push_str(&format!(
            "\n  missing variables: {}\n  available variables: {}",
            missing_vars.join(","),
            meta.variables.join(",")
        ));
    }
    bail!(msg)
}

fn summarise(items: &[String]) -> String {
    if items.len() <= 12 {
        items.join(",")
    } else {
        format!(
            "{} ... {} ({} total)",
            items[..6].join(","),
            items[items.len() - 3..].join(","),
            items.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> RunMeta {
        serde_json::from_str(
            r#"{"completed":true,"reference_time":"2026-09-15T00:00:00Z",
                "valid_times":["2026-09-15T00:00Z","2026-09-15T03:00Z","2026-09-15T06:00Z"],
                "variables":["temperature_2m","precipitation"]}"#,
        )
        .unwrap()
    }

    #[test]
    fn steps_from_valid_times() {
        assert_eq!(meta().steps_minutes().unwrap(), vec![0, 180, 360]);
    }

    #[test]
    fn validation() {
        let m = meta();
        assert!(validate_request(&m, &[0, 180], &["temperature_2m".into()]).is_ok());
        let err = validate_request(&m, &[0, 60, 540], &["temperature_2m".into(), "cape".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing steps (hours): 1,9"), "{err}");
        assert!(err.contains("missing variables: cape"), "{err}");
    }
}
