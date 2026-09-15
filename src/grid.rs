//! Regular latitude/longitude grid handling.
//!
//! Open-Meteo spatial files do not store coordinate arrays. The grid is
//! reconstructed from the `crs_wkt` scalar (its `BBOX[south,west,north,east]`
//! gives the extent of the cell centres) and the array shape `(ny, nx)`.
//! Row 0 is the southernmost latitude, column 0 the westernmost longitude.

use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;

use crate::gaussian::{GaussianError, OctahedralGrid};

#[derive(Debug, Error)]
pub enum GridError {
    #[error(
        "crs_wkt is not a geographic CRS (starts with {0:?}); projected grids are not supported"
    )]
    NotGeographic(String),
    #[error("grid is a rotated lat/lon grid (DERIVINGCONVERSION in crs_wkt); not supported")]
    Rotated,
    #[error(
        "grid is neither a regular lat/lon grid nor an octahedral reduced Gaussian grid (shape {0:?})"
    )]
    NotRegular(Vec<u64>),
    #[error(transparent)]
    Gaussian(#[from] GaussianError),
    #[error("--resolution must be > 0 and <= 10 degrees, got {0}")]
    BadResolution(f64),
    #[error("--resolution only applies to reduced Gaussian models; {0} is already a regular grid")]
    ResolutionNotApplicable(String),
    #[error("crs_wkt has no parsable BBOX[...]: {0}")]
    NoBbox(String),
    #[error("invalid bbox {0:?}: expected west,south,east,north in degrees")]
    BadBbox(String),
    #[error(
        "bbox does not intersect the model grid (grid covers lat {0:.3}..{1:.3}, lon {2:.3}..{3:.3})"
    )]
    NoOverlap(f64, f64, f64, f64),
    #[error("bbox crosses the antimeridian (west > east after normalisation); not supported")]
    Antimeridian,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegularGrid {
    pub ny: usize,
    pub nx: usize,
    pub lat0: f64,
    pub lon0: f64,
    pub dlat: f64,
    pub dlon: f64,
}

impl RegularGrid {
    pub fn from_crs_wkt(wkt: &str, dims: &[u64]) -> Result<Self, GridError> {
        let kind: String = wkt
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if kind != "GEOGCRS" && kind != "GEOGCS" {
            return Err(GridError::NotGeographic(kind));
        }
        if wkt.contains("DERIVINGCONVERSION") {
            return Err(GridError::Rotated);
        }
        if dims.len() != 2 || dims[0] < 2 || dims[1] < 2 {
            return Err(GridError::NotRegular(dims.to_vec()));
        }
        let bbox = parse_wkt_bbox(wkt).ok_or_else(|| GridError::NoBbox(wkt.to_string()))?;
        let (south, west, north, east) = bbox;
        let ny = dims[0] as usize;
        let nx = dims[1] as usize;
        Ok(Self {
            ny,
            nx,
            lat0: south,
            lon0: west,
            dlat: (north - south) / (ny as f64 - 1.0),
            dlon: (east - west) / (nx as f64 - 1.0),
        })
    }

    pub fn lat(&self, i: usize) -> f64 {
        self.lat0 + self.dlat * i as f64
    }

    pub fn lon(&self, j: usize) -> f64 {
        self.lon0 + self.dlon * j as f64
    }

    pub fn lat_max(&self) -> f64 {
        self.lat(self.ny - 1)
    }

    pub fn lon_max(&self) -> f64 {
        self.lon(self.nx - 1)
    }

    /// Cell-index window covering `bbox` (or the whole grid when `None`).
    pub fn subset(&self, bbox: Option<&BBox>) -> Result<Subset, GridError> {
        let (rows, cols) = match bbox {
            None => (0..self.ny, 0..self.nx),
            Some(b) => {
                let west = normalise_lon(b.west);
                let east = normalise_lon(b.east);
                if west > east {
                    return Err(GridError::Antimeridian);
                }
                let rows = index_window(self.lat0, self.dlat, self.ny, b.south, b.north);
                let cols = index_window(self.lon0, self.dlon, self.nx, west, east);
                match (rows, cols) {
                    (Some(r), Some(c)) => (r, c),
                    _ => {
                        return Err(GridError::NoOverlap(
                            self.lat0,
                            self.lat_max(),
                            self.lon0,
                            self.lon_max(),
                        ));
                    }
                }
            }
        };
        Ok(Subset {
            lats: rows.clone().map(|i| self.lat(i)).collect(),
            lons: cols.clone().map(|j| self.lon(j)).collect(),
            sampler: Sampler::Crop2d { rows, cols },
        })
    }
}

/// Where the data comes from and how it maps onto the output lat/lon grid.
pub enum SourceGrid {
    Regular(RegularGrid),
    ReducedGaussian(OctahedralGrid),
}

