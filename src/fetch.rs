//! The `fetch` command: validate, read all (step, variable) slabs, write NetCDF.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::cf::{Reduction, interval_reduction};
use crate::grid::{BBox, Sampler, SourceGrid, Subset};
use crate::meta::{ModelStatus, RunKind, validate_request};
use crate::nc::{NcSpec, NcWriter, VarSpec};
use crate::om::StepFile;
use crate::steps::minutes_to_hours_string;
use crate::store::{SPATIAL_PREFIX, Store};
use crate::timefmt::{iso, step_key};

pub struct FetchRequest {
    pub model: String,
    /// `None` means "latest completed run".
    pub init: Option<DateTime<Utc>>,
    /// `None` means "every step the run provides".
    pub steps_minutes: Option<Vec<i64>>,
    /// Empty means "every variable the run provides".
    pub vars: Vec<String>,
    pub bbox: Option<BBox>,
    /// Output spacing in degrees; only for reduced Gaussian sources.
    pub resolution: Option<f64>,
    pub output: PathBuf,
    pub concurrency: usize,
    pub allow_incomplete: bool,
    /// Fill NaN (with a warning) when a step file lacks a variable, instead of failing.
    pub missing_as_nan: bool,
    /// Combine native output files so interval variables cover the whole gap
    /// between consecutive requested steps.
    pub accumulate: bool,
    pub overwrite: bool,
    pub deflate_level: i32,
    pub block_size: u64,
    pub io_merge: u64,
    pub history: String,
    pub bucket: String,
}

pub struct FetchSummary {
    pub init: DateTime<Utc>,
    pub steps: usize,
    pub vars: usize,
    pub ny: usize,
    pub nx: usize,
    pub bytes_remote: u64,
}

