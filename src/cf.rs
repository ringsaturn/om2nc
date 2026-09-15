//! CF metadata: units normalisation, standard names, attribution text.

/// Map Open-Meteo unit strings to udunits-compatible spellings.
/// Returns the input unchanged when no mapping is needed.
pub fn cf_units(om_units: &str) -> &str {
    match om_units {
        "°C" => "degC",
        "W/m²" => "W m-2",
        "kg/m²" => "kg m-2",
        "m³/m³" => "m3 m-3",
        "m/s" => "m s-1",
        "km/h" => "km h-1",
        "°" => "degree",
        "wmo code" | "undefined" => "1",
        "" => "1",
        other => other,
    }
}

/// CF standard name for an Open-Meteo variable, when there is a clean match.
pub fn standard_name(var: &str) -> Option<&'static str> {
    // Pressure-level variables: strip the trailing `_<N>hPa`.
    let base = match var.rsplit_once('_') {
        Some((b, lvl)) if lvl.ends_with("hPa") && lvl[..lvl.len() - 3].parse::<u32>().is_ok() => b,
        _ => var,
    };
    Some(match base {
        "temperature_2m" | "temperature" => "air_temperature",
        "dew_point_2m" => "dew_point_temperature",
        "relative_humidity_2m" | "relative_humidity" => "relative_humidity",
        "precipitation" => "precipitation_amount",
        "rain" => "rainfall_amount",
        "snowfall_water_equivalent" => "snowfall_amount",
        "pressure_msl" => "air_pressure_at_mean_sea_level",
        "surface_pressure" => "surface_air_pressure",
        "wind_u_component_10m" | "wind_u_component_100m" | "wind_u_component" => "eastward_wind",
        "wind_v_component_10m" | "wind_v_component_100m" | "wind_v_component" => "northward_wind",
        "wind_gusts_10m" => "wind_speed_of_gust",
        "cloud_cover" => "cloud_area_fraction",
        "shortwave_radiation" => "surface_downwelling_shortwave_flux_in_air",
        "direct_radiation" => "surface_direct_downwelling_shortwave_flux_in_air",
        "diffuse_radiation" => "surface_diffuse_downwelling_shortwave_flux_in_air",
        "surface_temperature" => "surface_temperature",
        "sea_surface_temperature" => "sea_surface_temperature",
        "snow_depth" => "surface_snow_thickness",
        "cape" => "atmosphere_convective_available_potential_energy_wrt_surface",
        "convective_inhibition" => "atmosphere_convective_inhibition_wrt_surface",
        "boundary_layer_height" => "atmosphere_boundary_layer_thickness",
        "total_column_integrated_water_vapour" => "atmosphere_mass_content_of_water_vapor",
        "visibility" => "visibility_in_air",
        "geopotential_height" => "geopotential_height",
        "vertical_velocity" => "upward_air_velocity",
        "latent_heat_flux" => "surface_upward_latent_heat_flux",
        "sensible_heat_flux" => "surface_upward_sensible_heat_flux",
        "freezing_level_height" => "freezing_level_altitude",
        "sea_level_height_msl" => "sea_surface_height_above_mean_sea_level",
        "ocean_u_current" => "eastward_sea_water_velocity",
        "ocean_v_current" => "northward_sea_water_velocity",
        "sea_ice_thickness" => "sea_ice_thickness",
        "sea_water_salinity" => "sea_water_salinity",
        _ => return None,
    })
}

/// How a variable relates to the interval since the previous model output time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reduction {
    /// Amount accumulated over the interval (mm); combine sub-intervals by summing.
    Sum,
    /// Mean flux over the interval (W/m²); combine by duration-weighted mean.
    Mean,
    Maximum,
    Minimum,
}

impl Reduction {
    pub fn cell_methods(self) -> &'static str {
        match self {
            Self::Sum => "time: sum",
            Self::Mean => "time: mean",
            Self::Maximum => "time: maximum",
            Self::Minimum => "time: minimum",
        }
    }
}