impl SourceGrid {
    pub fn detect(wkt: &str, dims: &[u64]) -> Result<Self, GridError> {
        if dims.len() == 2 && dims[0] == 1 {
            return Ok(Self::ReducedGaussian(OctahedralGrid::from_crs_wkt(wkt, dims)?));
        }
        RegularGrid::from_crs_wkt(wkt, dims).map(Self::Regular)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Regular(g) => {
                format!("regular {}x{} ({:.4}° x {:.4}°)", g.ny, g.nx, g.dlat, g.dlon)
            }
            Self::ReducedGaussian(g) => {
                format!("octahedral reduced Gaussian O{} ({} points)", g.n, g.total())
            }
        }
    }

    /// Build the output grid and the sampling plan for it.
    ///
    /// `resolution` (degrees) is required for reduced Gaussian sources and
    /// rejected for regular ones (those are cropped, never resampled).
    pub fn plan(
        &self,
        bbox: Option<&BBox>,
        resolution: Option<f64>,
        model: &str,
    ) -> Result<Subset, GridError> {
        match self {
            Self::Regular(g) => {
                if resolution.is_some() {
                    return Err(GridError::ResolutionNotApplicable(model.to_string()));
                }
                g.subset(bbox)
            }
            Self::ReducedGaussian(g) => {
                let res = resolution.unwrap_or(DEFAULT_REDUCED_RESOLUTION);
                if !(res > 0.0 && res <= 10.0) {
                    return Err(GridError::BadResolution(res));
                }
                let (lats, lons) = target_axes(bbox, res)?;
                let (window, map) = g.nearest_plan(&lats, &lons);
                Ok(Subset {
                    lats,
                    lons,
                    sampler: Sampler::Nearest1d { window, map: Arc::new(map) },
                })
            }
        }
    }
}

/// Output spacing used for reduced Gaussian grids when `--resolution` is not given.
/// O1280 spacing is ~0.07° at the equator, so 0.1° slightly under-samples.
pub const DEFAULT_REDUCED_RESOLUTION: f64 = 0.1;

/// Regular target axes at `res` degrees: bbox snapped inwards to multiples of
/// `res`; the whole globe when there is no bbox.
fn target_axes(bbox: Option<&BBox>, res: f64) -> Result<(Vec<f64>, Vec<f64>), GridError> {
    const EPS: f64 = 1e-9;
    let axis = |lo: f64, hi: f64| -> Vec<f64> {
        let i0 = (lo / res - EPS).ceil() as i64;
        let i1 = (hi / res + EPS).floor() as i64;
        (i0..=i1).map(|i| i as f64 * res).collect()
    };
    match bbox {
        None => Ok((axis(-90.0, 90.0), axis(-180.0, 180.0 - res))),
        Some(b) => {
            let west = normalise_lon(b.west);
            let east = normalise_lon(b.east);
            if west > east {
                return Err(GridError::Antimeridian);
            }
            let lats = axis(b.south, b.north);
            let lons = axis(west, east);
            if lats.is_empty() || lons.is_empty() {
                return Err(GridError::BadBbox(format!("{b} is narrower than resolution {res}")));
            }
            Ok((lats, lons))
        }
    }
}

/// Inclusive index range of grid points with coordinate in `[lo, hi]`.
fn index_window(origin: f64, step: f64, n: usize, lo: f64, hi: f64) -> Option<Range<usize>> {
    const EPS: f64 = 1e-6;
    let start = ((lo - origin) / step - EPS).ceil().max(0.0);
    let end = ((hi - origin) / step + EPS).floor().min(n as f64 - 1.0);
    if start > end {
        return None;
    }
    Some(start as usize..end as usize + 1)
}

/// Map longitudes given in 0..360 convention onto -180..180.
fn normalise_lon(lon: f64) -> f64 {
    if lon > 180.0 { lon - 360.0 } else { lon }
}

/// Parse `BBOX[south,west,north,east]` out of a WKT2 string.
fn parse_wkt_bbox(wkt: &str) -> Option<(f64, f64, f64, f64)> {
    let start = wkt.find("BBOX[")? + "BBOX[".len();
    let end = start + wkt[start..].find(']')?;
    let vals: Vec<f64> = wkt[start..end]
        .split(',')
        .map(|s| s.trim().parse::<f64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if vals.len() != 4 {
        return None;
    }
    Some((vals[0], vals[1], vals[2], vals[3]))
}

/// Geographic bounding box, `west,south,east,north` in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl std::str::FromStr for BBox {
    type Err = GridError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || GridError::BadBbox(s.to_string());
        let v: Vec<f64> = s
            .split(',')
            .map(|p| p.trim().parse::<f64>().map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        if v.len() != 4 {
            return Err(bad());
        }
        let b = BBox { west: v[0], south: v[1], east: v[2], north: v[3] };
        if !(-90.0..=90.0).contains(&b.south)
            || !(-90.0..=90.0).contains(&b.north)
            || b.south > b.north
            || !(-180.0..=360.0).contains(&b.west)
            || !(-180.0..=360.0).contains(&b.east)
        {
            return Err(bad());
        }
        Ok(b)
    }
}

impl std::fmt::Display for BBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{},{}", self.west, self.south, self.east, self.north)
    }
}