pub async fn run(store: &Store, req: FetchRequest) -> Result<FetchSummary> {
    if req.output.exists() && !req.overwrite {
        bail!("{} exists; pass --overwrite to replace it", req.output.display());
    }

    // 1. Resolve the run from the status files and validate against them.
    let status = ModelStatus::fetch(store, &req.model).await?;
    let init = match req.init {
        Some(t) => t,
        None => status
            .latest
            .as_ref()
            .context("no completed run available (latest.json missing); pass --init explicitly")?
            .reference_time()?,
    };
    let run = status.run(init)?;
    let steps: Vec<i64> = match (&run, &req.steps_minutes) {
        (Some((meta, kind)), _) => {
            if *kind == RunKind::InProgress && !meta.completed && !req.allow_incomplete {
                bail!(
                    "run {} of {} is still being processed (in-progress.json, {} steps so far); \
                     wait for latest.json to advance or pass --allow-incomplete",
                    iso(init),
                    req.model,
                    meta.valid_times.len()
                );
            }
            let steps = match &req.steps_minutes {
                Some(s) => s.clone(),
                None => meta.steps_minutes()?,
            };
            validate_request(meta, &steps, &req.vars)?;
            steps
        }
        (None, Some(s)) => {
            log::warn!(
                "run {} is not described by latest.json/in-progress.json (older run?); \
                 validating against the object listing instead",
                iso(init)
            );
            s.clone()
        }
        (None, None) => bail!(
            "run {} is not described by latest.json/in-progress.json; pass --step explicitly",
            iso(init)
        ),
    };
    if steps.is_empty() {
        bail!("no steps selected");
    }
    // `--var` omitted: every variable listed for the run (older runs: from the first file, below).
    let mut vars: Vec<String> = req.vars.clone();
    if vars.is_empty()
        && let Some((meta, _)) = &run
    {
        vars = meta.variables.clone();
        log::info!("no --var given: fetching all {} variables of the run", vars.len());
    }
    let available = match &run {
        Some((meta, _)) => Some(meta.steps_minutes()?),
        None => None,
    };
    if req.accumulate && available.is_none() {
        bail!(
            "--accumulate needs the run's native output times, which only latest.json/in-progress.json provide"
        );
    }
    let plan = plan_steps(&steps, available.as_deref(), req.accumulate);
    let interval_comment = interval_comment(available.as_deref(), req.accumulate);

    // 2. Every source file must exist before we touch the output.
    let keys: Vec<String> =
        plan.unique_steps().iter().map(|m| step_key(&req.model, init, *m)).collect();
    let sizes = stat_all(store, &keys, req.concurrency.max(8)).await?;
    let missing: Vec<String> = sizes
        .iter()
        .zip(plan.unique_steps())
        .filter(|(s, _)| s.is_none())
        .map(|(_, m)| minutes_to_hours_string(*m))
        .collect();
    if !missing.is_empty() {
        bail!(
            "run {} of {}: {} of {} step files are missing (hours: {})",
            iso(init),
            req.model,
            missing.len(),
            keys.len(),
            missing.join(",")
        );
    }
    let bytes_remote: u64 = sizes.iter().flatten().sum();
    if plan.unique_steps().len() > steps.len() {
        log::info!(
            "--accumulate: reading {} native output files for {} output steps",
            plan.unique_steps().len(),
            steps.len()
        );
    }

    // 3. Grid, units and variable presence from the first file(s).
    let first_key = step_key(&req.model, init, steps[0]);
    let second_key = steps.get(1).map(|s| step_key(&req.model, init, *s));
    let first = StepFile::open(store, &first_key, req.block_size)
        .await?
        .with_context(|| format!("object vanished: {first_key}"))?;
    check_times(&first, init, steps[0])?;
    let crs_wkt = first.crs_wkt()?;
    let present: BTreeSet<&str> = first.variable_names().into_iter().collect();
    if vars.is_empty() {
        vars = present.iter().map(|v| v.to_string()).collect();
        log::info!("no --var given: fetching all {} variables found in {first_key}", vars.len());
    }
    if vars.is_empty() {
        bail!("{first_key} contains no array variables");
    }
    let interval_vars: Vec<&str> =
        vars.iter().map(String::as_str).filter(|v| interval_reduction(v).is_some()).collect();
    if plan.subsampled && !interval_vars.is_empty() {
        log::warn!(
            "requested steps skip native output times: {} will only cover the last native interval \
             before each step (see time_bnds); pass --accumulate to combine them",
            interval_vars.join(",")
        );
    }
    let absent: Vec<&str> =
        vars.iter().map(String::as_str).filter(|v| !present.contains(v)).collect();
    if !absent.is_empty() && !req.missing_as_nan {
        bail!(
            "{}: variables not present in file: {}\n  (accumulated variables such as \
             precipitation are usually absent at step 0: drop step 0 or pass --missing-as-nan)\n  \
             available: {}",
            first_key,
            absent.join(","),
            present.iter().copied().collect::<Vec<_>>().join(",")
        );
    }
    // Variables absent from the first file take their units/shape from the second one.
    let second = match (&second_key, absent.is_empty()) {
        (Some(k), false) => Some(
            StepFile::open(store, k, req.block_size)
                .await?
                .with_context(|| format!("object vanished: {k}"))?,
        ),
        _ => None,
    };
    let mut var_specs = Vec::with_capacity(vars.len());
    let mut dims: Option<Vec<u64>> = None;
    for v in &vars {
        let info = if present.contains(v.as_str()) {
            first.var_info(v).await?
        } else {
            match &second {
                Some(f) => f.var_info(v).await.with_context(|| {
                    format!("{v} is present in neither {first_key} nor {}", f.key)
                })?,
                None => bail!("{v} is not present in {first_key}"),
            }
        };
        match &dims {
            None => dims = Some(info.dims.clone()),
            Some(d) if *d != info.dims => {
                bail!("variable {v} has shape {:?} but {} has {:?}", info.dims, vars[0], d)
            }
            _ => {}
        }
        var_specs.push(VarSpec {
            name: v.clone(),
            om_units: info.units,
            reduction: interval_reduction(v),
        });
    }
    let dims = dims.unwrap();
    let source = SourceGrid::detect(&crs_wkt, &dims)
        .with_context(|| format!("model {} cannot be written as a lat/lon NetCDF", req.model))?;
    let subset = source.plan(req.bbox.as_ref(), req.resolution, &req.model)?;
    log::info!(
        "source grid {}; output {} x {} cells (lat {:.3}..{:.3}, lon {:.3}..{:.3}), {}",
        source.describe(),
        subset.lats.len(),
        subset.lons.len(),
        subset.lats[0],
        subset.lats[subset.lats.len() - 1],
        subset.lons[0],
        subset.lons[subset.lons.len() - 1],
        subset.sampler.describe()
    );
    let (source_grid, regrid_method) = match &source {
        SourceGrid::Regular(_) => (None, None),
        SourceGrid::ReducedGaussian(g) => {
            (Some(format!("octahedral reduced Gaussian O{}", g.n)), Some("nearest"))
        }
    };
    drop(second);

    // 4. Create the output (as a temp file; renamed only on success).
    let part = part_path(&req.output);
    let source_attr = format!(
        "Open-Meteo open data, s3://{}/{SPATIAL_PREFIX}/{}/ (model {}, reference time {})",
        req.bucket,
        req.model,
        req.model,
        iso(init)
    );
    let mut nc = NcWriter::create(
        &part,
        &NcSpec {
            model: &req.model,
            init,
            steps_minutes: &steps,
            interval_minutes: available.as_ref().map(|_| plan.intervals()).as_deref(),
            interval_comment: &interval_comment,
            lats: &subset.lats,
            lons: &subset.lons,
            vars: &var_specs,
            crs_wkt: &crs_wkt,
            history: &req.history,
            source: &source_attr,
            source_grid: source_grid.as_deref(),
            regrid_method,
            deflate_level: req.deflate_level,
        },
    )?;

    // 5. Read slabs concurrently, write as they arrive.
    let (ny, nx) = (subset.lats.len(), subset.lons.len());
    let nvars = vars.len();
    let read_plan =
        ReadPlan { init, outputs: &plan.outputs, subset: Arc::new(subset), vars: Arc::new(vars) };
    let result = read_all(store, &req, &read_plan, first, &mut nc).await;
    if let Err(e) = result {
        drop(nc);
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    nc.close()?;
    std::fs::rename(&part, &req.output)
        .with_context(|| format!("cannot move {} to {}", part.display(), req.output.display()))?;

    Ok(FetchSummary { init, steps: steps.len(), vars: nvars, ny, nx, bytes_remote })
}

/// One output time step and the native files that feed it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputStep {
    pub step: i64,
    /// Interval (minutes since init) covered by interval variables.
    pub interval: (i64, i64),
    /// Native files to read, ascending; the last one is `step` itself.
    pub sources: Vec<SourceStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceStep {
    pub step: i64,
    /// Length of the native interval this file covers (minutes).
    pub duration: i64,
}

#[derive(Debug, Default, PartialEq)]
pub struct StepPlan {
    pub outputs: Vec<OutputStep>,
    /// True when, without `--accumulate`, native output times fall between requested steps.
    pub subsampled: bool,
    unique: Vec<i64>,
}

impl StepPlan {
    pub fn unique_steps(&self) -> &[i64] {
        &self.unique
    }

    pub fn intervals(&self) -> Vec<(i64, i64)> {
        self.outputs.iter().map(|o| o.interval).collect()
    }
}

/// Decide which native files feed each requested step.
///
/// * Without `accumulate`, each step reads its own file; its interval is
///   `(previous native time, step]`.
/// * With `accumulate`, every native file in `(previous requested step, step]`
///   is read (from init for the first step) so interval variables can be
///   combined across the gap.
///
/// `available` is the run's native output times; `None` when unknown.
pub fn plan_steps(steps: &[i64], available: Option<&[i64]>, accumulate: bool) -> StepPlan {
    let prev_native = |x: i64| -> i64 {
        available.and_then(|av| av.iter().copied().filter(|&a| a < x).max()).unwrap_or(0).min(x)
    };
    let mut outputs = Vec::with_capacity(steps.len());
    let mut subsampled = false;
    for (i, &s) in steps.iter().enumerate() {
        let native_start = prev_native(s);
        let (start, sources) = match available {
            Some(av) if accumulate => {
                let start = if i == 0 { 0.min(s) } else { steps[i - 1] };
                let mut src: Vec<SourceStep> = av
                    .iter()
                    .copied()
                    .filter(|&a| a > start && a <= s)
                    .map(|a| SourceStep { step: a, duration: a - prev_native(a).max(start) })
                    .collect();
                if src.is_empty() {
                    src.push(SourceStep { step: s, duration: s - native_start });
                }
                (start, src)
            }
            Some(_) => {
                if i > 0 && native_start > steps[i - 1] {
                    subsampled = true;
                }
                (native_start, vec![SourceStep { step: s, duration: s - native_start }])
            }
            None => (s, vec![SourceStep { step: s, duration: 0 }]),
        };
        outputs.push(OutputStep { step: s, interval: (start, s), sources });
    }
    let mut unique: Vec<i64> =
        outputs.iter().flat_map(|o| o.sources.iter().map(|x| x.step)).collect();
    unique.sort_unstable();
    unique.dedup();
    StepPlan { outputs, subsampled, unique }
}

/// Text for the `comment` attribute of interval variables.
fn interval_comment(available: Option<&[i64]>, accumulate: bool) -> String {
    let Some(av) = available else {
        return "Value covers the interval since the previous model output time; the native \
                output interval of this run is unknown."
            .to_string();
    };
    // Describe the native cadence as runs of equal spacing, e.g. "1 h to +90 h, 3 h to +144 h".
    let mut runs: Vec<(i64, i64)> = Vec::new(); // (spacing, last step)
    for w in av.windows(2) {
        let d = w[1] - w[0];
        match runs.last_mut() {
            Some((sp, last)) if *sp == d => *last = w[1],
            _ => runs.push((d, w[1])),
        }
    }
    let cadence: Vec<String> = runs
        .iter()
        .map(|(d, last)| {
            format!("{} h to +{} h", minutes_to_hours_string(*d), minutes_to_hours_string(*last))
        })
        .collect();
    if accumulate {
        format!(
            "Combined by om2nc --accumulate from the model's native output files so that each \
             value covers the interval between consecutive output steps (time_bnds). Native \
             output interval of this run: {}.",
            cadence.join(", ")
        )
    } else {
        format!(
            "Value covers only the interval since the previous native model output time \
             (time_bnds), not since the previous step in this file. Native output interval of \
             this run: {}.",
            cadence.join(", ")
        )
    }
}

/// Slabs read from one source file, aligned with the variable list; `None`
/// for instantaneous variables that this source does not provide.
type SourceSlabs = Vec<Option<Vec<f32>>>;

/// Variables read in parallel from one file (multiplied by `--concurrency` files).
const VAR_CONCURRENCY: usize = 8;

struct ReadPlan<'a> {
    init: DateTime<Utc>,
    outputs: &'a [OutputStep],
    subset: Arc<Subset>,
    vars: Arc<Vec<String>>,
}

