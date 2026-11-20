//! Great-circle distance and the bounding box used as an R*-tree prefilter.

/// IUGG mean Earth radius.
const EARTH_RADIUS_KM: f64 = 6371.0088;
/// Absorbs trigonometric rounding so the prefilter never drops a point that the
/// exact haversine check would keep.
const PAD_DEG: f64 = 1e-6;

pub(crate) fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().min(1.0).asin()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BBox {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
}

/// A box containing every point within `km` of (lat, lon). Near a pole or across
/// the antimeridian it widens to all longitudes, which is correct (the exact
/// distance check follows) and only costs prefilter selectivity.
pub(crate) fn bbox(lat: f64, lon: f64, km: f64) -> BBox {
    let r = km / EARTH_RADIUS_KM; // angular radius
    let dlat = r.to_degrees() + PAD_DEG;
    let (min_lat, max_lat) = (lat - dlat, lat + dlat);
    let all_lon = BBox { min_lat: min_lat.max(-90.0), max_lat: max_lat.min(90.0), min_lon: -180.0, max_lon: 180.0 };
    if min_lat <= -90.0 || max_lat >= 90.0 {
        return all_lon;
    }
    let s = r.sin() / lat.to_radians().cos();
    if r >= std::f64::consts::FRAC_PI_2 || s >= 1.0 {
        return all_lon;
    }
    let dlon = s.asin().to_degrees() + PAD_DEG;
    if lon - dlon < -180.0 || lon + dlon > 180.0 {
        return all_lon;
    }
    BBox { min_lat, max_lat, min_lon: lon - dlon, max_lon: lon + dlon }
}