/// Output coordinates plus how to obtain each output slab from a step file.
#[derive(Debug, Clone, PartialEq)]
pub struct Subset {
    pub lats: Vec<f64>,
    pub lons: Vec<f64>,
    pub sampler: Sampler,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sampler {
    /// Crop a 2-D `(ny, nx)` array: rows are latitudes south → north.
    Crop2d { rows: Range<usize>, cols: Range<usize> },
    /// Read `window` of a 1-D array and pick `map[k]` for output cell `k`.
    Nearest1d { window: Range<u64>, map: Arc<Vec<u32>> },
}

impl Sampler {
    pub fn describe(&self) -> String {
        match self {
            Self::Crop2d { rows, cols } => format!("crop rows {rows:?} cols {cols:?}"),
            Self::Nearest1d { window, .. } => {
                format!("nearest-neighbour from {} source points", window.end - window.start)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFS025: &str = r#"GEOGCRS["WGS 84",
    DATUM["World Geodetic System 1984",
        ELLIPSOID["WGS 84",6378137,298.257223563]],
    CS[ellipsoidal,2],
        AXIS["latitude",north],
        AXIS["longitude",east],
        ANGLEUNIT["degree",0.0174532925199433]
    USAGE[
        SCOPE["grid"],
        BBOX[-90.0,-180.0,90.0,179.75]]]"#;

    #[test]
    fn ifs025_grid() {
        let g = RegularGrid::from_crs_wkt(IFS025, &[721, 1440]).unwrap();
        assert!((g.dlat - 0.25).abs() < 1e-9);
        assert!((g.dlon - 0.25).abs() < 1e-9);
        assert_eq!(g.lat(0), -90.0);
        assert!((g.lon(1439) - 179.75).abs() < 1e-9);
    }

    #[test]
    fn subset_east_asia() {
        let g = RegularGrid::from_crs_wkt(IFS025, &[721, 1440]).unwrap();
        let b: BBox = "120,20,150,50".parse().unwrap();
        let s = g.subset(Some(&b)).unwrap();
        assert_eq!(s.sampler, Sampler::Crop2d { rows: 440..561, cols: 1200..1321 });
        assert_eq!(s.lats.first().copied(), Some(20.0));
        assert_eq!(s.lats.last().copied(), Some(50.0));
        assert_eq!(s.lons.first().copied(), Some(120.0));
        assert_eq!(s.lons.last().copied(), Some(150.0));
    }

    #[test]
    fn subset_whole_grid() {
        let g = RegularGrid::from_crs_wkt(IFS025, &[721, 1440]).unwrap();
        let s = g.subset(None).unwrap();
        assert_eq!(s.sampler, Sampler::Crop2d { rows: 0..721, cols: 0..1440 });
    }

    #[test]
    fn subset_lon_0_360() {
        let g = RegularGrid::from_crs_wkt(IFS025, &[721, 1440]).unwrap();
        let b: BBox = "200,0,210,10".parse().unwrap();
        let s = g.subset(Some(&b)).unwrap();
        assert_eq!(s.lons.first().copied(), Some(-160.0));
    }

    #[test]
    fn subset_no_overlap() {
        let wkt = r#"GEOGCRS["WGS 84",USAGE[SCOPE["grid"],BBOX[22.4,120.0,47.6,150.0]]]"#;
        let g = RegularGrid::from_crs_wkt(wkt, &[505, 481]).unwrap();
        let b: BBox = "-10,-10,0,0".parse().unwrap();
        assert!(matches!(g.subset(Some(&b)), Err(GridError::NoOverlap(..))));
        // Partially overlapping boxes are clipped to the grid.
        let b: BBox = "100,40,125,60".parse().unwrap();
        let s = g.subset(Some(&b)).unwrap();
        assert!(matches!(&s.sampler, Sampler::Crop2d { cols, .. } if cols.start == 0));
        assert!((s.lats.last().unwrap() - 47.6).abs() < 1e-9);
    }

    #[test]
    fn rejects_unsupported_grids() {
        let rot = r#"GEOGCRS["Rotated Lat/Lon",DERIVINGCONVERSION["x"],USAGE[BBOX[1,2,3,4]]]"#;
        assert!(matches!(RegularGrid::from_crs_wkt(rot, &[10, 10]), Err(GridError::Rotated)));
        let proj = r#"PROJCRS["Lambert",USAGE[BBOX[1,2,3,4]]]"#;
        assert!(matches!(
            RegularGrid::from_crs_wkt(proj, &[10, 10]),
            Err(GridError::NotGeographic(_))
        ));
        assert!(matches!(
            RegularGrid::from_crs_wkt(IFS025, &[1, 6599680]),
            Err(GridError::NotRegular(_))
        ));
    }

    #[test]
    fn bbox_parse_errors() {
        assert!("1,2,3".parse::<BBox>().is_err());
        assert!("0,50,10,40".parse::<BBox>().is_err());
        assert!("a,b,c,d".parse::<BBox>().is_err());
    }
}