async fn read_all(
    store: &Store,
    req: &FetchRequest,
    plan: &ReadPlan<'_>,
    first: StepFile,
    nc: &mut NcWriter,
) -> Result<()> {
    let ReadPlan { init, outputs, subset, vars } = plan;
    let init = *init;
    // Permits are taken per source file, not per output step, so accumulated
    // steps with many sources read them in parallel too.
    let sem = Arc::new(Semaphore::new(req.concurrency.max(1)));
    let vars = Arc::clone(vars);
    let reductions: Arc<Vec<Option<Reduction>>> =
        Arc::new(vars.iter().map(|v| interval_reduction(v)).collect());
    let n = subset.lats.len() * subset.lons.len();
    let mut set: JoinSet<Result<(usize, Vec<Vec<f32>>)>> = JoinSet::new();
    let mut first = Some(Arc::new(first));
    for (t, out) in outputs.iter().enumerate() {
        let out = out.clone();
        let vars = vars.clone();
        let reductions = reductions.clone();
        // The pre-opened first file is the final source of output step 0.
        let preopened = first.take();
        let mut sources: JoinSet<Result<(usize, SourceSlabs)>> = JoinSet::new();
        for (i, src) in out.sources.iter().enumerate() {
            let is_last = i + 1 == out.sources.len();
            let key = step_key(&req.model, init, src.step);
            let preopened = preopened.clone().filter(|f| is_last && f.key == key);
            let (sem, store, vars, reductions, subset) =
                (sem.clone(), store.clone(), vars.clone(), reductions.clone(), Arc::clone(subset));
            let (block_size, io_merge, missing_as_nan, step) =
                (req.block_size, req.io_merge, req.missing_as_nan, src.step);
            sources.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                let file = match preopened {
                    Some(f) => f,
                    None => {
                        let f = StepFile::open(&store, &key, block_size)
                            .await?
                            .with_context(|| format!("object vanished: {key}"))?;
                        check_times(&f, init, step)?;
                        Arc::new(f)
                    }
                };
                // Variables of one file are independent: read up to VAR_CONCURRENCY at once.
                let var_sem = Arc::new(Semaphore::new(VAR_CONCURRENCY));
                let mut reads: JoinSet<Result<(usize, Vec<f32>)>> = JoinSet::new();
                for (vi, (v, r)) in vars.iter().zip(reductions.iter()).enumerate() {
                    // Instantaneous variables only come from the step's own file.
                    if r.is_none() && !is_last {
                        continue;
                    }
                    let (file, v, subset, var_sem) =
                        (file.clone(), v.clone(), subset.clone(), var_sem.clone());
                    reads.spawn(async move {
                        let _p = var_sem.acquire_owned().await.expect("semaphore closed");
                        let slab =
                            read_one(&file, &v, &subset.sampler, io_merge, missing_as_nan, n)
                                .await?;
                        Ok((vi, slab))
                    });
                }
                let mut slabs: SourceSlabs = vec![None; vars.len()];
                while let Some(joined) = reads.join_next().await {
                    let (vi, slab) = joined.context("variable read task panicked")??;
                    slabs[vi] = Some(slab);
                }
                Ok((i, slabs))
            });
        }
        set.spawn(async move {
            let mut per_source: Vec<Option<SourceSlabs>> = vec![None; out.sources.len()];
            while let Some(joined) = sources.join_next().await {
                let (i, slabs) = joined.context("source task panicked")??;
                per_source[i] = Some(slabs);
            }
            let mut result = Vec::with_capacity(vars.len());
            for (vi, r) in reductions.iter().enumerate() {
                let take = |i: usize| per_source[i].as_ref().and_then(|s| s[vi].clone());
                let slab = match r {
                    None => take(out.sources.len() - 1).expect("last source read"),
                    Some(r) => {
                        let mut acc: Option<Vec<f32>> = None;
                        let mut total = 0.0f32;
                        for (i, src) in out.sources.iter().enumerate() {
                            let s = take(i).expect("interval variable read from every source");
                            let d = src.duration as f32;
                            total += d;
                            acc = Some(match acc {
                                None => match r {
                                    Reduction::Mean => s.iter().map(|x| x * d).collect(),
                                    _ => s,
                                },
                                Some(mut a) => {
                                    for (a, x) in a.iter_mut().zip(&s) {
                                        *a = match r {
                                            Reduction::Sum => *a + x,
                                            Reduction::Mean => *a + x * d,
                                            Reduction::Maximum => a.max(*x),
                                            Reduction::Minimum => a.min(*x),
                                        };
                                    }
                                    a
                                }
                            });
                        }
                        let mut a = acc.expect("at least one source");
                        if *r == Reduction::Mean && total > 0.0 {
                            a.iter_mut().for_each(|x| *x /= total);
                        }
                        a
                    }
                };
                result.push(slab);
            }
            log::info!(
                "step +{}h done ({} file{})",
                minutes_to_hours_string(out.step),
                out.sources.len(),
                if out.sources.len() == 1 { "" } else { "s" }
            );
            Ok((t, result))
        });
    }
    let mut done = 0usize;
    while let Some(joined) = set.join_next().await {
        let (t, slabs) = joined.context("fetch task panicked")??;
        for (v, data) in vars.iter().zip(slabs) {
            nc.write_slab(v, t, &data)?;
        }
        done += 1;
        log::debug!("wrote {done}/{} steps", outputs.len());
    }
    Ok(())
}

