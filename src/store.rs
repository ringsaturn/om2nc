//! Object storage access (OpenDAL) and the `omfiles` async backend built on it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use omfiles::OmFilesError;
use omfiles::traits::OmFileReaderBackendAsync;
use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{ErrorKind, Operator};

pub const DEFAULT_BUCKET: &str = "openmeteo";
pub const DEFAULT_REGION: &str = "us-west-2";
pub const SPATIAL_PREFIX: &str = "data_spatial";

/// Connection settings for the Open-Meteo open-data bucket.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub bucket: String,
    pub region: String,
    /// Custom S3 endpoint (e.g. a mirror). `None` means AWS.
    pub endpoint: Option<String>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            bucket: DEFAULT_BUCKET.to_string(),
            region: DEFAULT_REGION.to_string(),
            endpoint: None,
        }
    }
}

/// Thin wrapper around an OpenDAL operator rooted at the bucket.
#[derive(Clone)]
pub struct Store {
    op: Operator,
}

impl Store {
    pub fn new(cfg: &StoreConfig) -> Result<Self> {
        // Installs the reqwest transport (idempotent; we do not use the ctor-based auto registration).
        opendal::install_default();
        let mut builder = opendal::services::S3::default()
            .bucket(&cfg.bucket)
            .region(&cfg.region)
            .skip_signature()
            .disable_config_load()
            .disable_ec2_metadata();
        if let Some(ep) = &cfg.endpoint {
            builder = builder.endpoint(ep);
        }
        let op = Operator::new(builder)
            .context("failed to build S3 operator")?
            .layer(
                RetryLayer::new()
                    .with_max_times(5)
                    .with_min_delay(Duration::from_millis(200))
                    .with_max_delay(Duration::from_secs(5))
                    .with_jitter(),
            )
            .layer(
                TimeoutLayer::new()
                    .with_timeout(Duration::from_secs(30))
                    .with_io_timeout(Duration::from_secs(30)),
            );
        Ok(Self { op })
    }

