# om2nc

Fetch [Open-Meteo open-data](https://open-meteo.com/en/docs/open-data) spatial
forecasts (`.om` files on `s3://openmeteo/data_spatial/`) and write them as
CF-compliant NetCDF-4 — one file per model run, cropped to a bounding box.

```sh
om2nc fetch --model ecmwf_ifs --init 2026-09-15T00Z \
  --step 0..144:3 --var temperature_2m,precipitation \
  --bbox 120,20,150,50 --missing-as-nan -o ifs_20260915T00.nc
```

Only the bytes covering the requested cells are downloaded (HTTP range reads
through [OpenDAL](https://opendal.apache.org/) + [rust-omfiles](https://github.com/open-meteo/rust-omfiles)):
the example above pulls ~1.2 MiB per step out of 128 MiB files; a 0.25° model
such as `ecmwf_ifs025` needs ~350 KiB per step.

Install: grab a static binary from the
[Releases page](https://github.com/ringsaturn/om2nc/releases) or
`cargo install --git https://github.com/ringsaturn/om2nc --features static`
— details in [Installation](#installation).

## Usage

### Explore what is available

```sh
# All models under data_spatial/ (one directory per model)
om2nc models

# Latest completed run of a model: reference time, steps, variables, CRS
om2nc info --model ecmwf_ifs025
# ... or the raw latest.json / in-progress.json
om2nc info --model ecmwf_ifs025 --json

# Variable tree of one .om file (dims, chunks, compression, scale factor, units)
om2nc inspect --model ecmwf_ifs025 --step 3
om2nc inspect --model ecmwf_ifs025 --init 2026-09-15T00Z --step 24
```

### Fetch: the basics

```sh
# Latest run, two variables, 3-hourly out to +144 h, East Asia box
# (precipitation does not exist at step 0, hence --missing-as-nan; see below)
om2nc fetch --model ecmwf_ifs025 --step 0..144:3 \
  --var temperature_2m,precipitation --bbox 120,20,150,50 --missing-as-nan -o out.nc

# A specific run (accepted forms: 2026-09-15T00Z, 2026-09-15T0000Z,
# 2026-09-15T00:00Z, 2026-09-15T00:00:00Z)
om2nc fetch --model ecmwf_ifs025 --init 2026-09-15T00Z --step 0..48:3 \
  --var temperature_2m -o out.nc

# Every step and every variable the run provides (no --step, no --var)
om2nc fetch --model ecmwf_ifs025 --bbox 120,20,150,50 --missing-as-nan -o full.nc

# Whole globe (no --bbox)
om2nc fetch --model ecmwf_ifs025 --step 0,24,48 --var pressure_msl -o global.nc
```

`--var` may be repeated or comma separated: `--var a,b --var c`.

### Choosing steps

`--step` takes hours since the reference time:

| Spec | Meaning |
| --- | --- |
| `0..144:3` | 0, 3, 6, …, 144 |
| `0..48` | 0, 1, 2, …, 48 (stride 1 h) |
| `0,6,12,24` | exactly these |
| `0..6:0.25` | 15-minute steps (for `*_15min` models) |
| `3..24:3,48,72` | ranges and lists may be mixed |
| *(omitted)* | every step the run provides |

Steps that the run does not provide are rejected up front, with the list of
what is available.

### Bounding boxes

`--bbox west,south,east,north` in degrees. Longitudes may be given in either
-180..180 or 0..360 (`--bbox 200,0,210,10` selects 160°W–150°W). For regular
grids the output holds every grid point whose centre lies inside the box (no
resampling); boxes that only partly overlap a regional model are clipped,
boxes with no overlap are an error. Boxes crossing the antimeridian are not
supported yet.

```sh
# Regional model, box larger than the domain -> clipped to the domain
om2nc fetch --model jma_msm --step 0..6 --var temperature_2m \
  --bbox 100,40,125,60 -o msm.nc
```

### Precipitation and other interval variables

```sh
# Step 0 has no precipitation -> either skip it ...
om2nc fetch --model ecmwf_ifs025 --step 3..144:3 --var precipitation -o pr.nc
# ... or fill it with NaN
om2nc fetch --model ecmwf_ifs025 --step 0..144:3 --var precipitation \
  --missing-as-nan -o pr.nc

# Hourly model, 3-hourly output: sum the skipped hours instead of keeping only
# the last one (see "Interval variables" below)
om2nc fetch --model ecmwf_ifs --step 3..144:3 --var precipitation,shortwave_radiation \
  --bbox 120,20,150,50 --accumulate -o pr3h.nc
```

### ECMWF IFS on its native O1280 grid

`ecmwf_ifs` is a reduced Gaussian grid and is resampled onto a regular grid
(nearest neighbour). `--resolution` sets the output spacing (default 0.1°);
it is rejected for regular-grid models.

```sh
om2nc fetch --model ecmwf_ifs --step 0..144:3 --var temperature_2m,precipitation \
  --bbox 120,20,150,50 --missing-as-nan --accumulate -o ifs.nc
om2nc fetch --model ecmwf_ifs --step 0..24 --var wind_gusts_10m \
  --bbox 135,30,145,40 --resolution 0.0625 -o gusts.nc
```

### Runs other than the latest

`latest.json` describes only the most recent completed run. The run currently
being written is described by `in-progress.json` and can be read with
`--allow-incomplete`. Older runs still on the bucket (about 7 days are kept)
are described by neither, so `--step` must be given explicitly and validation
falls back to the object listing and the files themselves:

```sh
# Run currently in progress (partial data!)
om2nc fetch --model ecmwf_ifs025 --init 2026-09-15T12Z --step 0..24:3 \
  --var temperature_2m --allow-incomplete -o partial.nc

# An older run
om2nc fetch --model ecmwf_ifs025 --init 2026-09-13T00Z --step 0..48:3 \
  --var temperature_2m -o old.nc
```

`--accumulate` needs the run's native cadence and therefore only works for
runs described by `latest.json` / `in-progress.json`.

### Output, performance, logging

```sh
# Replace an existing file; stronger/no compression
om2nc fetch ... --overwrite --deflate 6 -o out.nc
om2nc fetch ... --deflate 0 -o out.nc

# More parallel step files (default 8); smaller read blocks for tiny boxes
om2nc fetch ... --concurrency 16 --block-kib 16 --merge-kib 8 -o out.nc

# Quiet (warnings/errors only) or verbose (-v debug, -vv every range request)
om2nc -q fetch ... -o out.nc
om2nc -vv fetch ... -o out.nc 2> requests.log
```

Output is written to `<name>.part` and renamed on success, so an aborted run
never leaves a truncated `.nc`. Exit status is 0 on success, 1 on any
validation or transfer error, 2 for invalid arguments.

### Mirrors and other buckets

```sh
om2nc --endpoint https://minio.example.com --bucket openmeteo-mirror --region us-east-1 models
export OM2NC_ENDPOINT=https://minio.example.com   # same via environment
```

Access is anonymous (unsigned requests); the bucket must be publicly readable.

### `fetch` options at a glance

| Flag | Meaning |
| --- | --- |
| `--model` | Model name as printed by `om2nc models` (regular lat/lon or ECMWF O1280, see below). |
| `--init` | Run reference time (`2026-09-15T00Z`, `2026-09-15T00:00Z`, ...) or `latest` (default). |
| `--step` | Hours since init: `0..144:3`, `0..48` (1 h stride), `0,6,12`, decimals allowed. Default: every step of the run. |
| `--var` | Open-Meteo variable names, comma separated and/or repeated. Default: every variable of the run (`om2nc info` lists them). |
| `--bbox` | `west,south,east,north` in degrees (longitudes may use 0..360). Default: whole grid. |
| `--resolution` | Output spacing in degrees for reduced Gaussian models (`ecmwf_ifs`), default 0.1. Rejected for regular-grid models, which are cropped, never resampled. |
| `-o/--output` | Output path. Written to `<name>.part` first and renamed on success. |
| `--missing-as-nan` | Fill NaN when a step file lacks a variable (typically accumulated fields such as `precipitation` at step 0). Otherwise this is an error. |
| `--accumulate` | Combine the model's native output files so interval variables cover the whole gap between consecutive requested steps (see below). |
| `--allow-incomplete` | Read a run that is still listed in `in-progress.json`. |
| `--concurrency` | Step files fetched in parallel (default 8); up to 8 variables per file are read in parallel on top of that. |
| `--deflate` | zlib level for data variables (default 4, 0 = off). |
| `--overwrite` | Replace an existing output file. |
| `--block-kib`, `--merge-kib` | Read-cache block size and chunk-merge threshold (defaults 32 / 16 KiB). |
| `--bucket/--region/--endpoint` | Point at a mirror (`OM2NC_BUCKET`, `OM2NC_REGION`, `OM2NC_ENDPOINT`). |

### Validation before anything is written

1. `latest.json` (or `in-progress.json`) is read; every requested step and
   variable must be listed, otherwise the missing ones are reported and the
   command fails.
2. Every step object must exist (`stat`).
3. The first file's tree is read: requested variables must be present as
   arrays, and `crs_wkt` + array shape must describe a regular lat/lon grid or
   an octahedral reduced Gaussian grid.
4. Each file's own `forecast_reference_time` / `valid_time` scalars are checked
   against the requested run and step.

Runs older than the one in `latest.json` are not described by any JSON, so for
those `--step` must be given explicitly; the file-level checks still apply.
Note that spatial data is only kept for about 7 days on the bucket.

## Output layout (CF-1.10)

```
dimensions: time, latitude, longitude
time(time)                double  "hours since <init>"   standard_name=time
step(time)                double  "hours"                standard_name=forecast_period
forecast_reference_time   double  "seconds since 1970-01-01"
latitude(latitude)        double  degrees_north (ascending, south → north)
longitude(longitude)      double  degrees_east
crs                       int     grid_mapping_name=latitude_longitude, crs_wkt=<from .om>
<var>(time,latitude,longitude)  float  zlib(4)+shuffle, chunks (1, ny, nx), _FillValue=NaN
        long_name, units (udunits spelling), open_meteo_units (original),
        standard_name (when a clean CF match exists), grid_mapping="crs"
global: Conventions, title, source, history (om2nc version + argv),
        license="CC BY 4.0", attribution (Open-Meteo + data provider), model,
        source_grid + regrid_method (only when resampled, see below)
```

Variable names are the Open-Meteo names. `xarray.open_dataset` decodes `time`
and `forecast_reference_time` directly:

```python
import xarray as xr
ds = xr.open_dataset("ifs.nc")
ds.temperature_2m.sel(latitude=35.7, longitude=139.7, method="nearest").plot()
ds.precipitation.isel(time=1).plot()          # step 0 is NaN with --missing-as-nan
ds.time_bnds                                  # interval each step covers
```

### Interval variables (precipitation, radiation, max/min)

Instantaneous fields (`temperature_2m`, `pressure_msl`, …) are simply the
value at the valid time. Interval fields are different: in every `.om` file
they describe the interval **since the previous native output time of the
model**, and that cadence varies with lead time — `ecmwf_ifs` is hourly to
+90 h, 3-hourly to +144 h, 6-hourly to +360 h; `ecmwf_ifs025` is 3-hourly
throughout. om2nc classifies them as

| `cell_methods` | Variables | Combined by |
| --- | --- | --- |
| `time: sum` | `precipitation`, `rain`, `showers`, `snowfall_water_equivalent`, `runoff`, … | sum |
| `time: mean` | `shortwave_radiation`, `direct_radiation`, `diffuse_radiation`, heat fluxes, … | duration-weighted mean |
| `time: maximum` / `minimum` | `temperature_2m_max`, `temperature_2m_min`, `wind_gusts_10m` | max / min |

and writes a `time_bnds(time, nv)` variable (referenced by `time:bounds`)
holding the interval each output step covers, plus a `comment` on every
interval variable spelling out the run's native cadence.

By default each step reads only its own file, so `--step 0..144:3` on an
hourly model yields precipitation of the **last hour only** — om2nc warns
about this. With `--accumulate`, every native file between consecutive
requested steps is read and combined (`(previous requested step, step]`;
from init for the first step), so the same command yields true 3-hour
totals, and `time_bnds` says so. The cost is one extra read per skipped
native file (~1 MiB each for `ecmwf_ifs`). Large gaps between requested
steps accumulate over the whole gap, which is correct but can be expensive.

## How the grid is reconstructed

`.om` files do not carry coordinate arrays. Each file has a `crs_wkt` scalar
and the array shape tells the rest.

### Regular lat/lon grids (cropped)

`BBOX[south,west,north,east]` in `crs_wkt` gives the extent of the cell
centres and each variable is a `(ny, nx)` array; row 0 is the southernmost
latitude, column 0 the westernmost longitude. Spacing is `(north-south)/(ny-1)`
and `(east-west)/(nx-1)`. Checked against the Open-Meteo API for `ecmwf_ifs025`
(0.25°, 721×1440, `BBOX[-90,-180,90,179.75]`) and `jma_msm`.

Examples: `ecmwf_ifs025`, `dwd_icon`, `dwd_icon_d2`, `ncep_gfs013`, `jma_msm`,
`meteofrance_arome_france0025`. Caveat: `ncep_gfs013` is a Gaussian grid whose
latitudes are only approximately equidistant; the reconstructed latitudes
deviate by a few thousandths of a degree near the poles.

### Octahedral reduced Gaussian grids (resampled)

`ecmwf_ifs` is ECMWF's native O1280 grid, stored as one `(1, 6599680)` array:
2560 rows north → south, row *i* (from the pole) holding `16 + 4i` points at
longitudes `360·j/n_row` starting at 0°. Latitudes are the Gaussian latitudes
(roots of the Legendre polynomial P₂₅₆₀, computed with Newton iteration; the
first row is 89.946188°). N is inferred from the point count (`4N² + 36N`)
and cross-checked against the `O<N>` in the WKT REMARK.

om2nc resamples these onto a regular grid (`--resolution`, default 0.1°;
native spacing is ~0.07° at the equator) with **nearest-neighbour** sampling,
the same cell selection the Open-Meteo API uses, so values are untouched (no
smoothing of precipitation) and match `cell_selection=nearest` API output.
The latitude band covering the bbox is one contiguous index range, so it is
read with a single windowed read. The output carries
`source_grid = "octahedral reduced Gaussian O1280"` and `regrid_method = "nearest"`.

Nearest sampling at 0.1° leaves ~30 % of source points unsampled; use a finer
`--resolution` (e.g. 0.0625) if area statistics matter.

### Rejected

Rotated grids (`cmc_gem_hrdps`, `knmi_harmonie_arome_europe`, …) and projected
grids (`ncep_hrrr_conus`) are refused with an explicit error; they would need
a proper reprojection.

## Installation

### Prebuilt binaries (recommended)

Every release on the [Releases page](https://github.com/ringsaturn/om2nc/releases)
ships a statically linked single-file binary (no libnetcdf/HDF5 needed at
runtime) for

| Asset name | Platform |
| --- | --- |
| `om2nc-<tag>-x86_64-unknown-linux-gnu` | Linux x86_64 (glibc) |
| `om2nc-<tag>-aarch64-unknown-linux-gnu` | Linux arm64 (glibc) |
| `om2nc-<tag>-aarch64-apple-darwin` | macOS Apple Silicon |

plus `<asset>.sha256` per file and a combined `SHA256SUMS`.

```sh
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-gnu        # or aarch64-unknown-linux-gnu / aarch64-apple-darwin
NAME="om2nc-${VERSION}-${TARGET}"
BASE="https://github.com/ringsaturn/om2nc/releases/download/${VERSION}"

curl -sSfL -o om2nc "${BASE}/${NAME}"
curl -sSfL "${BASE}/${NAME}.sha256" | sed "s| ${NAME}| om2nc|" | sha256sum -c -   # macOS: shasum -a 256 -c -
chmod +x om2nc
sudo install -m 755 om2nc /usr/local/bin/om2nc

om2nc --version
```

On macOS the binary is not notarized; if Gatekeeper complains, run
`xattr -d com.apple.quarantine om2nc` once.

### With cargo

```sh
# needs libnetcdf at build time (see below) ...
cargo install --git https://github.com/ringsaturn/om2nc --tag v0.1.0
# ... or build libnetcdf/HDF5 from source and link them statically (needs cmake; ~2 min)
cargo install --git https://github.com/ringsaturn/om2nc --tag v0.1.0 --features static
```

The binary lands in `~/.cargo/bin/om2nc`.

### From source

```sh
git clone https://github.com/ringsaturn/om2nc && cd om2nc

# libnetcdf (with HDF5) for the default dynamic build
sudo apt-get install libnetcdf-dev      # Debian/Ubuntu
brew install netcdf                     # macOS

cargo build --release                   # -> target/release/om2nc
cargo build --release --features static # portable binary, needs cmake, no libnetcdf required
cargo test
```

Rust 1.88 or newer (edition 2024, let chains). The dynamic build links whatever
`libnetcdf` the system provides (4.x with HDF5 support); the `static` feature
compiles libnetcdf and libhdf5 from source via the `netcdf-src`/`hdf5-src`
crates and is what the release workflow uses.

### In a GitHub Actions workflow

Pin the version and its checksum so a compromised release cannot slip in:

```yaml
- name: Install om2nc
  env:
    OM2NC_VERSION: v0.1.0
    OM2NC_SHA256: <sha256 of om2nc-v0.1.0-x86_64-unknown-linux-gnu from SHA256SUMS>
  run: |
    set -euo pipefail
    name="om2nc-${OM2NC_VERSION}-x86_64-unknown-linux-gnu"
    curl -sSfL -o om2nc "https://github.com/ringsaturn/om2nc/releases/download/${OM2NC_VERSION}/${name}"
    echo "${OM2NC_SHA256}  om2nc" | sha256sum -c -
    sudo install -m 755 om2nc /usr/local/bin/om2nc
- run: om2nc fetch --model ecmwf_ifs --step 0..144:3 --var temperature_2m,precipitation \
         --bbox 120,20,150,50 --missing-as-nan --accumulate -o ifs.nc
```

No credentials are needed: the Open-Meteo bucket is public and om2nc sends
unsigned requests.

## License

GPL-2.0-only (see `LICENSE`). The data is provided by Open-Meteo under
CC BY 4.0; the attribution is written into every output file.