/// One slab from one file, honouring `--missing-as-nan`.
async fn read_one(
    file: &StepFile,
    var: &str,
    sampler: &Sampler,
    io_merge: u64,
    missing_as_nan: bool,
    n: usize,
) -> Result<Vec<f32>> {
    if !file.has_variable(var) {
        if !missing_as_nan {
            bail!(
                "{}: variable {var} not present in file (pass --missing-as-nan to fill)",
                file.key
            );
        }
        log::warn!("{}: {var} not present, filling with NaN", file.key);
        return Ok(vec![f32::NAN; n]);
    }
    sample(file, var, sampler, io_merge).await
}

/// Produce one output slab (row-major over lats × lons) for `var`.
async fn sample(file: &StepFile, var: &str, sampler: &Sampler, io_merge: u64) -> Result<Vec<f32>> {
    match sampler {
        Sampler::Crop2d { rows, cols } => {
            file.read_subset(var, rows.clone(), cols.clone(), io_merge).await
        }
        Sampler::Nearest1d { window, map } => {
            let src = file.read_window_1d(var, window.clone(), io_merge).await?;
            Ok(map.iter().map(|&i| src[i as usize]).collect())
        }
    }
}

/// Sanity check the file's own timestamps against what the key promised.
fn check_times(file: &StepFile, init: DateTime<Utc>, step_minutes: i64) -> Result<()> {
    if let Some(t) = file.scalar_time("forecast_reference_time")
        && t != init
    {
        bail!("{}: forecast_reference_time is {} but expected {}", file.key, iso(t), iso(init));
    }
    if let Some(t) = file.scalar_time("valid_time") {
        let want = init + chrono::TimeDelta::minutes(step_minutes);
        if t != want {
            bail!("{}: valid_time is {} but expected {}", file.key, iso(t), iso(want));
        }
    }
    Ok(())
}

