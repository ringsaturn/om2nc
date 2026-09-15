//! CF-1.10 NetCDF-4 output.
//!
//! Layout: dimensions `time`, `latitude`, `longitude`; coordinate variables
//! `time` (hours since init), `step` (hours), scalar `forecast_reference_time`,
//! a `crs` grid-mapping variable; data variables are `float32` with
//! `(time, latitude, longitude)` chunks of `(1, ny, nx)` and zlib compression.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::cf::{self, Reduction};
use crate::timefmt::iso;

pub struct VarSpec {
    pub name: String,
    /// Unit string as stored in the .om file.
    pub om_units: String,
    /// Interval semantics (`None` = instantaneous).
    pub reduction: Option<Reduction>,
}

pub struct NcSpec<'a> {
    pub model: &'a str,
    pub init: DateTime<Utc>,
    pub steps_minutes: &'a [i64],
    /// Per output step, the interval `[start, end]` in minutes since init that
    /// interval variables (precipitation, radiation, ...) cover. `None` when
    /// the model's native output interval is unknown.
    pub interval_minutes: Option<&'a [(i64, i64)]>,
    /// Free-text description of the interval semantics, written as `comment`.
    pub interval_comment: &'a str,
    pub lats: &'a [f64],
    pub lons: &'a [f64],
    pub vars: &'a [VarSpec],
    pub crs_wkt: &'a str,
    pub history: &'a str,
    pub source: &'a str,
    /// Set when the data was resampled from a non-regular source grid.
    pub source_grid: Option<&'a str>,
    pub regrid_method: Option<&'a str>,
    pub deflate_level: i32,
}

pub struct NcWriter {
    file: netcdf::FileMut,
    ny: usize,
    nx: usize,
}

