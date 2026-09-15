//! ECMWF octahedral reduced Gaussian grids (O<N>, e.g. O1280 for `ecmwf_ifs`).
//!
//! Layout as stored in Open-Meteo `.om` files (verified against the API):
//! a single 1-D array, rows ordered north → south, row `i` (1-based from the
//! pole) holding `16 + 4i` points at longitudes `360·j/n_row`, `j = 0..n_row`,
//! i.e. starting at 0° and going east. Latitudes are the Gaussian latitudes
//! (roots of the Legendre polynomial of degree 2N), not equidistant.

use std::ops::Range;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GaussianError {
    #[error("{0} points do not form an octahedral reduced Gaussian grid")]
    NotOctahedral(u64),
    #[error("crs_wkt says O{0} but the array has {1} points (expected {2})")]
    Mismatch(u32, u64, u64),
}

#[derive(Debug, Clone)]
pub struct OctahedralGrid {
    pub n: u32,
    /// Row latitudes in degrees, north → south.
    pub lats: Vec<f64>,
    /// Points per row.
    pub row_len: Vec<u32>,
    /// Start index of each row in the 1-D array, plus a final total.
    pub row_off: Vec<u64>,
}

impl OctahedralGrid {
    pub fn total_points(n: u32) -> u64 {
        let n = n as u64;
        4 * n * n + 36 * n
    }

    /// Infer N from the point count (4N² + 36N = total).
    pub fn n_from_total(total: u64) -> Option<u32> {
        let disc = (36f64 * 36.0 + 16.0 * total as f64).sqrt();
        let n = ((-36.0 + disc) / 8.0).round();
        (n >= 1.0 && Self::total_points(n as u32) == total).then_some(n as u32)
    }

    pub fn new(n: u32) -> Self {
        let mut row_len: Vec<u32> = (1..=n).map(|i| 16 + 4 * i).collect();
        let south: Vec<u32> = row_len.iter().rev().copied().collect();
        row_len.extend(south);
        let mut row_off = Vec::with_capacity(row_len.len() + 1);
        let mut acc = 0u64;
        for &l in &row_len {
            row_off.push(acc);
            acc += l as u64;
        }
        row_off.push(acc);
        let north = gaussian_latitudes(n);
        let mut lats = north.clone();
        lats.extend(north.iter().rev().map(|l| -l));
        Self { n, lats, row_len, row_off }
    }

    /// Build from `crs_wkt` (REMARK "... O1280 ...") and the array shape.
    pub fn from_crs_wkt(wkt: &str, dims: &[u64]) -> Result<Self, GaussianError> {
        let total: u64 = dims.iter().product();
        let n = Self::n_from_total(total).ok_or(GaussianError::NotOctahedral(total))?;
        if let Some(claimed) = parse_octahedral_n(wkt)
            && claimed != n
        {
            return Err(GaussianError::Mismatch(claimed, total, Self::total_points(claimed)));
        }
        Ok(Self::new(n))
    }

    pub fn total(&self) -> u64 {
        *self.row_off.last().unwrap()
    }

    /// Row whose latitude is closest to `lat`.
    pub fn nearest_row(&self, lat: f64) -> usize {
        // lats are descending; find first row with lat <= target.
        let i = self.lats.partition_point(|&l| l > lat);
        if i == 0 {
            return 0;
        }
        if i >= self.lats.len() {
            return self.lats.len() - 1;
        }
        if (self.lats[i - 1] - lat).abs() <= (lat - self.lats[i]).abs() { i - 1 } else { i }
    }

    /// Index within `row` of the point closest to `lon` (any longitude convention).
    pub fn nearest_in_row(&self, row: usize, lon: f64) -> u32 {
        let n = self.row_len[row];
        let frac = lon.rem_euclid(360.0) / 360.0;
        ((frac * n as f64).round() as u32) % n
    }

    /// Nearest-neighbour sampling plan for a regular target grid.
    /// Returns the contiguous 1-D window to read and, per target cell
    /// (row-major over `lats` × `lons`), the index into that window.
    pub fn nearest_plan(&self, lats: &[f64], lons: &[f64]) -> (Range<u64>, Vec<u32>) {
        let rows: Vec<usize> = lats.iter().map(|&l| self.nearest_row(l)).collect();
        let (rmin, rmax) = match (rows.iter().min(), rows.iter().max()) {
            (Some(a), Some(b)) => (*a, *b),
            _ => return (0..0, Vec::new()),
        };
        let base = self.row_off[rmin];
        let window = base..self.row_off[rmax + 1];
        let mut map = Vec::with_capacity(lats.len() * lons.len());
        for &row in &rows {
            let off = self.row_off[row] - base;
            for &lon in lons {
                map.push((off + self.nearest_in_row(row, lon) as u64) as u32);
            }
        }
        (window, map)
    }
}