    /// Read a whole object as bytes. Returns `Ok(None)` when the key does not exist.
    pub async fn read_opt(&self, key: &str) -> Result<Option<Bytes>> {
        match self.op.read(key).await {
            Ok(buf) => Ok(Some(buf.to_bytes())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read {key}")),
        }
    }

    /// Object size in bytes, or `None` when the key does not exist.
    pub async fn size_opt(&self, key: &str) -> Result<Option<u64>> {
        match self.op.stat(key).await {
            Ok(meta) => Ok(Some(meta.content_length())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to stat {key}")),
        }
    }

    /// Non-recursive listing of a "directory" prefix (must end with '/').
    pub async fn list_dir(&self, prefix: &str) -> Result<Vec<opendal::Entry>> {
        self.op.list(prefix).await.with_context(|| format!("failed to list {prefix}"))
    }

    async fn read_range(&self, key: &str, offset: u64, count: u64) -> Result<Bytes> {
        log::trace!("GET {key} range {offset}+{count}");
        let buf = self
            .op
            .read_with(key)
            .range(offset..offset + count)
            .await
            .with_context(|| format!("range read failed: {key} @ {offset}+{count}"))?;
        let bytes = buf.to_bytes();
        anyhow::ensure!(
            bytes.len() as u64 == count,
            "short range read for {key} @ {offset}+{count}: got {} bytes",
            bytes.len()
        );
        Ok(bytes)
    }
}

/// Default block size for the read cache. Variable metadata of an .om file is
/// clustered near the trailer and chunk data of one variable is contiguous, so
/// fetching in fixed blocks turns many tiny reads into a few requests, while a
/// small block keeps over-read low for bbox subsets.
pub const DEFAULT_BLOCK_SIZE: u64 = 32 * 1024;

/// One cached block. The mutex is held by whoever is fetching it, so
/// concurrent readers of the same block wait instead of re-downloading.
type Block = Arc<tokio::sync::Mutex<Option<Bytes>>>;

/// `omfiles` async backend: one remote object, read through a block cache.
pub struct OmObjectBackend {
    store: Store,
    key: String,
    size: u64,
    block_size: u64,
    blocks: std::sync::Mutex<BTreeMap<u64, Block>>,
}

impl OmObjectBackend {
    pub async fn open(store: &Store, key: &str, block_size: u64) -> Result<Option<Arc<Self>>> {
        let Some(size) = store.size_opt(key).await? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(Self {
            store: store.clone(),
            key: key.to_string(),
            size,
            block_size,
            blocks: std::sync::Mutex::new(BTreeMap::new()),
        })))
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    fn block(&self, idx: u64) -> Block {
        let mut map = self.blocks.lock().expect("block cache poisoned");
        map.entry(idx).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))).clone()
    }

    /// Fetch blocks `a..=b` with one range request and store them into `guards`.
    async fn fetch_run(
        &self,
        a: u64,
        b: u64,
        guards: &mut [(u64, tokio::sync::OwnedMutexGuard<Option<Bytes>>)],
    ) -> Result<()> {
        let offset = a * self.block_size;
        let end = ((b + 1) * self.block_size).min(self.size);
        let data = self.store.read_range(&self.key, offset, end - offset).await?;
        for (blk, guard) in guards.iter_mut() {
            let s = ((*blk - a) * self.block_size) as usize;
            let e = (s as u64 + self.block_size).min(data.len() as u64) as usize;
            **guard = Some(data.slice(s..e));
        }
        Ok(())
    }

    /// Ensure blocks `first..=last` are cached. Blocks nobody is fetching yet
    /// are claimed and fetched in contiguous runs; blocks another task is
    /// fetching are awaited.
    async fn ensure_blocks(&self, first: u64, last: u64) -> Result<()> {
        let mut owned: Vec<(u64, tokio::sync::OwnedMutexGuard<Option<Bytes>>)> = Vec::new();
        let mut pending: Vec<(u64, Block)> = Vec::new();
        for idx in first..=last {
            let cell = self.block(idx);
            match cell.clone().try_lock_owned() {
                Ok(guard) if guard.is_some() => {}
                Ok(guard) => owned.push((idx, guard)),
                Err(_) => pending.push((idx, cell)),
            }
        }
        // Contiguous runs of claimed blocks -> one request each.
        let mut i = 0;
        while i < owned.len() {
            let mut j = i;
            while j + 1 < owned.len() && owned[j + 1].0 == owned[j].0 + 1 {
                j += 1;
            }
            let (a, b) = (owned[i].0, owned[j].0);
            self.fetch_run(a, b, &mut owned[i..=j]).await?;
            i = j + 1;
        }
        drop(owned);
        for (idx, cell) in pending {
            let guard = cell.lock_owned().await;
            if guard.is_none() {
                // The other fetch failed; try ourselves.
                let mut one = vec![(idx, guard)];
                self.fetch_run(idx, idx, &mut one).await?;
            } else {
                drop(guard);
            }
        }
        Ok(())
    }

    async fn read(&self, offset: u64, count: u64) -> Result<Bytes> {
        if count == 0 {
            return Ok(Bytes::new());
        }
        anyhow::ensure!(
            offset + count <= self.size,
            "read beyond end of {}: {offset}+{count} > {}",
            self.key,
            self.size
        );
        let first = offset / self.block_size;
        let last = (offset + count - 1) / self.block_size;
        self.ensure_blocks(first, last).await?;
        let mut out = BytesMut::new();
        for idx in first..=last {
            let cell = self.block(idx);
            let guard = cell.lock().await;
            let data = guard.as_ref().context("block cache inconsistency")?;
            let blk_start = idx * self.block_size;
            let s = offset.saturating_sub(blk_start) as usize;
            let e = ((offset + count) - blk_start).min(data.len() as u64) as usize;
            if first == last {
                return Ok(data.slice(s..e));
            }
            out.extend_from_slice(&data[s..e]);
        }
        Ok(out.freeze())
    }
}

impl OmFileReaderBackendAsync for OmObjectBackend {
    type Bytes = Bytes;

    fn count_async(&self) -> usize {
        self.size as usize
    }

    async fn get_bytes_async(&self, offset: u64, count: u64) -> Result<Bytes, OmFilesError> {
        self.read(offset, count).await.map_err(|e| OmFilesError::GenericError(format!("{e:#}")))
    }
}
