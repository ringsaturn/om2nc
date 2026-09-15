//! om2nc — fetch Open-Meteo spatial `.om` forecasts and write CF NetCDF.

mod cf;
mod fetch;
mod gaussian;
mod grid;
mod meta;
mod nc;
mod om;
mod steps;
mod store;
mod timefmt;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::grid::BBox;
use crate::meta::ModelStatus;
use crate::store::{SPATIAL_PREFIX, Store, StoreConfig};
use crate::timefmt::{iso, parse_utc, run_prefix, step_key};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "om2nc", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    store: StoreArgs,

    /// Increase log verbosity (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Only print warnings and errors.
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct StoreArgs {
    /// S3 bucket holding the Open-Meteo open data.
    #[arg(long, global = true, env = "OM2NC_BUCKET", default_value = store::DEFAULT_BUCKET)]
    bucket: String,

    /// S3 region of the bucket.
    #[arg(long, global = true, env = "OM2NC_REGION", default_value = store::DEFAULT_REGION)]
    region: String,

    /// Custom S3 endpoint URL (mirrors, MinIO, ...). Default: AWS.
    #[arg(long, global = true, env = "OM2NC_ENDPOINT")]
    endpoint: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Download steps/variables of one model run into a NetCDF file.
    Fetch(FetchArgs),
    /// List models available under data_spatial/.
    Models,
    /// Show latest.json / in-progress.json of a model.
    Info {
        /// Model name as listed by `om2nc models`.
        #[arg(long)]
        model: String,
        /// Print the full JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Print the variable tree of one .om step file.
    Inspect {
        /// Model name as listed by `om2nc models`.
        #[arg(long)]
        model: String,
        /// Run reference time (e.g. 2026-09-15T00Z) or "latest".
        #[arg(long, default_value = "latest")]
        init: String,
        /// Forecast step in hours.
        #[arg(long, default_value = "0")]
        step: String,
    },
}

#[derive(Args)]
struct FetchArgs {
    /// Model name as listed by `om2nc models` (e.g. ecmwf_ifs025).
    #[arg(long)]
    model: String,

    /// Run reference time, e.g. 2026-09-15T00Z, or "latest" (default).
    #[arg(long, default_value = "latest")]
    init: String,

    /// Forecast steps in hours: "0..144:3", "0..48", "0,6,12". Default: all steps of the run.
    #[arg(long)]
    step: Option<String>,

    /// Variables, comma separated and/or repeated (Open-Meteo names). Default: all variables of the run.
    #[arg(long = "var", value_delimiter = ',')]
    vars: Vec<String>,

    /// Bounding box west,south,east,north in degrees. Default: whole grid.
    #[arg(long, allow_hyphen_values = true)]
    bbox: Option<BBox>,

    /// Output grid spacing in degrees for reduced Gaussian models (e.g. ecmwf_ifs O1280),
    /// which are resampled (nearest neighbour) onto a regular grid. Default 0.1.
    #[arg(long)]
    resolution: Option<f64>,

    /// Output NetCDF path.
    #[arg(short, long)]
    output: PathBuf,

    /// Number of step files fetched concurrently.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Allow reading a run that is still listed in in-progress.json.
    #[arg(long)]
    allow_incomplete: bool,

    /// Fill NaN (with a warning) when a step file lacks a variable, e.g. precipitation at step 0.
    #[arg(long)]
    missing_as_nan: bool,

    /// Combine the model's native output files so accumulated/mean/extreme variables
    /// (precipitation, radiation, temperature_2m_max, ...) cover the whole interval between
    /// consecutive requested steps instead of only the last native interval.
    #[arg(long)]
    accumulate: bool,

    /// Replace the output file if it exists.
    #[arg(long)]
    overwrite: bool,

    /// zlib level for data variables (0 = off).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(i32).range(0..=9))]
    deflate: i32,

    /// Read cache block size in KiB.
    #[arg(long, default_value_t = store::DEFAULT_BLOCK_SIZE / 1024)]
    block_kib: u64,

    /// Merge chunk reads separated by gaps smaller than this many KiB into one request.
    #[arg(long, default_value_t = 16)]
    merge_kib: u64,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let cli = Cli::parse();
    let level = if cli.quiet {
        log::LevelFilter::Warn
    } else {
        match cli.verbose {
            0 => log::LevelFilter::Info,
            1 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        }
    };
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .filter_module("om2nc", level)
        .format_timestamp_millis()
        .parse_default_env()
        .init();

    let cfg = StoreConfig {
        bucket: cli.store.bucket.clone(),
        region: cli.store.region.clone(),
        endpoint: cli.store.endpoint.clone(),
    };
    let store = Store::new(&cfg)?;

    match cli.command {
        Command::Fetch(args) => cmd_fetch(&store, &cfg, args, &argv).await,
        Command::Models => cmd_models(&store).await,
        Command::Info { model, json } => cmd_info(&store, &model, json).await,
        Command::Inspect { model, init, step } => cmd_inspect(&store, &model, &init, &step).await,
    }
}

async fn cmd_fetch(
    store: &Store,
    cfg: &StoreConfig,
    args: FetchArgs,
    argv: &[String],
) -> Result<()> {
    let init = parse_init(&args.init)?;
    let steps_minutes = match &args.step {
        Some(s) => Some(steps::parse_steps(s)?),
        None => None,
    };
    let mut vars: Vec<String> =
        args.vars.iter().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
    vars.dedup();
    let history = format!("{} om2nc {VERSION}: {}", iso(chrono::Utc::now()), shell_words(argv));
    let req = fetch::FetchRequest {
        model: args.model,
        init,
        steps_minutes,
        vars,
        bbox: args.bbox,
        resolution: args.resolution,
        output: args.output.clone(),
        concurrency: args.concurrency,
        allow_incomplete: args.allow_incomplete,
        missing_as_nan: args.missing_as_nan,
        accumulate: args.accumulate,
        overwrite: args.overwrite,
        deflate_level: args.deflate,
        block_size: args.block_kib.max(16) * 1024,
        io_merge: args.merge_kib * 1024,
        history,
        bucket: cfg.bucket.clone(),
    };
    let started = std::time::Instant::now();
    let summary = fetch::run(store, req).await?;
    log::info!(
        "wrote {} — run {}, {} steps x {} vars, {}x{} cells, {:.1} MiB remote, {:.1}s",
        args.output.display(),
        iso(summary.init),
        summary.steps,
        summary.vars,
        summary.ny,
        summary.nx,
        summary.bytes_remote as f64 / (1024.0 * 1024.0),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn cmd_models(store: &Store) -> Result<()> {
    let entries = store.list_dir(&format!("{SPATIAL_PREFIX}/")).await?;
    let mut names: Vec<&str> = entries
        .iter()
        .filter(|e| e.metadata().is_dir())
        .map(|e| e.name().trim_end_matches('/'))
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        bail!("no models found under {SPATIAL_PREFIX}/");
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

async fn cmd_info(store: &Store, model: &str, json: bool) -> Result<()> {
    if json {
        for file in ["latest.json", "in-progress.json"] {
            let key = format!("{SPATIAL_PREFIX}/{model}/{file}");
            if let Some(b) = store.read_opt(&key).await? {
                println!("// {key}");
                println!("{}", String::from_utf8_lossy(&b));
            }
        }
        return Ok(());
    }
    let status = ModelStatus::fetch(store, model).await?;
    for (label, meta) in [("latest", &status.latest), ("in-progress", &status.in_progress)] {
        let Some(m) = meta else {
            println!("{label}: (absent)");
            continue;
        };
        let steps = m.steps_minutes()?;
        let hours: Vec<String> = steps.iter().map(|s| steps::minutes_to_hours_string(*s)).collect();
        println!("{label}:");
        println!("  reference_time: {}", m.reference_time);
        println!("  completed:      {}", m.completed);
        if let Some(t) = &m.last_modified_time {
            println!("  last_modified:  {t}");
        }
        println!("  steps ({}):     {}", steps.len(), compact_steps(&hours));
        println!("  variables ({}):", m.variables.len());
        for chunk in m.variables.chunks(4) {
            println!("    {}", chunk.join(", "));
        }
        if let Some(wkt) = &m.crs_wkt {
            let first = wkt.lines().next().unwrap_or("").trim();
            println!("  crs:            {first}");
        }
    }
    Ok(())
}

async fn cmd_inspect(store: &Store, model: &str, init: &str, step: &str) -> Result<()> {
    let init = match parse_init(init)? {
        Some(t) => t,
        None => ModelStatus::fetch(store, model)
            .await?
            .latest
            .context("latest.json missing; pass --init")?
            .reference_time()?,
    };
    let steps = steps::parse_steps(step)?;
    if steps.len() != 1 {
        bail!("--step must be a single value for inspect");
    }
    let key = step_key(model, init, steps[0]);
    let file = om::open_required(store, &key, store::DEFAULT_BLOCK_SIZE).await?;
    file.print_tree().await?;
    log::debug!("run prefix {}", run_prefix(model, init));
    Ok(())
}

fn parse_init(s: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    if s.eq_ignore_ascii_case("latest") {
        return Ok(None);
    }
    parse_utc(s).map(Some)
}

fn compact_steps(hours: &[String]) -> String {
    if hours.len() <= 16 {
        hours.join(",")
    } else {
        format!("{} ... {}", hours[..8].join(","), hours[hours.len() - 4..].join(","))
    }
}

/// Re-quote argv for the `history` attribute.
fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c.is_whitespace() || "\"'$`\\".contains(c)) {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
