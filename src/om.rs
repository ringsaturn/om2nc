//! Reading one Open-Meteo spatial `.om` step file.
//!
//! Tree layout (as observed on the `openmeteo` bucket):
//! root
//! ├── crs_wkt: String
//! ├── forecast_reference_time / valid_time / created_at: Int64 (unix seconds)
//! ├── coordinates: String ("lat lon")
//! └── <variable>: FloatArray (ny, nx), chunks (32, 32)
//!     └── unit: String

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omfiles::OmDataType;
use omfiles::reader_async::OmFileReaderAsync;
use omfiles::traits::{OmArrayVariable, OmFileAsyncReadable, OmFileVariable, OmScalarVariable};
use std::collections::BTreeMap;

use crate::store::{OmObjectBackend, Store};

type Reader = OmFileReaderAsync<OmObjectBackend>;

pub struct StepFile {
    pub key: String,
    pub size: u64,
    /// Direct children by name (variables and scalar metadata).
    children: BTreeMap<String, Reader>,
}

pub struct VarInfo {
    pub dims: Vec<u64>,
    pub units: String,
}

impl StepFile {
    /// Open a step file. Returns `Ok(None)` when the object does not exist.
    pub async fn open(store: &Store, key: &str, block_size: u64) -> Result<Option<Self>> {
        let Some(backend) = OmObjectBackend::open(store, key, block_size).await? else {
            return Ok(None);
        };
        let size = backend.size();
        let root = Reader::new(backend)
            .await
            .map_err(|e| anyhow!("{key}: not a readable .om file: {e}"))?;
        let mut children = BTreeMap::new();
        for i in 0..root.number_of_children() {
            if let Some(child) = root.get_child_by_index(i).await {
                children.insert(child.name().to_string(), child);
            }
        }
        Ok(Some(Self { key: key.to_string(), size, children }))
    }

    /// Names of array (data) variables.
    pub fn variable_names(&self) -> Vec<&str> {
        self.children
            .iter()
            .filter(|(_, r)| r.data_type().is_array())
            .map(|(n, _)| n.as_str())
            .collect()
    }

    pub fn has_variable(&self, name: &str) -> bool {
        self.children.get(name).is_some_and(|r| r.data_type().is_array())
    }

    pub fn scalar_string(&self, name: &str) -> Option<String> {
        let r = self.children.get(name)?;
        if r.data_type() != OmDataType::String {
            return None;
        }
        r.expect_scalar().ok()?.read_scalar::<String>()
    }

    pub fn scalar_i64(&self, name: &str) -> Option<i64> {
        let r = self.children.get(name)?;
        let s = r.expect_scalar().ok()?;
        match r.data_type() {
            OmDataType::Int64 => s.read_scalar::<i64>(),
            OmDataType::Int32 => s.read_scalar::<i32>().map(i64::from),
            OmDataType::Uint64 => s.read_scalar::<u64>().map(|v| v as i64),
            OmDataType::Uint32 => s.read_scalar::<u32>().map(i64::from),
            OmDataType::Double => s.read_scalar::<f64>().map(|v| v as i64),
            _ => None,
        }
    }

    pub fn scalar_time(&self, name: &str) -> Option<DateTime<Utc>> {
        DateTime::<Utc>::from_timestamp(self.scalar_i64(name)?, 0)
    }

    pub fn crs_wkt(&self) -> Result<String> {
        self.scalar_string("crs_wkt")
            .with_context(|| format!("{}: missing crs_wkt scalar", self.key))
    }

    fn variable(&self, name: &str) -> Result<&Reader> {
        let r = self
            .children
            .get(name)
            .with_context(|| format!("{}: variable {name:?} not present", self.key))?;
        if !r.data_type().is_array() {
            bail!("{}: {name:?} is not an array variable", self.key);
        }
        Ok(r)
    }

    pub async fn var_info(&self, name: &str) -> Result<VarInfo> {
        let r = self.variable(name)?;
        let dims = r.expect_array()?.get_dimensions().to_vec();
        let units = match r.get_child_by_name("unit").await {
            Some(u) if u.data_type() == OmDataType::String => {
                u.expect_scalar()?.read_scalar::<String>().unwrap_or_default()
            }
            _ => String::new(),
        };
        Ok(VarInfo { dims, units })
    }

