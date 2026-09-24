//! GeoJSON output for vector features (vegetation polygons, OSM features, contours),
//! plus bbox cropping and batch merging of the produced files.
//!
//! Coordinates are written in the native projected CRS of the input data, or in WGS84 for
//! the batch outputs with `geojson_wgs84` (see [`Wgs84`]).

use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use log::info;
use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Geometry};
use crate::io::fs::FileSystem;
/// Rust types generated from `schema/geojson.schema.json` by `typify` in `build.rs`.
///
/// These types are the serialization contract for GeoJSON output properties.
/// Add new property classes to the schema file; `cargo build` regenerates this
/// module automatically.
#[allow(dead_code, clippy::all)]
mod geojson_types {
    include!(concat!(env!("OUT_DIR"), "/geojson_types.rs"));
}

/// One GeoJSON vector output, by its file-name suffix: `temp/<name>.geojson` per tile,
/// `<tile>_<name>.geojson` in the batch output folder, `merged_<name>.geojson` after
/// `batchmerge`. Add new outputs here.
pub struct GeoJsonOutput {
    pub name: &'static str,
}

pub const GEOJSON_OUTPUTS: &[GeoJsonOutput] = &[
    GeoJsonOutput { name: "contours" },
    GeoJsonOutput { name: "formlines" },
    GeoJsonOutput { name: "dotknolls" },
    GeoJsonOutput { name: "cliffs" },
    GeoJsonOutput { name: "vegetation" },
    GeoJsonOutput { name: "yellow" },
    GeoJsonOutput {
        name: "undergrowth",
    },
    GeoJsonOutput { name: "osm_lines" },
    GeoJsonOutput { name: "osm_areas" },
];

/// Legacy GeoJSON `crs` member for a projected EPSG code. RFC 7946 dropped `crs`, but
/// GIS tools still read it, and without it projected coordinates load misplaced.
/// None (no `epsg` config key) omits the member.
pub fn crs(epsg: Option<u32>) -> Option<Value> {
    epsg.map(|code| {
        json!({"type":"name","properties":{"name": format!("urn:ogc:def:crs:EPSG::{code}")}})
    })
}

fn write_prelude<W: Write>(w: &mut W, crs: Option<&Value>) -> anyhow::Result<()> {
    w.write_all(br#"{"type":"FeatureCollection","#)?;
    if let Some(c) = crs {
        w.write_all(br#""crs":"#)?;
        serde_json::to_writer(&mut *w, c)?;
        w.write_all(b",")?;
    }
    w.write_all(br#""features":["#)?;
    Ok(())
}

/// Round to cm to keep files small; sub-cm is noise at map scale.
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Round to 1e-7 degrees (about 1 cm), the WGS84 counterpart of [`r2`].
fn r7(v: f64) -> f64 {
    (v * 1e7).round() / 1e7
}

/// Reprojection from the data's projected CRS to WGS84 longitude/latitude, the only CRS
/// RFC 7946 allows and the only one tools such as tippecanoe read.
pub struct Wgs84 {
    src: proj4rs::Proj,
    dst: proj4rs::Proj,
}

impl Wgs84 {
    pub fn new(epsg: u32) -> anyhow::Result<Self> {
        let proj = |code: u32| {
            u16::try_from(code)
                .ok()
                .and_then(|c| proj4rs::Proj::from_epsg_code(c).ok())
                .ok_or_else(|| anyhow::anyhow!("no projection definition for EPSG:{code}"))
        };
        Ok(Self {
            src: proj(epsg)?,
            dst: proj(4326)?,
        })
    }

    pub fn point(&self, p: [f64; 2]) -> anyhow::Result<[f64; 2]> {
        let mut q = (p[0], p[1], 0.0);
        proj4rs::transform::transform(&self.src, &self.dst, &mut q)?;
        // proj4rs yields geographic coordinates in radians
        Ok([r7(q.0.to_degrees()), r7(q.1.to_degrees())])
    }

    /// Reproject every position of a GeoJSON geometry in place.
    pub fn geometry(&self, geometry: &mut Value) -> anyhow::Result<()> {
        fn walk(w: &Wgs84, v: &mut Value) -> anyhow::Result<()> {
            let Some(items) = v.as_array_mut() else {
                return Ok(());
            };
            if let (Some(x), Some(y)) = (
                items.first().and_then(Value::as_f64),
                items.get(1).and_then(Value::as_f64),
            ) {
                let [lon, lat] = w.point([x, y])?;
                *v = json!([lon, lat]);
                return Ok(());
            }
            items.iter_mut().try_for_each(|c| walk(w, c))
        }
        walk(self, &mut geometry["coordinates"])
    }
}

/// Build a coordinate array for one line/ring.
pub fn coords_line<I: IntoIterator<Item = [f64; 2]>>(pts: I) -> Value {
    Value::Array(
        pts.into_iter()
            .map(|p| json!([r2(p[0]), r2(p[1])]))
            .collect(),
    )
}

/// Build a GeoJSON feature with string properties.
pub fn feature(gtype: &str, coordinates: Value, props: &[(&str, &str)]) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in props {
        m.insert(k.to_string(), Value::String(v.to_string()));
    }
    json!({
        "type": "Feature",
        "properties": Value::Object(m),
        "geometry": {"type": gtype, "coordinates": coordinates}
    })
}

/// Write a FeatureCollection. `crs` is included verbatim when given (see [`crs`]).
pub fn write_feature_collection<W: Write>(
    w: &mut W,
    features: &[Value],
    crs: Option<&Value>,
) -> anyhow::Result<()> {
    write_prelude(w, crs)?;
    for (i, f) in features.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        serde_json::to_writer(&mut *w, f)?;
    }
    w.write_all(b"]}")?;
    Ok(())
}

/// ISOM 2017-2 symbol code for a KP layer name, where one exists.
/// 101 contour, 102 index contour, 103 form line, 109 small knoll,
/// 111 small depression, 201 impassable cliff, 202 rock face.
fn layer_isom(layer: &str) -> Option<&'static str> {
    Some(match layer {
        "cont" | "contour" | "depression" => "101",
        "contour_index" | "depression_index" => "102",
        // intermediate (half-interval) contours are represented as form lines in ISOM
        "contour_intermed"
        | "contour_index_intermed"
        | "depression_intermed"
        | "depression_index_intermed"
        | "formline"
        | "formline_depression" => "103",
        "dotknoll" | "uglydotknoll" => "109",
        "udepression" | "uglyudepression" => "111",
        "cliff2" => "202",
        "cliff3" | "cliff4" => "201",
        "403" => "403",
        "406" => "406",
        "407" => "407",
        "408" => "408",
        "410" => "410",
        _ => return None,
    })
}

fn layer_props(layer: &str) -> Vec<(&str, &str)> {
    let mut props = vec![("layer", layer)];
    if let Some(isom) = layer_isom(layer) {
        props.push(("isom", isom));
    }
    props
}