async fn stat_all(store: &Store, keys: &[String], concurrency: usize) -> Result<Vec<Option<u64>>> {
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    for (i, key) in keys.iter().enumerate() {
        let sem = sem.clone();
        let store = store.clone();
        let key = key.clone();
        set.spawn(async move {
            let _p = sem.acquire_owned().await.expect("semaphore closed");
            store.size_opt(&key).await.map(|s| (i, s))
        });
    }
    let mut out = vec![None; keys.len()];
    while let Some(r) = set.join_next().await {
        let (i, s) = r.context("stat task panicked")??;
        out[i] = s;
    }
    Ok(out)
}

fn part_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    output.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(x: i64) -> i64 {
        x * 60
    }

    // ecmwf_ifs-like cadence: hourly to 6 h, then 3-hourly to 12 h.
    fn av() -> Vec<i64> {
        let mut v: Vec<i64> = (0..=6).map(h).collect();
        v.extend([9, 12].map(h));
        v
    }

    #[test]
    fn native_intervals_without_accumulate() {
        let p = plan_steps(&[h(0), h(3), h(6), h(12)], Some(&av()), false);
        assert!(p.subsampled);
        assert_eq!(p.outputs[0].interval, (0, 0));
        assert_eq!(p.outputs[1].interval, (h(2), h(3)));
        assert_eq!(p.outputs[3].interval, (h(9), h(12)));
        assert_eq!(p.outputs[3].sources, vec![SourceStep { step: h(12), duration: h(3) }]);
        assert_eq!(p.unique_steps(), &[h(0), h(3), h(6), h(12)]);
        // Requesting every native step is not subsampled.
        assert!(!plan_steps(&av(), Some(&av()), false).subsampled);
    }

    #[test]
    fn accumulate_combines_native_files() {
        let p = plan_steps(&[h(3), h(6), h(12)], Some(&av()), true);
        assert!(!p.subsampled);
        assert_eq!(p.outputs[0].interval, (0, h(3)));
        assert_eq!(
            p.outputs[0].sources.iter().map(|s| (s.step, s.duration)).collect::<Vec<_>>(),
            vec![(h(1), h(1)), (h(2), h(1)), (h(3), h(1))]
        );
        assert_eq!(p.outputs[1].interval, (h(3), h(6)));
        assert_eq!(p.outputs[2].interval, (h(6), h(12)));
        assert_eq!(
            p.outputs[2].sources.iter().map(|s| (s.step, s.duration)).collect::<Vec<_>>(),
            vec![(h(9), h(3)), (h(12), h(3))]
        );
        assert_eq!(p.unique_steps().len(), 8);
    }

    #[test]
    fn unknown_cadence() {
        let p = plan_steps(&[h(3), h(6)], None, false);
        assert_eq!(p.outputs[1].interval, (h(6), h(6)));
        assert_eq!(p.outputs[1].sources, vec![SourceStep { step: h(6), duration: 0 }]);
    }

    #[test]
    fn comment_describes_cadence() {
        let c = interval_comment(Some(&av()), false);
        assert!(c.contains("1 h to +6 h, 3 h to +12 h"), "{c}");
        assert!(interval_comment(Some(&av()), true).starts_with("Combined by om2nc"));
    }
}