    /// Read `rows × cols` of a 2-D variable as row-major f32.
    pub async fn read_subset(
        &self,
        name: &str,
        rows: std::ops::Range<usize>,
        cols: std::ops::Range<usize>,
        io_merge: u64,
    ) -> Result<Vec<f32>> {
        let r = self.variable(name)?;
        // Large merge threshold: adjacent chunk reads become one request,
        // which is what we want over HTTP.
        let arr = r.expect_array_with_io_sizes(16 * 1024 * 1024, io_merge)?;
        let dims = arr.get_dimensions();
        if dims.len() != 2 {
            bail!("{}: {name} has {} dimensions, expected 2", self.key, dims.len());
        }
        let data = arr
            .read::<f32>(&[rows.start as u64..rows.end as u64, cols.start as u64..cols.end as u64])
            .await
            .with_context(|| format!("{}: reading {name} failed", self.key))?;
        Ok(data.into_raw_vec_and_offset().0)
    }

    /// Read `window` of a 1-D variable stored as `(1, n)` (reduced Gaussian grids).
    pub async fn read_window_1d(
        &self,
        name: &str,
        window: std::ops::Range<u64>,
        io_merge: u64,
    ) -> Result<Vec<f32>> {
        let r = self.variable(name)?;
        let arr = r.expect_array_with_io_sizes(16 * 1024 * 1024, io_merge)?;
        let dims = arr.get_dimensions();
        if dims.len() != 2 || dims[0] != 1 {
            bail!("{}: {name} has shape {dims:?}, expected (1, n)", self.key);
        }
        let data = arr
            .read::<f32>(&[0..1, window])
            .await
            .with_context(|| format!("{}: reading {name} failed", self.key))?;
        Ok(data.into_raw_vec_and_offset().0)
    }

    /// Recursively print the variable tree (for `om2nc inspect`).
    pub async fn print_tree(&self) -> Result<()> {
        println!("{} ({} bytes)", self.key, self.size);
        for (name, child) in &self.children {
            print_node(child, name, 1).await?;
        }
        Ok(())
    }
}

async fn print_node(r: &Reader, name: &str, depth: usize) -> Result<()> {
    let pad = "  ".repeat(depth);
    let dt = r.data_type();
    if dt.is_array() {
        let a = r.expect_array()?;
        println!(
            "{pad}{name}: {dt:?} dims={:?} chunks={:?} compression={:?} scale_factor={} add_offset={}",
            a.get_dimensions(),
            a.get_chunk_dimensions(),
            a.compression(),
            a.scale_factor(),
            a.add_offset()
        );
    } else if dt.is_scalar() {
        let s = r.expect_scalar()?;
        let val = match dt {
            OmDataType::String => s.read_scalar::<String>().map(|v| format!("{v:?}")),
            OmDataType::Int64 => s.read_scalar::<i64>().map(|v| v.to_string()),
            OmDataType::Int32 => s.read_scalar::<i32>().map(|v| v.to_string()),
            OmDataType::Double => s.read_scalar::<f64>().map(|v| v.to_string()),
            OmDataType::Float => s.read_scalar::<f32>().map(|v| v.to_string()),
            _ => None,
        };
        println!("{pad}{name}: {dt:?} = {}", val.unwrap_or_else(|| "<unreadable>".into()));
    } else {
        println!("{pad}{name}: group");
    }
    for i in 0..r.number_of_children() {
        if let Some(c) = r.get_child_by_index(i).await {
            Box::pin(print_node(&c, c.name(), depth + 1)).await?;
        }
    }
    Ok(())
}

/// Convenience: open or fail with a "missing" error.
pub async fn open_required(store: &Store, key: &str, block_size: u64) -> Result<StepFile> {
    StepFile::open(store, key, block_size)
        .await?
        .with_context(|| format!("object not found: {key}"))
}