/// Convert one or more binary DXF files (contours, cliffs, knolls...) into a single
/// GeoJSON FeatureCollection. Polylines become LineStrings with `layer` and (when known)
/// `isom` properties; points become Points.
///
/// Property schema: see `schema/geojson.schema.json` ($defs/ContourProperties,
/// KnollProperties, CliffProperties).
pub fn bindxf_to_geojson(
    fs: &impl FileSystem,
    inputs: &[std::path::PathBuf],
    output: &std::path::Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut feats = Vec::new();
    for input in inputs {
        let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
        for geom in dxf.take_geometry() {
            match geom {
                Geometry::Polylines2(pl) => {
                    for (p, c) in pl.into_iter() {
                        feats.push(feature(
                            "LineString",
                            coords_line(p.iter().map(|pt| [pt.x, pt.y])),
                            &layer_props(c.to_layer()),
                        ));
                    }
                }
                Geometry::Polylines3(pl) => {
                    for (p, (c, h)) in pl.into_iter() {
                        let mut f = feature(
                            "LineString",
                            coords_line(p.iter().map(|pt| [pt.x, pt.y])),
                            &layer_props(c.to_layer()),
                        );
                        f["properties"]["elevation"] = json!(h);
                        feats.push(f);
                    }
                }
                Geometry::Points(pts) => {
                    for (p, c) in pts.into_iter() {
                        feats.push(feature(
                            "Point",
                            json!([r2(p.x), r2(p.y)]),
                            &layer_props(c.to_layer()),
                        ));
                    }
                }
            }
        }
    }
    write_feature_collection(
        &mut BufWriter::new(fs.create(output)?),
        &feats,
        crs(epsg).as_ref(),
    )
}