/// Parse the octahedral N out of a WKT REMARK such as
/// `REMARK["Reduced Gaussian Grid O1280 (ECMWF)"]`.
pub fn parse_octahedral_n(wkt: &str) -> Option<u32> {
    let i = wkt.find("Gaussian Grid O")? + "Gaussian Grid O".len();
    let digits: String = wkt[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Northern-hemisphere Gaussian latitudes (degrees, descending) for a grid
/// with `n` rows per hemisphere: `asin` of the positive roots of P_{2n}.
pub fn gaussian_latitudes(n: u32) -> Vec<f64> {
    let deg = 2 * n as usize;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        // Newton iteration on the Legendre recurrence (Numerical Recipes gauleg).
        let mut z = (std::f64::consts::PI * (i as f64 + 0.75) / (deg as f64 + 0.5)).cos();
        loop {
            let mut p1 = 1.0f64;
            let mut p2 = 0.0f64;
            for j in 1..=deg {
                let p3 = p2;
                p2 = p1;
                p1 = ((2 * j - 1) as f64 * z * p2 - (j - 1) as f64 * p3) / j as f64;
            }
            let pp = deg as f64 * (z * p1 - p2) / (z * z - 1.0);
            let z1 = z;
            z = z1 - p1 / pp;
            if (z - z1).abs() < 1e-15 {
                break;
            }
        }
        out.push(z.asin().to_degrees());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o1280_geometry() {
        assert_eq!(OctahedralGrid::total_points(1280), 6_599_680);
        assert_eq!(OctahedralGrid::n_from_total(6_599_680), Some(1280));
        assert_eq!(OctahedralGrid::n_from_total(6_599_681), None);
        let g = OctahedralGrid::new(1280);
        assert_eq!(g.row_len.len(), 2560);
        assert_eq!(g.row_len[0], 20);
        assert_eq!(g.row_len[1279], 5136);
        assert_eq!(g.row_len[1280], 5136);
        assert_eq!(g.total(), 6_599_680);
        // First Gaussian latitude of N1280 (ECMWF documentation): 89.946187715665774
        assert!((g.lats[0] - 89.946_187_7).abs() < 1e-6, "{}", g.lats[0]);
        assert!((g.lats[2559] + 89.946_187_7).abs() < 1e-6);
        assert!(g.lats[1279] > 0.0 && g.lats[1280] < 0.0);
        assert!((g.lats[1279] - 0.035_149_4).abs() < 1e-5, "{}", g.lats[1279]);
    }

    #[test]
    fn small_grid_latitudes() {
        // O2: P_4 roots -> latitudes asin(±0.861136), asin(±0.339981)
        let l = gaussian_latitudes(2);
        assert!((l[0] - 0.861_136_311_594_053f64.asin().to_degrees()).abs() < 1e-9);
        assert!((l[1] - 0.339_981_043_584_856f64.asin().to_degrees()).abs() < 1e-9);
    }

    #[test]
    fn nearest_lookup() {
        let g = OctahedralGrid::new(1280);
        assert_eq!(g.nearest_row(0.035), 1279);
        assert_eq!(g.nearest_row(-0.035), 1280);
        assert_eq!(g.nearest_row(90.0), 0);
        assert_eq!(g.nearest_row(-90.0), 2559);
        assert_eq!(g.nearest_in_row(1279, 90.0), 1284);
        assert_eq!(g.nearest_in_row(1279, -90.0), 3852);
        assert_eq!(g.nearest_in_row(1279, 359.99), 0);
        let (win, map) = g.nearest_plan(&[0.035, -0.035], &[0.0, 90.0]);
        assert_eq!(win, g.row_off[1279]..g.row_off[1281]);
        assert_eq!(map, vec![0, 1284, 5136, 5136 + 1284]);
    }

    #[test]
    fn remark_parsing() {
        assert_eq!(
            parse_octahedral_n(r#"REMARK["Reduced Gaussian Grid O1280 (ECMWF)"]"#),
            Some(1280)
        );
        assert_eq!(parse_octahedral_n("GEOGCRS[]"), None);
        let g = OctahedralGrid::from_crs_wkt(
            r#"REMARK["Reduced Gaussian Grid O1280 (ECMWF)"]"#,
            &[1, 6_599_680],
        )
        .unwrap();
        assert_eq!(g.n, 1280);
        assert!(
            OctahedralGrid::from_crs_wkt(
                r#"REMARK["Reduced Gaussian Grid O640"]"#,
                &[1, 6_599_680]
            )
            .is_err()
        );
    }
}