impl NcWriter {
    pub fn create(path: &Path, spec: &NcSpec<'_>) -> Result<Self> {
        let mut file =
            netcdf::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        let nt = spec.steps_minutes.len();
        let ny = spec.lats.len();
        let nx = spec.lons.len();

        file.add_attribute("Conventions", "CF-1.10")?;
        file.add_attribute("title", format!("Open-Meteo {} forecast", spec.model))?;
        file.add_attribute("source", spec.source)?;
        file.add_attribute("history", spec.history)?;
        file.add_attribute("license", "CC BY 4.0")?;
        file.add_attribute("attribution", cf::attribution(spec.model))?;
        file.add_attribute("references", "https://open-meteo.com/en/docs/open-data")?;
        file.add_attribute("model", spec.model)?;
        file.add_attribute("forecast_reference_time", iso(spec.init))?;
        if let Some(g) = spec.source_grid {
            file.add_attribute("source_grid", g)?;
        }
        if let Some(m) = spec.regrid_method {
            file.add_attribute("regrid_method", m)?;
        }

        file.add_dimension("time", nt)?;
        file.add_dimension("latitude", ny)?;
        file.add_dimension("longitude", nx)?;
        if spec.interval_minutes.is_some() {
            file.add_dimension("nv", 2)?;
        }

        let time_units = format!("hours since {}", spec.init.format("%Y-%m-%d %H:%M:%S"));
        let hours: Vec<f64> = spec.steps_minutes.iter().map(|m| *m as f64 / 60.0).collect();
        {
            let mut v = file.add_variable::<f64>("time", &["time"])?;
            v.put_attribute("standard_name", "time")?;
            v.put_attribute("long_name", "valid time")?;
            v.put_attribute("units", time_units.as_str())?;
            v.put_attribute("calendar", "standard")?;
            v.put_attribute("axis", "T")?;
            if spec.interval_minutes.is_some() {
                v.put_attribute("bounds", "time_bnds")?;
            }
            v.put_values(&hours, ..)?;
        }
        if let Some(iv) = spec.interval_minutes {
            anyhow::ensure!(iv.len() == nt, "interval list length mismatch");
            let bnds: Vec<f64> =
                iv.iter().flat_map(|(a, b)| [*a as f64 / 60.0, *b as f64 / 60.0]).collect();
            let mut v = file.add_variable::<f64>("time_bnds", &["time", "nv"])?;
            v.put_attribute(
                "long_name",
                "interval covered by accumulated/mean/extreme variables (see cell_methods)",
            )?;
            v.put_attribute("units", time_units.as_str())?;
            v.put_values(&bnds, ..)?;
        }
        {
            let mut v = file.add_variable::<f64>("step", &["time"])?;
            v.put_attribute("standard_name", "forecast_period")?;
            v.put_attribute("long_name", "time since forecast_reference_time")?;
            v.put_attribute("units", "hours")?;
            v.put_values(&hours, ..)?;
        }
        {
            let mut v = file.add_variable::<f64>("forecast_reference_time", &[])?;
            v.put_attribute("standard_name", "forecast_reference_time")?;
            v.put_attribute("long_name", "initial time of forecast")?;
            v.put_attribute("units", "seconds since 1970-01-01 00:00:00")?;
            v.put_attribute("calendar", "standard")?;
            v.put_value(spec.init.timestamp() as f64, ())?;
        }
        {
            let mut v = file.add_variable::<f64>("latitude", &["latitude"])?;
            v.put_attribute("standard_name", "latitude")?;
            v.put_attribute("long_name", "latitude")?;
            v.put_attribute("units", "degrees_north")?;
            v.put_attribute("axis", "Y")?;
            v.put_values(spec.lats, ..)?;
        }
        {
            let mut v = file.add_variable::<f64>("longitude", &["longitude"])?;
            v.put_attribute("standard_name", "longitude")?;
            v.put_attribute("long_name", "longitude")?;
            v.put_attribute("units", "degrees_east")?;
            v.put_attribute("axis", "X")?;
            v.put_values(spec.lons, ..)?;
        }
        {
            let mut v = file.add_variable::<i32>("crs", &[])?;
            v.put_attribute("grid_mapping_name", "latitude_longitude")?;
            v.put_attribute("crs_wkt", spec.crs_wkt)?;
            v.put_value(0i32, ())?;
        }

        for var in spec.vars {
            let mut v = file
                .add_variable::<f32>(&var.name, &["time", "latitude", "longitude"])
                .with_context(|| format!("cannot define variable {}", var.name))?;
            v.set_chunking(&[1, ny, nx])?;
            v.set_compression(spec.deflate_level, true)?;
            v.set_fill_value(f32::NAN)?;
            v.put_attribute("long_name", cf::long_name(&var.name))?;
            if let Some(sn) = cf::standard_name(&var.name) {
                v.put_attribute("standard_name", sn)?;
            }
            let units = cf::cf_units(&var.om_units);
            v.put_attribute("units", units)?;
            if units != var.om_units {
                v.put_attribute("open_meteo_units", var.om_units.as_str())?;
            }
            v.put_attribute("open_meteo_variable", var.name.as_str())?;
            v.put_attribute("grid_mapping", "crs")?;
            v.put_attribute("coordinates", "forecast_reference_time step")?;
            if let Some(r) = var.reduction {
                v.put_attribute("cell_methods", r.cell_methods())?;
                v.put_attribute("comment", spec.interval_comment)?;
            }
        }

        Ok(Self { file, ny, nx })
    }

    /// Write one `(latitude, longitude)` slab (row-major, south to north) at time index `t`.
    pub fn write_slab(&mut self, var: &str, t: usize, data: &[f32]) -> Result<()> {
        anyhow::ensure!(
            data.len() == self.ny * self.nx,
            "slab for {var} has {} values, expected {}",
            data.len(),
            self.ny * self.nx
        );
        let mut v = self
            .file
            .variable_mut(var)
            .with_context(|| format!("variable {var} missing from output"))?;
        v.put_values(data, [t..t + 1, 0..self.ny, 0..self.nx])
            .with_context(|| format!("failed writing {var} at time index {t}"))?;
        Ok(())
    }

    pub fn close(self) -> Result<()> {
        self.file.close().context("failed to close NetCDF file")
    }
}