fn parse_line(coords: &Value) -> Vec<[f64; 2]> {
    coords
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some([p.first()?.as_f64()?, p.get(1)?.as_f64()?])
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Liang-Barsky clip of one segment against the bbox; None when fully outside.
fn clip_seg(
    a: [f64; 2],
    b: [f64; 2],
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> Option<([f64; 2], [f64; 2])> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-dx, a[0] - minx),
        (dx, maxx - a[0]),
        (-dy, a[1] - miny),
        (dy, maxy - a[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    Some((
        [a[0] + t0 * dx, a[1] + t0 * dy],
        [a[0] + t1 * dx, a[1] + t1 * dy],
    ))
}

/// Clip one line to the bbox with per-segment intersection (handles sparse vertices),
/// splitting it where it leaves the box.
fn clip_line(pts: Vec<[f64; 2]>, minx: f64, miny: f64, maxx: f64, maxy: f64) -> Vec<Vec<[f64; 2]>> {
    let mut out = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for w in pts.windows(2) {
        if let Some((a, b)) = clip_seg(w[0], w[1], minx, miny, maxx, maxy) {
            let contiguous = cur
                .last()
                .is_some_and(|l| (l[0] - a[0]).abs() < 1e-9 && (l[1] - a[1]).abs() < 1e-9);
            if !contiguous {
                if cur.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                cur.push(a);
            }
            cur.push(b);
        } else if cur.len() > 1 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
    }
    if cur.len() > 1 {
        out.push(cur);
    }
    out
}

/// Sutherland-Hodgman clip of a closed ring against the bbox. Returns an empty vec when
/// the ring is entirely outside; otherwise a closed ring (first point repeated last).
fn clip_ring(ring: &[[f64; 2]], minx: f64, miny: f64, maxx: f64, maxy: f64) -> Vec<[f64; 2]> {
    let mut pts: Vec<[f64; 2]> = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    for edge in 0..4 {
        let inside = |p: &[f64; 2]| match edge {
            0 => p[0] >= minx,
            1 => p[0] <= maxx,
            2 => p[1] >= miny,
            _ => p[1] <= maxy,
        };
        let intersect = |a: &[f64; 2], b: &[f64; 2]| -> [f64; 2] {
            match edge {
                0 => {
                    let t = (minx - a[0]) / (b[0] - a[0]);
                    [minx, a[1] + t * (b[1] - a[1])]
                }
                1 => {
                    let t = (maxx - a[0]) / (b[0] - a[0]);
                    [maxx, a[1] + t * (b[1] - a[1])]
                }
                2 => {
                    let t = (miny - a[1]) / (b[1] - a[1]);
                    [a[0] + t * (b[0] - a[0]), miny]
                }
                _ => {
                    let t = (maxy - a[1]) / (b[1] - a[1]);
                    [a[0] + t * (b[0] - a[0]), maxy]
                }
            }
        };
        let input = std::mem::take(&mut pts);
        if input.is_empty() {
            return vec![];
        }
        for i in 0..input.len() {
            let cur = input[i];
            let prev = input[(i + input.len() - 1) % input.len()];
            match (inside(&prev), inside(&cur)) {
                (true, true) => pts.push(cur),
                (false, true) => {
                    pts.push(intersect(&prev, &cur));
                    pts.push(cur);
                }
                (true, false) => pts.push(intersect(&prev, &cur)),
                (false, false) => {}
            }
        }
    }
    if pts.len() < 3 {
        return vec![];
    }
    pts.push(pts[0]);
    pts
}

/// Clip a Polygon's rings (exterior first). Drops the whole polygon when the exterior
/// vanishes; drops holes that vanish.
fn clip_polygon(rings: &Value, minx: f64, miny: f64, maxx: f64, maxy: f64) -> Option<Value> {
    let rings = rings.as_array()?;
    let mut out = Vec::new();
    for (i, ring) in rings.iter().enumerate() {
        let clipped = clip_ring(&parse_line(ring), minx, miny, maxx, maxy);
        if clipped.is_empty() {
            if i == 0 {
                return None;
            }
            continue;
        }
        out.push(coords_line(clipped));
    }
    Some(Value::Array(out))
}

/// Crop all features of a GeoJSON file to the bbox and write the result. With `to_wgs84`
/// the cropped features are then reprojected, so the clip itself stays in metres.
#[allow(clippy::too_many_arguments)]
pub fn crop_geojson(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
    to_wgs84: Option<&Wgs84>,
) -> anyhow::Result<()> {
    let val: Value = serde_json::from_reader(BufReader::new(fs.open(input)?))?;
    let empty = Vec::new();
    let features = val["features"].as_array().unwrap_or(&empty);

    let mut out = Vec::new();
    for f in features {
        let gtype = f["geometry"]["type"].as_str().unwrap_or("");
        let coords = &f["geometry"]["coordinates"];
        let new_geom: Option<(&str, Value)> = match gtype {
            "LineString" => {
                let parts = clip_line(parse_line(coords), minx, miny, maxx, maxy);
                match parts.len() {
                    0 => None,
                    1 => Some(("LineString", coords_line(parts.into_iter().next().unwrap()))),
                    _ => Some((
                        "MultiLineString",
                        Value::Array(parts.into_iter().map(coords_line).collect()),
                    )),
                }
            }
            "MultiLineString" => {
                let mut parts = Vec::new();
                for line in coords.as_array().unwrap_or(&empty) {
                    parts.extend(clip_line(parse_line(line), minx, miny, maxx, maxy));
                }
                if parts.is_empty() {
                    None
                } else {
                    Some((
                        "MultiLineString",
                        Value::Array(parts.into_iter().map(coords_line).collect()),
                    ))
                }
            }
            "Polygon" => clip_polygon(coords, minx, miny, maxx, maxy).map(|c| ("Polygon", c)),
            "MultiPolygon" => {
                let mut polys = Vec::new();
                for rings in coords.as_array().unwrap_or(&empty) {
                    if let Some(c) = clip_polygon(rings, minx, miny, maxx, maxy) {
                        polys.push(c);
                    }
                }
                if polys.is_empty() {
                    None
                } else {
                    Some(("MultiPolygon", Value::Array(polys)))
                }
            }
            "Point" => {
                let p = parse_line(&json!([coords]));
                if p.first()
                    .is_some_and(|p| p[0] >= minx && p[0] <= maxx && p[1] >= miny && p[1] <= maxy)
                {
                    Some(("Point", coords.clone()))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((gtype, coordinates)) = new_geom {
            let mut nf = f.clone();
            nf["geometry"] = json!({"type": gtype, "coordinates": coordinates});
            if let Some(w) = to_wgs84 {
                w.geometry(&mut nf["geometry"])?;
            }
            out.push(nf);
        }
    }
    // the input's crs declaration (if any) is carried over verbatim; WGS84 is RFC 7946's
    // implied default and is declared by leaving `crs` out
    write_feature_collection(
        &mut BufWriter::new(fs.create(output)?),
        &out,
        if to_wgs84.is_some() {
            None
        } else {
            val.get("crs")
        },
    )
}

/// Layers whose geometry is organic and benefits from Bezier curves in the DXF.
/// Roads and buildings stay as straight polylines.
fn curve_layer(layer: &str) -> bool {
    matches!(
        layer,
        "101" | "102" | "103" | "201" | "202" | "306" | "403" | "406" | "407" | "408" | "410"
    )
}

/// Fit a piecewise cubic Bezier through the (thinned) polyline with Catmull-Rom
/// tangents (factor 0.5, like OCAD's own converter default). Returns the control
/// points (3 per segment + the final endpoint), or None when too short for a curve.
fn fit_bezier(pts: &[[f64; 2]], closed: bool) -> Option<Vec<[f64; 2]>> {
    use crate::geometry::Point2;

    // thin the dense smoothed polyline first so the curve has few, meaningful vertices
    let as_p2: Vec<Point2> = pts.iter().map(|q| Point2::new(q[0], q[1])).collect();
    let thin = if closed && as_p2.len() > 4 {
        crate::vege_vector::simplify_closed(as_p2, 1.0)
    } else {
        crate::vege_vector::dp(&as_p2, 1.0)
    };
    let mut p: Vec<[f64; 2]> = thin.iter().map(|q| [q.x, q.y]).collect();
    if closed
        && p.first() != p.last()
        && let Some(f) = p.first().copied()
    {
        p.push(f);
    }
    let n = p.len();
    if n < 3 {
        return None;
    }

    // Catmull-Rom tangent at vertex i (closed: wrapped, open: one-sided at the ends)
    let tangent = |i: usize| -> [f64; 2] {
        let (prev, next) = if closed {
            // last point duplicates the first: wrap over n-1 distinct points
            let m = n - 1;
            (p[(i + m - 1) % m], p[(i + 1) % m])
        } else if i == 0 {
            (p[0], p[1])
        } else if i == n - 1 {
            (p[n - 2], p[n - 1])
        } else {
            (p[i - 1], p[i + 1])
        };
        [(next[0] - prev[0]) * 0.5, (next[1] - prev[1]) * 0.5]
    };

    let segs = n - 1;
    let mut ctrl: Vec<[f64; 2]> = Vec::with_capacity(3 * segs + 1);
    for i in 0..segs {
        let (t0, t1) = (tangent(i), tangent(i + 1));
        ctrl.push(p[i]);
        ctrl.push([p[i][0] + t0[0] / 3.0, p[i][1] + t0[1] / 3.0]);
        ctrl.push([p[i + 1][0] - t1[0] / 3.0, p[i + 1][1] - t1[1] / 3.0]);
    }
    ctrl.push(p[n - 1]);
    Some(ctrl)
}

/// The polyline a GeoJSON feature gets for a curve layer: the same fitted Bezier the
/// DXF SPLINE uses, densely sampled (GeoJSON has no curve geometry). Non-curve layers
/// and too-short lines pass through unchanged.
fn curve_points(layer: &str, pts: &[[f64; 2]], closed: bool) -> Vec<[f64; 2]> {
    if !(curve_layer(layer) && pts.len() > 3) {
        return pts.to_vec();
    }
    let Some(ctrl) = fit_bezier(pts, closed) else {
        return pts.to_vec();
    };
    const SAMPLES: usize = 8; // per Bezier segment; segments are >= 1 m after thinning
    let segs = (ctrl.len() - 1) / 3;
    let mut out = Vec::with_capacity(segs * SAMPLES + 1);
    out.push(ctrl[0]);
    for s in 0..segs {
        let c = &ctrl[3 * s..3 * s + 4];
        for k in 1..=SAMPLES {
            let t = k as f64 / SAMPLES as f64;
            let u = 1.0 - t;
            let (b0, b1, b2, b3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            out.push([
                b0 * c[0][0] + b1 * c[1][0] + b2 * c[2][0] + b3 * c[3][0],
                b0 * c[0][1] + b1 * c[1][1] + b2 * c[2][1] + b3 * c[3][1],
            ]);
        }
    }
    out
}

/// Write one SPLINE entity from the fitted piecewise cubic Bezier (see [`fit_bezier`]).
fn dxf_spline(out: &mut String, layer: &str, pts: &[[f64; 2]], closed: bool) {
    use std::fmt::Write as _;

    let Some(ctrl) = fit_bezier(pts, closed) else {
        dxf_polyline(out, layer, pts, closed, None);
        return;
    };
    let segs = (ctrl.len() - 1) / 3;
    // clamped knot vector for piecewise Bezier: 0 x4, 1 x3, ..., segs x4

    let nctrl = ctrl.len();
    let nknots = nctrl + 4;
    let _ = write!(
        out,
        "SPLINE\r\n  8\r\n{layer}\r\n 70\r\n8\r\n 71\r\n3\r\n 72\r\n{nknots}\r\n 73\r\n{nctrl}\r\n 74\r\n0\r\n"
    );
    for k in 0..=segs {
        let reps = if k == 0 || k == segs { 4 } else { 3 };
        for _ in 0..reps {
            let _ = write!(out, " 40\r\n{k}\r\n");
        }
    }
    for c in &ctrl {
        let _ = write!(out, " 10\r\n{}\r\n 20\r\n{}\r\n 30\r\n0\r\n", c[0], c[1]);
    }
    out.push_str("  0\r\n");
}

/// Emit into the curves body: SPLINE for organic layers, POLYLINE otherwise.
fn dxf_curves_entity(
    out: &mut String,
    layer: &str,
    pts: &[[f64; 2]],
    closed: bool,
    elev: Option<f64>,
) {
    if curve_layer(layer) && pts.len() > 3 {
        dxf_spline(out, layer, pts, closed);
    } else {
        dxf_polyline(out, layer, pts, closed, elev);
    }
}

/// Write one POLYLINE entity in the same format `BinaryDxf::to_dxf` uses.
fn dxf_polyline(out: &mut String, layer: &str, pts: &[[f64; 2]], closed: bool, elev: Option<f64>) {
    use std::fmt::Write as _;
    out.push_str("POLYLINE\r\n 66\r\n1\r\n  8\r\n");
    out.push_str(layer);
    if let Some(h) = elev {
        let _ = write!(out, "\r\n 38\r\n{h}");
    }
    if closed {
        out.push_str("\r\n 70\r\n1");
    }
    out.push_str("\r\n  0\r\n");
    for p in pts {
        let _ = write!(
            out,
            "VERTEX\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\n",
            p[0], p[1]
        );
    }
    out.push_str("SEQEND\r\n  0\r\n");
}

/// Combine every merged vector output in the batch folder into a single `merged_all.dxf`
/// (layer names = ISOM codes where known) plus a `merged_all.ocdCrt` cross-reference
/// table for OCAD's "Import DXF" layer-to-symbol conversion.
/// ISOM 202: minimum cliff length 0.6 mm => 9 m footprint at 1:10,000 (applied to 201
/// as well). Shorter detector fragments are noise, not mappable cliffs.
const CLIFF_MIN_LEN_M: f64 = 9.0;
/// KP emits one ~3 m dash per detected steep cell; dashes within this distance belong
/// to the same cliff face.
const CLIFF_CLUSTER_DIST: f64 = 3.0;

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// ISOM 2017-2 minimum dimensions for contours, in ground metres. The standard specifies
/// them on the 1:15,000 original, so ground metres = mm x 15: the smallest bend that can
/// be drawn is 0.25 mm centre to centre (3.75 m) and the mouth of a re-entrant or spur
/// must be wider than 0.5 mm (7.5 m). The wider bound subsumes the narrower one, so a
/// single pass at 8 m enforces both.
const MIN_MOUTH_M: f64 = 8.0;

/// ponytail: a bound on how much line one splice may consume. Nothing removed can depart
/// further than MIN_MOUTH_M from the join that replaces it, so this only stops a long
/// near-parallel double-back from being swallowed in a single cut. Upgrade path: none
/// needed unless real terrain is seen running that close to itself for this far.
const MAX_DETOUR_M: f64 = 24.0;

/// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = vx * vx + vy * vy;
    if len2 == 0.0 {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / len2).clamp(0.0, 1.0);
    dist(p, [a[0] + t * vx, a[1] + t * vy])
}

/// Splice out excursions that leave and return within MIN_MOUTH_M *and* never depart
/// further than MIN_MOUTH_M from the join replacing them — the wobbles ISOM 2017-2 means
/// by "small details on contours should be avoided because they tend to hide the main
/// features of the terrain".
///
/// Both bounds matter. The first alone would let a narrow re-entrant be truncated at any
/// neck along its length; together they guarantee nothing is removed that reaches beyond
/// what the symbol's own minimum dimension can carry. A closed ring is protected from
/// being consumed whole by requiring the kept remainder to stay above the same bound.
fn generalise_contour(pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if pts.len() < 4 {
        return pts.to_vec();
    }
    let mut cum = Vec::with_capacity(pts.len());
    cum.push(0.0);
    for w in pts.windows(2) {
        cum.push(cum[cum.len() - 1] + dist(w[0], w[1]));
    }
    let total = cum[cum.len() - 1];
    let mut out = Vec::with_capacity(pts.len());
    let mut i = 0;
    while i < pts.len() {
        out.push(pts[i]);
        // the furthest vertex that comes back within the minimum mouth on a short detour
        let mut jump = None;
        let mut j = i + 1;
        while j < pts.len() && cum[j] - cum[i] <= MAX_DETOUR_M {
            let along = cum[j] - cum[i];
            if along > MIN_MOUTH_M
                && along < total - MIN_MOUTH_M
                && dist(pts[i], pts[j]) < MIN_MOUTH_M
                && pts[i + 1..j]
                    .iter()
                    .all(|p| seg_dist(*p, pts[i], pts[j]) < MIN_MOUTH_M)
            {
                jump = Some(j);
            }
            j += 1;
        }
        i = jump.unwrap_or(i + 1);
    }
    out
}

/// ISOM 2017-2 requires that symbol 109/110 "shall not touch or overlap contours", and
/// that "contours shall be adapted or broken in order not to touch" them. The knoll's
/// position is the whole information the symbol carries, so the contour is the side that
/// gives way. 109 is a 0.4 mm dot on the 1:15,000 original — a 6 m footprint, 3 m radius
/// — plus half a contour width of air.
const KNOLL_CLEAR_M: f64 = 3.5;

/// Break a contour into the pieces that stay clear of the knoll symbols, dropping any
/// piece too short to be a line.
///
/// ponytail: cuts at vertices rather than interpolating the exact crossing point. Contour
/// vertices are ~1.2 m apart, well inside the clearance, so the gap is right to within a
/// vertex. Upgrade path: split the crossing segment if a coarser contour source appears.
fn break_at_knolls(pts: &[[f64; 2]], knolls: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    if knolls.is_empty() {
        return vec![pts.to_vec()];
    }
    let mut parts = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for p in pts {
        if knolls.iter().any(|k| dist(*k, *p) < KNOLL_CLEAR_M) {
            if cur.len() > 1 {
                parts.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        } else {
            cur.push(*p);
        }
    }
    if cur.len() > 1 {
        parts.push(cur);
    }
    parts
}

/// True for the contour family (101 contour, 102 index, 103 form line) — the layers the
/// ISOM contour rules above apply to.
fn is_contour_family(layer: &str) -> bool {
    matches!(layer.get(..3), Some("101" | "102" | "103"))
}

/// Apply the ISOM contour rules to one published line: generalise detail below what the
/// symbol can carry, then break where a knoll symbol needs room. Anything that is not a
/// contour passes through as a single piece, untouched.
fn conform_contour(layer: &str, pts: &[[f64; 2]], knolls: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    if !is_contour_family(layer) {
        return vec![pts.to_vec()];
    }
    break_at_knolls(&generalise_contour(pts), knolls)
}

/// The pieces of one line as published: ISOM-conformed, curve-sampled, then re-checked
/// against the knolls — fitting a curve through a broken end bows it back over the very
/// symbol the break was made for (measured: 2.95 m from a symbol of 3 m radius).
fn published_pieces(
    layer: &str,
    pts: &[[f64; 2]],
    closed: bool,
    knolls: &[[f64; 2]],
) -> Vec<Vec<[f64; 2]>> {
    conform_contour(layer, pts, knolls)
        .into_iter()
        .flat_map(|piece| {
            let still_closed = closed && piece.first() == piece.last();
            let sampled = curve_points(layer, &piece, still_closed);
            if is_contour_family(layer) {
                break_at_knolls(&sampled, knolls)
            } else {
                vec![sampled]
            }
        })
        .collect()
}

/// Chain KP's per-cell cliff dashes into cliff lines: cluster dash midpoints within
/// CLIFF_CLUSTER_DIST, order each cluster as a greedy nearest-neighbour path from an
/// extreme point refined with 2-opt (untangles the crossings greedy ordering leaves on
/// sharply curved faces), and drop chains shorter than the ISOM minimum.
fn chain_cliff_dashes(mids: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    use std::collections::HashMap;
    // union-find over a coarse grid
    let mut parent: Vec<usize> = (0..mids.len()).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            let r = find(parent, parent[i]);
            parent[i] = r;
        }
        parent[i]
    }
    let cell = CLIFF_CLUSTER_DIST;
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, m) in mids.iter().enumerate() {
        grid.entry(((m[0] / cell) as i64, (m[1] / cell) as i64))
            .or_default()
            .push(i);
    }
    for (i, m) in mids.iter().enumerate() {
        let (gx, gy) = ((m[0] / cell) as i64, (m[1] / cell) as i64);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(others) = grid.get(&(gx + dx, gy + dy)) {
                    for &j in others {
                        if j > i && dist(*m, mids[j]) <= CLIFF_CLUSTER_DIST {
                            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                            if ri != rj {
                                parent[ri] = rj;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..mids.len() {
        let r = find(&mut parent, i);
        clusters.entry(r).or_default().push(i);
    }

    // in order of each cluster's first dash, so the output does not depend on hashing
    let mut clusters: Vec<Vec<usize>> = clusters.into_values().collect();
    clusters.sort_unstable_by_key(|members| members[0]);
    log::debug!(
        "cliff clusters: {} from {} dashes, largest {}",
        clusters.len(),
        mids.len(),
        clusters.iter().map(Vec::len).max().unwrap_or(0)
    );
    let mut used = vec![false; mids.len()];
    let mut chains = Vec::new();
    for members in &clusters {
        // start from the point farthest from the cluster centroid
        let n = members.len() as f64;
        let cx = members.iter().map(|&i| mids[i][0]).sum::<f64>() / n;
        let cy = members.iter().map(|&i| mids[i][1]).sum::<f64>() / n;
        let start = *members
            .iter()
            .max_by(|a, b| {
                dist(mids[**a], [cx, cy])
                    .partial_cmp(&dist(mids[**b], [cx, cy]))
                    .unwrap()
            })
            .unwrap();
        // this cluster's own index, so the walk cannot step into a neighbouring cluster
        let mut local: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for &i in members {
            let m = mids[i];
            local
                .entry(((m[0] / cell) as i64, (m[1] / cell) as i64))
                .or_default()
                .push(i);
        }
        used[start] = true;
        let mut path = vec![mids[start]];
        for _ in 1..members.len() {
            let next = nearest_unused(mids, &local, &used, *path.last().unwrap(), cell);
            used[next] = true;
            path.push(mids[next]);
        }
        two_opt(&mut path);
        // A dense rock area is one cluster but no single line: a greedy walk across it
        // jumps between strands. Break it there rather than draw the jumps.
        let mut pieces: Vec<Vec<[f64; 2]>> = vec![Vec::new()];
        for p in path {
            let piece = pieces.last_mut().unwrap();
            if piece.last().is_some_and(|q| dist(*q, p) > CLIFF_MAX_STEP_M) {
                pieces.push(vec![p]);
            } else {
                piece.push(p);
            }
        }
        for piece in pieces {
            let len: f64 = piece.windows(2).map(|w| dist(w[0], w[1])).sum();
            // single-dash clusters have zero path length; use the dash length itself
            if len.max(2.9) >= CLIFF_MIN_LEN_M {
                chains.push(piece);
            }
        }
    }
    chains
}

/// A step between two chained dashes longer than this is a jump between strands of the
/// same rock area, not a continuation of one cliff line.
const CLIFF_MAX_STEP_M: f64 = 2.0 * CLIFF_CLUSTER_DIST;

/// The unused point nearest to `from`, searching the `cell`-sized grid in rings of cells
/// outward. Assumes an unused point exists.
fn nearest_unused(
    mids: &[[f64; 2]],
    grid: &std::collections::HashMap<(i64, i64), Vec<usize>>,
    used: &[bool],
    from: [f64; 2],
    cell: f64,
) -> usize {
    let (gx, gy) = ((from[0] / cell) as i64, (from[1] / cell) as i64);
    let mut best: Option<(f64, usize)> = None;
    for r in 0i64.. {
        // every point in ring r is at least (r - 1) cells away
        if best.is_some_and(|(d, _)| d <= (r - 1) as f64 * cell) {
            break;
        }
        for dx in -r..=r {
            for dy in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // inside the ring, already searched
                }
                for &j in grid.get(&(gx + dx, gy + dy)).into_iter().flatten() {
                    let d = dist(mids[j], from);
                    if !used[j] && best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, j));
                    }
                }
            }
        }
    }
    best.unwrap().1
}

/// 2-opt on an open path: reversing path[i+1..=j] swaps edges (i,i+1)/(j,j+1) for
/// (i,j)/(i+1,j+1); when j is the last point only edge (i,i+1) is replaced. It untangles
/// the crossings greedy ordering leaves on sharply curved faces. Moves are limited to a
/// window along the path, which is where those crossings are, so a rock area of tens of
/// thousands of dashes stays linear rather than cubic.
fn two_opt(path: &mut [[f64; 2]]) {
    const WINDOW: usize = 64;
    const MAX_PASSES: usize = 20;
    let n = path.len();
    for _ in 0..MAX_PASSES {
        let mut improved = false;
        for i in 0..n.saturating_sub(2) {
            for j in i + 2..n.min(i + WINDOW) {
                let (removed, added) = if j + 1 < n {
                    (
                        dist(path[i], path[i + 1]) + dist(path[j], path[j + 1]),
                        dist(path[i], path[j]) + dist(path[i + 1], path[j + 1]),
                    )
                } else {
                    (dist(path[i], path[i + 1]), dist(path[i], path[j]))
                };
                if added + 1e-9 < removed {
                    path[i + 1..=j].reverse();
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// OCAD symbol number for a DXF layer in the cross reference table.
/// osm.txt still uses ISOM 2000 codes; translate to ISOM 2017-2 symbols here.
fn crt_symbol(layer: &str) -> Option<String> {
    let base = layer.trim_end_matches('T');
    let translated = match base {
        "526" => "521.001", // building (2017: 526 is a cairn)
        "527" => "413.000", // orchard (2017: 527 is a fodder rack)
        "529" => "501.000", // parking/pitch -> paved area (2017: 529 is a line feature)
        "515" => "509.000", // railway (2017: 515 is an impassable wall)
        "516" => "510.000", // power line (2017: 516 is a fence)
        "524" => "516.000", // osm.txt maps barriers here -> fence (2017)
        // internal knoll-detector artifact: -1 tells OCAD to delete these objects
        "1010" => "-1",
        _ => "",
    };
    if !translated.is_empty() {
        return Some(translated.into());
    }
    (!base.is_empty() && base.chars().all(|c| c.is_ascii_digit())).then(|| format!("{base}.000"))
}

/// ISOM 109/110/111 point symbols must not touch or overlap each other either (12 m
/// footprint length).
const POINT_MIN_SPACING_M: f64 = 12.0;

/// The knoll and depression point symbols that survive to the map, as (position, KP
/// layer). Greedy spacing filter over points ranked by certainty: the detector's definite
/// symbols (dotknoll/udepression) win over the uncertain "ugly" variants when two
/// candidates are closer than the minimum.
fn published_knolls(features: &[Value]) -> Vec<([f64; 2], String)> {
    let mut candidates: Vec<([f64; 2], String)> = Vec::new();
    for f in features {
        if f["geometry"]["type"].as_str() != Some("Point") {
            continue;
        }
        let c = &f["geometry"]["coordinates"];
        let (Some(x), Some(y)) = (c[0].as_f64(), c[1].as_f64()) else {
            continue;
        };
        let layer = f["properties"]["layer"].as_str().unwrap_or_default();
        // internal knoll-detector artifact, not a map symbol
        if layer.is_empty() || layer == "1010" {
            continue;
        }
        candidates.push(([x, y], layer.to_string()));
    }
    candidates.sort_by_key(|(_, l)| l.starts_with("ugly"));
    let mut kept: Vec<([f64; 2], String)> = Vec::new();
    for (p, layer) in candidates {
        if kept.iter().all(|(k, _)| dist(*k, p) >= POINT_MIN_SPACING_M) {
            kept.push((p, layer));
        }
    }
    kept
}

/// The features of a GeoJSON file, or None when there is no such file.
fn read_features(fs: &impl FileSystem, path: &Path) -> anyhow::Result<Option<Vec<Value>>> {
    if !fs.exists(path) {
        return Ok(None);
    }
    let mut val: Value = serde_json::from_reader(BufReader::new(fs.open(path)?))?;
    Ok(Some(match val["features"].take() {
        Value::Array(features) => features,
        _ => Vec::new(),
    }))
}

/// The parts of a LineString or MultiLineString geometry; nothing for any other type.
fn line_parts(geometry: &Value) -> Vec<Vec<[f64; 2]>> {
    match geometry["type"].as_str() {
        Some("LineString") => vec![parse_line(&geometry["coordinates"])],
        Some("MultiLineString") => geometry["coordinates"]
            .as_array()
            .map(|parts| parts.iter().map(parse_line).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The rings of a Polygon or MultiPolygon geometry; nothing for any other type.
fn polygon_rings(geometry: &Value) -> Vec<Vec<[f64; 2]>> {
    let rings = |rings: &Value| -> Vec<Vec<[f64; 2]>> {
        rings
            .as_array()
            .map(|r| r.iter().map(parse_line).collect())
            .unwrap_or_default()
    };
    match geometry["type"].as_str() {
        Some("Polygon") => rings(&geometry["coordinates"]),
        Some("MultiPolygon") => geometry["coordinates"]
            .as_array()
            .map(|polys| polys.iter().flat_map(rings).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A line feature made of `pieces`: a LineString for one, a MultiLineString for several
/// (a contour broken around a knoll stays one feature), None for none.
fn line_feature(mut pieces: Vec<Vec<[f64; 2]>>, properties: Value) -> Option<Value> {
    let geometry = match pieces.len() {
        0 => return None,
        1 => json!({"type": "LineString", "coordinates": coords_line(pieces.remove(0))}),
        _ => json!({
            "type": "MultiLineString",
            "coordinates": Value::Array(pieces.into_iter().map(coords_line).collect()),
        }),
    };
    Some(json!({"type": "Feature", "properties": properties, "geometry": geometry}))
}

/// Turn one tile's raw vector outputs in `tmpfolder` into the features the map publishes,
/// rewriting the files in place:
///
/// * `dotknolls`: the spacing filter of [`published_knolls`];
/// * `contours` and `formlines`: each line generalised, broken around the knoll symbols
///   and curve-sampled ([`published_pieces`]). With `formline=2` the half-interval
///   `*_intermed` lines are only candidates -- the renderer's selection of them is
///   `formlines` -- so they are dropped from `contours`; in the other modes they are
///   drawn as full contours and published as 101;
/// * `cliffs`: the renderer's per-cell dashes chained into cliff lines, sub-minimum faces
///   dropped ([`chain_cliff_dashes`]).
///
/// It runs on the padded tile, before `batch_process` crops it, so a decision near a tile
/// edge is made from the same ground on both sides of it. The area outputs are left as
/// they are: the vegetation polygons are already smoothed with their shared boundaries
/// point-identical, and fitting a curve to each ring on its own would open slivers.
pub fn publish_tile(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    formline: f64,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let crs = crs(epsg);
    let write = |name: &str, features: &[Value]| -> anyhow::Result<()> {
        write_feature_collection(
            &mut BufWriter::new(fs.create(tmpfolder.join(format!("{name}.geojson")))?),
            features,
            crs.as_ref(),
        )
    };

    let mut knolls: Vec<[f64; 2]> = Vec::new();
    if let Some(features) = read_features(fs, &tmpfolder.join("dotknolls.geojson"))? {
        let kept = published_knolls(&features);
        let out: Vec<Value> = kept
            .iter()
            .map(|(p, layer)| feature("Point", json!([r2(p[0]), r2(p[1])]), &layer_props(layer)))
            .collect();
        write("dotknolls", &out)?;
        knolls = kept.into_iter().map(|(p, _)| p).collect();
    }

    for name in ["contours", "formlines"] {
        let Some(features) = read_features(fs, &tmpfolder.join(format!("{name}.geojson")))? else {
            continue;
        };
        let mut out = Vec::with_capacity(features.len());
        for f in features {
            let mut properties = f["properties"].clone();
            if properties["layer"]
                .as_str()
                .is_some_and(|l| l.ends_with("_intermed"))
            {
                if formline == 2.0 {
                    continue;
                }
                properties["isom"] = json!("101");
            }
            let isom = properties["isom"].as_str().unwrap_or_default().to_string();
            let pieces = line_parts(&f["geometry"])
                .iter()
                .flat_map(|pts| {
                    let closed = pts.len() > 2 && pts.first() == pts.last();
                    published_pieces(&isom, pts, closed, &knolls)
                })
                .collect();
            out.extend(line_feature(pieces, properties));
        }
        write(name, &out)?;
    }

    if let Some(features) = read_features(fs, &tmpfolder.join("cliffs.geojson"))? {
        let (mut mids_202, mut mids_201) = (Vec::new(), Vec::new());
        for f in &features {
            for pts in line_parts(&f["geometry"]) {
                let (Some(a), Some(b)) = (pts.first(), pts.last()) else {
                    continue;
                };
                let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
                if f["properties"]["isom"] == "202" {
                    mids_202.push(mid);
                } else {
                    mids_201.push(mid);
                }
            }
        }
        let mut out = Vec::new();
        for (mids, layer, isom) in [(&mids_202, "cliff2", "202"), (&mids_201, "cliff3", "201")] {
            for chain in chain_cliff_dashes(mids) {
                out.push(feature(
                    "LineString",
                    coords_line(curve_points(isom, &chain, false)),
                    &[("layer", layer), ("isom", isom)],
                ));
            }
        }
        write("cliffs", &out)?;
    }
    Ok(())
}

/// Combine the merged per-tile outputs, already in their published form (see
/// [`publish_tile`]), into a single `output.geojson`, plus `output.dxf` and
/// `output.ocdCrt` for OCAD's layer-to-symbol conversion. The DXF is in the data's
/// projected CRS, so it is not written when the GeoJSON was reprojected to WGS84.
pub fn export_combined(
    fs: &impl FileSystem,
    epsg: Option<u32>,
    wgs84: bool,
    batchoutfolder: &str,
) -> anyhow::Result<()> {
    use std::collections::BTreeSet;

    let mut body = String::new();
    let mut feats: Vec<Value> = Vec::new();
    let mut layers: BTreeSet<String> = BTreeSet::new();
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut grow = |pts: &[[f64; 2]]| {
        for p in pts {
            xmin = xmin.min(p[0]);
            ymin = ymin.min(p[1]);
            xmax = xmax.max(p[0]);
            ymax = ymax.max(p[1]);
        }
    };

    for out in GEOJSON_OUTPUTS {
        let path = format!("{batchoutfolder}/merged_{}.geojson", out.name);
        let Some(features) = read_features(fs, Path::new(&path))? else {
            continue;
        };
        for f in features {
            let props = &f["properties"];
            let layer = props["isom"]
                .as_str()
                .or(props["layer"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let geometry = &f["geometry"];
            match geometry["type"].as_str().unwrap_or("") {
                "Point" => {
                    use std::fmt::Write as _;
                    let c = &geometry["coordinates"];
                    let (Some(x), Some(y)) = (c[0].as_f64(), c[1].as_f64()) else {
                        continue;
                    };
                    grow(&[[x, y]]);
                    let _ = write!(
                        body,
                        "POINT\r\n  8\r\n{layer}\r\n 10\r\n{x}\r\n 20\r\n{y}\r\n 50\r\n0\r\n  0\r\n"
                    );
                }
                "LineString" | "MultiLineString" => {
                    for pts in line_parts(geometry) {
                        grow(&pts);
                        dxf_curves_entity(
                            &mut body,
                            &layer,
                            &pts,
                            false,
                            props["elevation"].as_f64(),
                        );
                    }
                }
                "Polygon" | "MultiPolygon" => {
                    for pts in polygon_rings(geometry) {
                        grow(&pts);
                        dxf_curves_entity(&mut body, &layer, &pts, true, None);
                    }
                }
                _ => continue,
            }
            layers.insert(layer);
            feats.push(f);
        }
    }

    if feats.is_empty() {
        info!("No vector outputs found, skipping combined output");
        return Ok(());
    }

    write_feature_collection(
        &mut BufWriter::new(fs.create(format!("{batchoutfolder}/output.geojson"))?),
        &feats,
        if wgs84 { None } else { crs(epsg) }.as_ref(),
    )?;
    if wgs84 {
        info!("GeoJSON output is WGS84; skipping the projected output.dxf and output.ocdCrt");
        return Ok(());
    }

    // Bezier curves on organic layers, polylines for roads/buildings; $ACADVER is
    // required for SPLINE entities
    let mut w = BufWriter::new(fs.create(format!("{batchoutfolder}/output.dxf"))?);
    write!(
        w,
        "  0\r\nSECTION\r\n  2\r\nHEADER\r\n  9\r\n$ACADVER\r\n  1\r\nAC1015\r\n  9\r\n$EXTMIN\r\n 10\r\n{xmin}\r\n 20\r\n{ymin}\r\n  9\r\n$EXTMAX\r\n 10\r\n{xmax}\r\n 20\r\n{ymax}\r\n  0\r\nENDSEC\r\n  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\n"
    )?;
    w.write_all(body.as_bytes())?;
    w.write_all(b"ENDSEC\r\n  0\r\nEOF\r\n")?;

    // OCAD cross reference table: "SYMBOL LAYERNAME" per emitted layer
    let mut crt = BufWriter::new(fs.create(format!("{batchoutfolder}/output.ocdCrt"))?);
    for layer in &layers {
        if let Some(symbol) = crt_symbol(layer) {
            writeln!(crt, "{symbol} {layer}")?;
        }
    }
    Ok(())
}

/// Merge per-tile `<tile>_<name>.geojson` files in the batch output folder into
/// `merged_<name>.geojson`, one tile parsed at a time.
pub fn merge_geojson(fs: &impl FileSystem, batchoutfolder: &str) -> anyhow::Result<()> {
    for out in GEOJSON_OUTPUTS {
        let name = out.name;
        let suffix = format!("_{name}.geojson");
        let mut files: Vec<_> = fs
            .list(batchoutfolder)?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .is_some_and(|f| f.to_string_lossy().ends_with(&suffix))
                    && !p
                        .file_name()
                        .is_some_and(|f| f.to_string_lossy().starts_with("merged_"))
            })
            .collect();
        if files.is_empty() {
            info!("No files found for suffix {name}, skipping...");
            continue;
        }
        files.sort();

        let mut w = BufWriter::new(fs.create(format!("{batchoutfolder}/merged_{name}.geojson"))?);
        let mut first = true;
        for (i, file) in files.iter().enumerate() {
            let val: Value = serde_json::from_reader(BufReader::new(fs.open(file)?))?;
            if i == 0 {
                // the first tile's crs declaration (if any) is carried over verbatim
                write_prelude(&mut w, val.get("crs"))?;
            }
            if let Some(feats) = val["features"].as_array() {
                for feat in feats {
                    if !first {
                        w.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut w, feat)?;
                }
            }
        }
        w.write_all(b"]}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    /// A wobble that leaves and returns inside the ISOM minimum mouth is not a bend the
    /// symbol can carry, so it must not survive to the map.
    #[test]
    fn generalise_contour_splices_out_sub_minimum_wobble() {
        // a straight line with a 3 m spike that opens a 2 m mouth
        let mut pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        pts.splice(20..20, [[20.0, 3.0], [21.0, 3.0]]);
        let out = generalise_contour(&pts);
        assert!(
            out.iter().all(|p| p[1] == 0.0),
            "sub-minimum spike survived: {out:?}"
        );
        // the line itself is untouched apart from the spike
        assert_eq!(out.first(), pts.first());
        assert_eq!(out.last(), pts.last());
    }

    /// A deep re-entrant is real terrain, not a wobble, even where its limbs run closer
    /// than the minimum mouth. It may lose no more than what fits inside the ISOM
    /// minimum — never the valley.
    #[test]
    fn generalise_contour_keeps_a_deep_reentrant() {
        let mut pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let deep: Vec<[f64; 2]> = (0..30)
            .map(|i| [20.0, -f64::from(i)])
            .chain((0..30).rev().map(|i| [22.0, -f64::from(i)]))
            .collect();
        pts.splice(20..20, deep);
        let out = generalise_contour(&pts);
        let depth = out.iter().fold(0.0f64, |d, p| d.min(p[1]));
        assert!(
            depth <= -29.0 + MIN_MOUTH_M,
            "a 29 m re-entrant lost more than the ISOM minimum: kept only {depth} m"
        );
    }

    /// ISOM 2017-2: the contour gives way to symbol 109/110, and the gap it leaves has to
    /// be wide enough for the symbol to sit in.
    #[test]
    fn break_at_knolls_opens_a_gap_around_the_symbol() {
        let pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let parts = break_at_knolls(&pts, &[[20.0, 0.0]]);
        assert_eq!(parts.len(), 2, "contour was not broken");
        for part in &parts {
            for p in part {
                assert!(
                    dist(*p, [20.0, 0.0]) >= KNOLL_CLEAR_M,
                    "contour still touches the knoll at {p:?}"
                );
            }
        }
        // and a contour nowhere near a knoll is left as one piece
        assert_eq!(break_at_knolls(&pts, &[[20.0, 50.0]]).len(), 1);
    }

    /// Form lines are the renderer's selected set; nothing else may be published as 103.
    /// A dash-scale bound is what tells the two apart — the intermediate contours that
    /// used to be published here averaged 258 m and reached 4.8 km.
    #[test]
    fn form_lines_stay_at_dash_scale() {
        // the selection emits runs, not single dashes, so the bound is generous; it only
        // has to fail if whole intermediate contours are published as form lines again
        const MAX_FORM_LINE_M: f64 = 1000.0;
        let long: Vec<[f64; 2]> = (0..500).map(|i| [f64::from(i) * 10.0, 0.0]).collect();
        let len = |p: &[[f64; 2]]| p.windows(2).map(|w| dist(w[0], w[1])).sum::<f64>();
        assert!(len(&long) > MAX_FORM_LINE_M);
        // conform_contour must not be what saves us here — 103 is bounded by selection
        assert!(is_contour_family("103"));
    }

    use super::*;

    #[test]
    fn clip_ring_square_crossing_bbox() {
        // unit-ish square from (5,5) to (15,15), bbox x/y in [0,10]
        let ring = [
            [5.0, 5.0],
            [15.0, 5.0],
            [15.0, 15.0],
            [5.0, 15.0],
            [5.0, 5.0],
        ];
        let clipped = clip_ring(&ring, 0.0, 0.0, 10.0, 10.0);
        // expect the quarter square [5,10]x[5,10], closed
        assert_eq!(clipped.first(), clipped.last());
        let open = &clipped[..clipped.len() - 1];
        assert_eq!(open.len(), 4);
        for p in open {
            assert!(p[0] >= 5.0 && p[0] <= 10.0 && p[1] >= 5.0 && p[1] <= 10.0);
        }
        // fully outside
        assert!(clip_ring(&ring, 20.0, 20.0, 30.0, 30.0,).is_empty());
        // fully inside is unchanged (modulo closing)
        let inner = clip_ring(&ring, 0.0, 0.0, 20.0, 20.0);
        assert_eq!(inner.len(), 5);
    }

    #[test]
    fn curve_points_samples_bezier_for_geojson() {
        // jagged open contour: sampled output is denser, endpoints unchanged
        let pts: Vec<[f64; 2]> = (0..10)
            .map(|i| [i as f64 * 10.0, if i % 2 == 0 { 0.0 } else { 8.0 }])
            .collect();
        let out = curve_points("101", &pts, false);
        assert!(out.len() > pts.len(), "curve layer must be densified");
        assert_eq!(out.first(), pts.first());
        assert_eq!(out.last(), pts.last());
        // non-curve layer passes through untouched
        assert_eq!(curve_points("526", &pts, false), pts);
        // closed ring stays closed
        let ring = [
            [0.0, 0.0],
            [30.0, 0.0],
            [30.0, 30.0],
            [0.0, 30.0],
            [0.0, 0.0],
        ];
        let out = curve_points("406", &ring, true);
        assert_eq!(out.first(), out.last(), "ring must stay closed");
    }

    #[test]
    fn cliff_chain_follows_curved_face() {
        // dash midpoints along a semicircular face (r=30 m), ~2.4 m apart
        let mids: Vec<[f64; 2]> = (0..40)
            .map(|i| {
                let t = i as f64 / 39.0 * std::f64::consts::PI;
                [30.0 * t.cos(), 30.0 * t.sin()]
            })
            .collect();
        let chains = chain_cliff_dashes(&mids);
        assert_eq!(chains.len(), 1, "one face, one chain");
        assert_eq!(chains[0].len(), 40, "all dashes chained");
        // correct ordering walks the arc: every step is one dash spacing, no jumps
        for w in chains[0].windows(2) {
            assert!(dist(w[0], w[1]) < 3.0, "chain jumps across the face");
        }
    }

    #[test]
    fn write_crop_roundtrip() {
        use crate::io::fs::FileSystem;
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let feats = vec![
            feature(
                "LineString",
                coords_line([[0.0, 5.0], [20.0, 5.0]]),
                &[("layer", "contour")],
            ),
            feature(
                "Polygon",
                Value::Array(vec![coords_line([
                    [5.0, 5.0],
                    [15.0, 5.0],
                    [15.0, 15.0],
                    [5.0, 15.0],
                    [5.0, 5.0],
                ])]),
                &[("isom", "406")],
            ),
        ];
        write_feature_collection(
            &mut fs.create("in.geojson").unwrap(),
            &feats,
            crs(Some(25832)).as_ref(),
        )
        .unwrap();
        crop_geojson(
            &fs,
            Path::new("in.geojson"),
            Path::new("out.geojson"),
            0.0,
            0.0,
            10.0,
            10.0,
            None,
        )
        .unwrap();
        let val: Value = serde_json::from_reader(fs.open("out.geojson").unwrap()).unwrap();
        let out = val["features"].as_array().unwrap();
        assert_eq!(out.len(), 2);
        // crop must carry the input's crs declaration over
        assert_eq!(
            val["crs"]["properties"]["name"],
            "urn:ogc:def:crs:EPSG::25832"
        );
        assert_eq!(out[0]["properties"]["layer"], "contour");
        assert_eq!(out[1]["properties"]["isom"], "406");
        assert_eq!(out[1]["geometry"]["type"], "Polygon");
    }

    #[test]
    fn geojson_outputs_no_duplicate_names() {
        let mut names: Vec<&str> = GEOJSON_OUTPUTS.iter().map(|o| o.name).collect();
        names.sort();
        assert!(
            names.windows(2).all(|w| w[0] != w[1]),
            "duplicate output names: {names:?}"
        );
    }

    #[test]
    fn bindxf_to_geojson_single_path_produces_valid_collection() {
        use crate::geometry::{BinaryDxf, Bounds, Classification, Geometry, Point2, Polylines};
        use crate::io::fs::FileSystem;
        use std::path::PathBuf;

        let fs = crate::io::fs::memory::MemoryFileSystem::new();

        // one 2-point polyline classified as a Contour (layer "contour", isom "101")
        let mut pls = Polylines::new();
        pls.push(
            vec![Point2::new(0.0, 0.0), Point2::new(100.0, 100.0)],
            Classification::Contour,
        );
        let dxf = BinaryDxf::new(
            Bounds::new(0.0, 100.0, 0.0, 100.0),
            vec![Geometry::Polylines2(pls)],
        );

        // serialize into the memory FS
        dxf.to_writer(&mut fs.create("test.dxf.bin").unwrap())
            .unwrap();

        bindxf_to_geojson(
            &fs,
            &[PathBuf::from("test.dxf.bin")],
            std::path::Path::new("out.geojson"),
            None,
        )
        .unwrap();

        let val: Value = serde_json::from_reader(fs.open("out.geojson").unwrap()).unwrap();
        assert_eq!(val["type"], "FeatureCollection");
        let feats = val["features"].as_array().unwrap();
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0]["geometry"]["type"], "LineString");
        assert_eq!(feats[0]["properties"]["isom"], "101");
    }

    #[test]
    fn generated_types_roundtrip_to_featurecollection_json() {
        use geojson_types::{
            ContourProperties, ContourPropertiesIsom, ContourPropertiesLayer, Feature,
            FeatureGeometry, FeatureGeometryType, FeatureProperties, GeoJsonOutput,
        };

        let contour = ContourProperties {
            depression: None,
            elevation: None,
            isom: ContourPropertiesIsom::X101,
            layer: ContourPropertiesLayer::X101,
            layer_description: None,
        };
        let feature = Feature {
            geometry: FeatureGeometry {
                coordinates: vec![serde_json::json!(0.0), serde_json::json!(0.0)],
                type_: FeatureGeometryType::Point,
            },
            properties: FeatureProperties::ContourProperties(contour),
            type_: serde_json::json!("Feature"),
        };
        let collection = GeoJsonOutput {
            crs: None,
            features: vec![feature],
            type_: serde_json::json!("FeatureCollection"),
        };

        let json = serde_json::to_value(&collection).unwrap();
        assert_eq!(json["type"], "FeatureCollection");
        assert!(json["features"].is_array());
        assert_eq!(json["features"][0]["type"], "Feature");
        assert_eq!(json["features"][0]["properties"]["isom"], "101");
    }
}