/// Interval semantics of an Open-Meteo variable, `None` for instantaneous fields.
pub fn interval_reduction(var: &str) -> Option<Reduction> {
    Some(match var {
        "precipitation"
        | "rain"
        | "showers"
        | "snowfall"
        | "snowfall_water_equivalent"
        | "runoff"
        | "evapotranspiration"
        | "et0_fao_evapotranspiration" => Reduction::Sum,
        "shortwave_radiation"
        | "direct_radiation"
        | "diffuse_radiation"
        | "direct_normal_irradiance"
        | "global_tilted_irradiance"
        | "terrestrial_radiation"
        | "shortwave_radiation_clear_sky"
        | "latent_heat_flux"
        | "sensible_heat_flux" => Reduction::Mean,
        "temperature_2m_max" | "wind_gusts_10m" => Reduction::Maximum,
        "temperature_2m_min" => Reduction::Minimum,
        _ => return None,
    })
}

/// Human readable name derived from the variable name.
pub fn long_name(var: &str) -> String {
    var.replace('_', " ")
}

/// Data provider behind a model, for the attribution attribute.
pub fn provider(model: &str) -> &'static str {
    const TABLE: &[(&str, &str)] = &[
        ("ecmwf_", "ECMWF"),
        ("dwd_", "Deutscher Wetterdienst (DWD)"),
        ("ncep_", "NOAA NCEP"),
        ("meteofrance_", "Météo-France"),
        ("cmc_", "Environment and Climate Change Canada (CMC)"),
        ("jma_", "Japan Meteorological Agency (JMA)"),
        ("ukmo_", "UK Met Office"),
        ("knmi_", "KNMI"),
        ("dmi_", "Danish Meteorological Institute (DMI)"),
        ("metno_", "MET Norway"),
        ("bom_", "Australian Bureau of Meteorology (BOM)"),
        ("cma_", "China Meteorological Administration (CMA)"),
        ("kma_", "Korea Meteorological Administration (KMA)"),
        ("italia_meteo_", "ItaliaMeteo / ARPAE"),
        ("geosphere_", "GeoSphere Austria"),
        ("chmi_", "Czech Hydrometeorological Institute (CHMI)"),
        ("cams_", "Copernicus Atmosphere Monitoring Service (CAMS)"),
        ("meteoswiss_", "MeteoSwiss"),
    ];
    TABLE
        .iter()
        .find(|(p, _)| model.starts_with(p))
        .map(|(_, name)| *name)
        .unwrap_or("the originating national weather service")
}

pub fn attribution(model: &str) -> String {
    format!(
        "Weather data by Open-Meteo.com (https://open-meteo.com), based on {} model {model}. \
         Licensed under CC BY 4.0.",
        provider(model)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(cf_units("°C"), "degC");
        assert_eq!(cf_units("mm"), "mm");
        assert_eq!(cf_units(""), "1");
    }

    #[test]
    fn names() {
        assert_eq!(standard_name("temperature_2m"), Some("air_temperature"));
        assert_eq!(standard_name("temperature_850hPa"), Some("air_temperature"));
        assert_eq!(standard_name("wind_u_component_500hPa"), Some("eastward_wind"));
        assert_eq!(standard_name("temperature_2m_max"), None);
        assert_eq!(standard_name("soil_moisture_0_to_7cm"), None);
        assert_eq!(long_name("wind_gusts_10m"), "wind gusts 10m");
    }

    #[test]
    fn reductions() {
        assert_eq!(interval_reduction("precipitation"), Some(Reduction::Sum));
        assert_eq!(interval_reduction("shortwave_radiation"), Some(Reduction::Mean));
        assert_eq!(interval_reduction("temperature_2m_min"), Some(Reduction::Minimum));
        assert_eq!(interval_reduction("temperature_2m"), None);
        assert_eq!(Reduction::Sum.cell_methods(), "time: sum");
    }

    #[test]
    fn providers() {
        assert_eq!(provider("ecmwf_ifs025"), "ECMWF");
        assert!(attribution("dwd_icon").contains("DWD"));
    }
}
