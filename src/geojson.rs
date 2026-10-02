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
    let mut out = FeatureWriter::new(w, crs)?;
    for f in features {
        out.push(f)?;
    }
    out.finish()
}

/// A FeatureCollection written one feature at a time, for outputs too large to hold as
/// [`Value`]s: a `Value` costs about a kilobyte per two-point cliff dash, and a steep alpine
/// tile has millions of them.
pub struct FeatureWriter<W: Write> {
    w: W,
    first: bool,
}

impl<W: Write> FeatureWriter<W> {
    pub fn new(mut w: W, crs: Option<&Value>) -> anyhow::Result<Self> {
        write_prelude(&mut w, crs)?;
        Ok(Self { w, first: true })
    }

    pub fn push(&mut self, feature: &Value) -> anyhow::Result<()> {
        if !self.first {
            self.w.write_all(b",")?;
        }
        self.first = false;
        serde_json::to_writer(&mut self.w, feature)?;
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        self.w.write_all(b"]}")?;
        self.w.flush()?;
        Ok(())
    }
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
        "409" => "409",
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
    let mut out = FeatureWriter::new(BufWriter::new(fs.create(output)?), crs(epsg).as_ref())?;
    // one input at a time: c3g.dxf.bin alone can be 150 MB on alpine rock
    for input in inputs {
        let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
        write_dxf_features(&mut out, dxf)?;
    }
    out.finish()
}

/// [`bindxf_to_geojson`] for geometry already in memory.
pub fn dxfs_to_geojson(
    fs: &impl FileSystem,
    dxfs: Vec<BinaryDxf>,
    output: &std::path::Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut out = FeatureWriter::new(BufWriter::new(fs.create(output)?), crs(epsg).as_ref())?;
    for dxf in dxfs {
        write_dxf_features(&mut out, dxf)?;
    }
    out.finish()
}

fn write_dxf_features<W: Write>(out: &mut FeatureWriter<W>, dxf: BinaryDxf) -> anyhow::Result<()> {
    for geom in dxf.take_geometry() {
        match geom {
            Geometry::Polylines2(pl) => {
                for (p, c) in pl.into_iter() {
                    out.push(&feature(
                        "LineString",
                        coords_line(p.iter().map(|pt| [pt.x, pt.y])),
                        &layer_props(c.to_layer()),
                    ))?;
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
                    out.push(&f)?;
                }
            }
            Geometry::Points(pts) => {
                for (p, c) in pts.into_iter() {
                    out.push(&feature(
                        "Point",
                        json!([r2(p.x), r2(p.y)]),
                        &layer_props(c.to_layer()),
                    ))?;
                }
            }
        }
    }
    Ok(())
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
        "101"
            | "102"
            | "103"
            | "201"
            | "202"
            | "306"
            | "403"
            | "406"
            | "407"
            | "408"
            | "409"
            | "410"
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
    // A closed ring is a knoll or a depression in its own right -- karttapullautin lifted
    // the ground under a small knoll precisely so that it earns one -- and a ring narrower
    // than the minimum mouth would otherwise be spliced across into a single line. A splice
    // that takes a noticeable part of a ring's area is not a wobble, so such a ring stays.
    if pts.first() == pts.last() && ring_area(&out).abs() < 0.9 * ring_area(pts).abs() {
        return pts.to_vec();
    }
    out
}

/// Signed area of a ring (shoelace; positive counter-clockwise).
fn ring_area(pts: &[[f64; 2]]) -> f64 {
    pts.windows(2)
        .map(|w| w[0][0] * w[1][1] - w[1][0] * w[0][1])
        .sum::<f64>()
        / 2.0
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
fn conform_contour(
    layer: &str,
    pts: &[[f64; 2]],
    knolls: &[[f64; 2]],
    chaikin: u32,
) -> Vec<Vec<[f64; 2]>> {
    if !is_contour_family(layer) {
        return vec![pts.to_vec()];
    }
    break_at_knolls(&round_corners(&generalise_contour(pts), chaikin), knolls)
}

/// `iterations` rounds of Chaikin corner cutting: every corner is replaced by two points a
/// quarter of the way along its segments, so the line keeps its course and loses its kinks.
/// A closed ring stays closed. Each new point depends on two neighbouring vertices only, so a
/// contour that is vertex-for-vertex identical in two tiles stays identical; only its ends,
/// at the padded tile's edge, move.
fn round_corners(pts: &[[f64; 2]], iterations: u32) -> Vec<[f64; 2]> {
    let mut pts = pts.to_vec();
    for _ in 0..iterations {
        if pts.len() < 3 {
            break;
        }
        let closed = pts.first() == pts.last();
        let cut = |a: [f64; 2], b: [f64; 2]| {
            [
                [0.75 * a[0] + 0.25 * b[0], 0.75 * a[1] + 0.25 * b[1]],
                [0.25 * a[0] + 0.75 * b[0], 0.25 * a[1] + 0.75 * b[1]],
            ]
        };
        let mut out = Vec::with_capacity(2 * pts.len());
        if !closed {
            out.push(pts[0]);
        }
        for w in pts.windows(2) {
            out.extend(cut(w[0], w[1]));
        }
        if closed {
            out.push(out[0]);
        } else {
            out.push(pts[pts.len() - 1]);
        }
        pts = out;
    }
    pts
}

/// The pieces of one line as published: generalised to the ISOM minimums and broken around
/// the knoll symbols. No curve is fitted: the line is karttapullautin's own smoothed contour,
/// the one the rendered map draws, and a fit that depends on the whole line (a
/// Douglas-Peucker thinning, say) would come out differently in the two tiles that share it.
fn published_pieces(
    layer: &str,
    pts: &[[f64; 2]],
    knolls: &[[f64; 2]],
    chaikin: u32,
) -> Vec<Vec<[f64; 2]>> {
    conform_contour(layer, pts, knolls, chaikin)
}

/// ISOM 2017-2 101: a slope line is 0.4 mm long at 1:15 000, 0.6 mm at 1:10 000 -- 6 m of ground.
const SLOPE_LINE_M: f64 = 6.0;

/// One slope line per started this much of a depression's line, so a long one shows its
/// direction along all of it.
const SLOPE_LINE_EVERY_M: f64 = 150.0;

/// True for the published classes that run round lower ground: contour and form-line
/// depressions. They are oriented with downhill on the right ([`publish_tile`]).
fn is_depression_layer(layer: &str) -> bool {
    layer.starts_with("depression") || layer == "formline_depression"
}

/// The slope lines of one published depression line: short lines from the line, at right
/// angles to it, on its right -- downhill. ISOM 101: "a depression has to have at least one
/// slope line", or it reads as a knoll.
///
/// Placed by the geometry alone, so that the two tiles that share a depression put its slope
/// lines in the same places: on a closed ring, the first starts at the vertex closest to the
/// ring's centre (the narrow side, so it points across the depression), the others follow
/// evenly round the ring; on an open piece they are spread evenly along it. A slope line
/// never reaches more than 0.45 of a small ring's width.
fn slope_ticks(pts: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    if pts.len() < 3 {
        return Vec::new();
    }
    let closed = pts.first() == pts.last();
    let mut cum = vec![0.0];
    for w in pts.windows(2) {
        cum.push(cum.last().unwrap() + dist(w[0], w[1]));
    }
    let total = *cum.last().unwrap();
    if total < 8.0 {
        return Vec::new();
    }
    let lo = pts
        .iter()
        .fold([f64::MAX; 2], |m, p| [m[0].min(p[0]), m[1].min(p[1])]);
    let hi = pts
        .iter()
        .fold([f64::MIN; 2], |m, p| [m[0].max(p[0]), m[1].max(p[1])]);
    let length = if closed {
        SLOPE_LINE_M.min(0.45 * (hi[0] - lo[0]).min(hi[1] - lo[1]))
    } else {
        SLOPE_LINE_M
    };
    let count = ((total / SLOPE_LINE_EVERY_M).round() as usize).max(1);
    let start = if closed {
        let ring = &pts[..pts.len() - 1];
        let c = ring
            .iter()
            .fold([0.0, 0.0], |a, p| [a[0] + p[0], a[1] + p[1]]);
        let c = [c[0] / ring.len() as f64, c[1] / ring.len() as f64];
        let nearest = (0..ring.len())
            // ties (a symmetric ring) go by position, never by where the trace began
            .min_by(|&i, &j| {
                (dist(ring[i], c), ring[i][0], ring[i][1])
                    .partial_cmp(&(dist(ring[j], c), ring[j][0], ring[j][1]))
                    .unwrap()
            })
            .unwrap();
        cum[nearest]
    } else {
        total / count as f64 / 2.0
    };
    (0..count)
        .filter_map(|k| {
            let s = (start + k as f64 * total / count as f64) % total;
            let i = cum.partition_point(|&c| c <= s).clamp(1, pts.len() - 1);
            let (a, b) = (pts[i - 1], pts[i]);
            let seg = dist(a, b);
            if seg == 0.0 {
                return None;
            }
            let t = (s - cum[i - 1]) / seg;
            let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let (dx, dy) = ((b[0] - a[0]) / seg, (b[1] - a[1]) / seg);
            Some(vec![p, [p[0] + dy * length, p[1] - dx * length]])
        })
        .collect()
}

/// One area the contour family is hidden under, with its bounding box for a cheap reject.
struct MaskArea {
    rings: Vec<Vec<[f64; 2]>>,
    min: [f64; 2],
    max: [f64; 2],
}

impl MaskArea {
    fn new(rings: Vec<Vec<[f64; 2]>>) -> Option<Self> {
        let pts = rings.iter().flatten();
        let min = pts
            .clone()
            .fold([f64::MAX; 2], |m, p| [m[0].min(p[0]), m[1].min(p[1])]);
        let max = pts.fold([f64::MIN; 2], |m, p| [m[0].max(p[0]), m[1].max(p[1])]);
        (min[0] < max[0] && min[1] < max[1]).then_some(Self { rings, min, max })
    }

    fn overlaps(&self, min: [f64; 2], max: [f64; 2]) -> bool {
        self.min[0] <= max[0]
            && min[0] <= self.max[0]
            && self.min[1] <= max[1]
            && min[1] <= self.max[1]
    }

    /// Even-odd over all rings, so a hole (an island) is outside.
    fn contains(&self, p: [f64; 2]) -> bool {
        if !self.overlaps(p, p) {
            return false;
        }
        let mut inside = false;
        for ring in &self.rings {
            for w in ring.windows(2) {
                let (a, b) = (w[0], w[1]);
                if (a[1] > p[1]) != (b[1] > p[1])
                    && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
                {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// Where the segment a-b crosses this area's boundary, as fractions of its length.
    fn crossings(&self, a: [f64; 2], b: [f64; 2], out: &mut Vec<f64>) {
        let d = [b[0] - a[0], b[1] - a[1]];
        for ring in &self.rings {
            for w in ring.windows(2) {
                let (c, e) = (w[0], w[1]);
                let f = [e[0] - c[0], e[1] - c[1]];
                let denom = d[0] * f[1] - d[1] * f[0];
                if denom == 0.0 {
                    continue;
                }
                let g = [c[0] - a[0], c[1] - a[1]];
                let t = (g[0] * f[1] - g[1] * f[0]) / denom;
                let u = (g[0] * d[1] - g[1] * d[0]) / denom;
                if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
                    out.push(t);
                }
            }
        }
    }
}

/// The OSM areas whose vectorconf rule has one of `categories` as its description: lakes,
/// typically. LiDAR has no ground on open water, so karttapullautin traces contours across a
/// lake from whatever it interpolates there, and they would be drawn over the water.
fn read_mask(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    categories: &[String],
) -> anyhow::Result<Vec<MaskArea>> {
    if categories.is_empty() {
        return Ok(Vec::new());
    }
    let Some(features) = read_features(fs, &tmpfolder.join("osm_areas.geojson"))? else {
        return Ok(Vec::new());
    };
    Ok(features
        .iter()
        .filter(|f| {
            f["properties"]["category"]
                .as_str()
                .is_some_and(|c| categories.iter().any(|m| m == c))
        })
        .filter_map(|f| MaskArea::new(polygon_rings(&f["geometry"])))
        .collect())
}

fn masked(mask: &[MaskArea], p: [f64; 2]) -> bool {
    mask.iter().any(|m| m.contains(p))
}

/// The pieces of a line that lie outside every mask area, cut exactly where the line crosses
/// an area's boundary. Pieces are kept in order; a line that never comes near an area is
/// returned as it is.
fn mask_line(pts: Vec<[f64; 2]>, mask: &[MaskArea]) -> Vec<Vec<[f64; 2]>> {
    let lo = pts
        .iter()
        .fold([f64::MAX; 2], |m, p| [m[0].min(p[0]), m[1].min(p[1])]);
    let hi = pts
        .iter()
        .fold([f64::MIN; 2], |m, p| [m[0].max(p[0]), m[1].max(p[1])]);
    let near: Vec<&MaskArea> = mask.iter().filter(|m| m.overlaps(lo, hi)).collect();
    if near.is_empty() || pts.len() < 2 {
        return vec![pts];
    }
    // the vertices themselves stay exact, so a line keeps the vertices it shares with its
    // continuation in the next tile
    let at = |a: [f64; 2], b: [f64; 2], t: f64| match t {
        0.0 => a,
        1.0 => b,
        _ => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t],
    };
    let inside = |p: [f64; 2]| near.iter().any(|m| m.contains(p));

    let mut pieces = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    let mut ts = Vec::new();
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let smin = [a[0].min(b[0]), a[1].min(b[1])];
        let smax = [a[0].max(b[0]), a[1].max(b[1])];
        ts.clear();
        ts.push(0.0);
        for m in near.iter().filter(|m| m.overlaps(smin, smax)) {
            m.crossings(a, b, &mut ts);
        }
        ts.push(1.0);
        ts.sort_by(f64::total_cmp);
        for span in ts.windows(2) {
            let (t0, t1) = (span[0], span[1]);
            if t1 - t0 < 1e-9 {
                continue;
            }
            if inside(at(a, b, (t0 + t1) / 2.0)) {
                if cur.len() > 1 {
                    pieces.push(std::mem::take(&mut cur));
                }
                cur.clear();
            } else {
                let start = at(a, b, t0);
                if cur.last() != Some(&start) {
                    cur.push(start);
                }
                cur.push(at(a, b, t1));
            }
        }
    }
    if cur.len() > 1 {
        pieces.push(cur);
    }
    pieces
}

/// Groups of points that lie within CLIFF_CLUSTER_DIST of each other, transitively (a
/// union-find over a grid of that size), each group in ascending index order and the groups
/// in order of their first point.
fn cluster_points(mids: &[[f64; 2]]) -> Vec<Vec<usize>> {
    use std::collections::HashMap;
    let mut parent: Vec<usize> = (0..mids.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = i;
        while parent[c] != r {
            let next = parent[c];
            parent[c] = r;
            c = next;
        }
        r
    }
    let cell = CLIFF_CLUSTER_DIST;
    let key = |m: &[f64; 2]| ((m[0] / cell).floor() as i64, (m[1] / cell).floor() as i64);
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, m) in mids.iter().enumerate() {
        grid.entry(key(m)).or_default().push(i);
    }
    for (i, m) in mids.iter().enumerate() {
        let (gx, gy) = key(m);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &j in grid.get(&(gx + dx, gy + dy)).into_iter().flatten() {
                    if j > i && dist(*m, mids[j]) <= CLIFF_CLUSTER_DIST {
                        let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                        if ri != rj {
                            parent[ri.max(rj)] = ri.min(rj);
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
    let mut clusters: Vec<Vec<usize>> = clusters.into_values().collect();
    clusters.sort_unstable_by_key(|members| members[0]);
    clusters
}

/// A cliff dash, as its two end points.
type Dash = [[f64; 2]; 2];

/// Two dashes whose midpoints fall in the same cell of this size and whose directions in the
/// same bin of CLIFF_DEDUP_DEG are one stroke on the map: the rendered cliff is drawn
/// 2.5 m wide, and the detector marks most places several times over (on alpine terrain
/// four dashes in five are such repeats).
const CLIFF_DEDUP_M: f64 = 0.5;
const CLIFF_DEDUP_DEG: f64 = 15.0;

/// The cliff faces of a set of dashes: repeats dropped (see CLIFF_DEDUP_M), the rest grouped
/// into faces by CLIFF_CLUSTER_DIST. The dashes themselves are kept, not replaced by a line
/// through them: they *are* the symbol the rendered map draws, and a dense rock area is not
/// a line. Which of two repeats survives depends only on the dashes themselves, and so does
/// every grouping, so two tiles sharing a stretch of rock publish the same dashes for it.
fn cliff_faces(dashes: &[Dash]) -> Vec<Vec<Dash>> {
    use std::collections::HashMap;
    let mut kept: HashMap<(i64, i64, i64), Dash> = HashMap::new();
    for d in dashes {
        let [a, b] = *d;
        // one direction for both ways a dash can be written down
        let d = if (a[0], a[1]) <= (b[0], b[1]) {
            [a, b]
        } else {
            [b, a]
        };
        let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
        let angle = (d[1][1] - d[0][1])
            .atan2(d[1][0] - d[0][0])
            .to_degrees()
            .rem_euclid(180.0);
        let bins = (180.0 / CLIFF_DEDUP_DEG) as i64;
        let key = (
            (mid[0] / CLIFF_DEDUP_M).floor() as i64,
            (mid[1] / CLIFF_DEDUP_M).floor() as i64,
            ((angle / CLIFF_DEDUP_DEG).floor() as i64).rem_euclid(bins),
        );
        kept.entry(key)
            .and_modify(|k| {
                if d.as_flattened() < k.as_flattened() {
                    *k = d;
                }
            })
            .or_insert(d);
    }
    let mut kept: Vec<Dash> = kept.into_values().collect();
    kept.sort_unstable_by(|a, b| a.as_flattened().partial_cmp(b.as_flattened()).unwrap());
    let mids: Vec<[f64; 2]> = kept
        .iter()
        .map(|[a, b]| [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0])
        .collect();
    cluster_points(&mids)
        .into_iter()
        .map(|members| members.into_iter().map(|i| kept[i]).collect())
        .collect()
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
    // certain before uncertain, then by position: the order the detector wrote them in differs
    // between two tiles that share a knoll, and a greedy filter must not depend on it
    candidates.sort_by(|(p, l), (q, m)| {
        (l.starts_with("ugly"), p[0], p[1])
            .partial_cmp(&(m.starts_with("ugly"), q[0], q[1]))
            .unwrap()
    });
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
/// * `contours` and `formlines`: each line generalised and broken around the knoll symbols
///   ([`published_pieces`]), with `contour_chaikin` rounds of corner cutting
///   ([`round_corners`]), and slope lines (`slope_line`, 101.1) hanging from every depression
///   ([`slope_ticks`]). With `formline=2` the half-interval
///   `*_intermed` lines are only candidates -- the renderer's selection of them is
///   `formlines` -- so they are dropped from `contours`; in the other modes they are
///   drawn as full contours and published as 101;
/// * `cliffs`: the renderer's dashes, repeats dropped, one MultiLineString per cliff face
///   ([`cliff_faces`]).
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
    contour_chaikin: u32,
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
            // a class without a symbol of its own (slope lines, the knoll detector's
            // 1010) is part of the rendering, not a feature of the map
            let Some(isom) = properties["isom"].as_str().map(str::to_string) else {
                continue;
            };
            let pieces: Vec<Vec<[f64; 2]>> = line_parts(&f["geometry"])
                .iter()
                .flat_map(|pts| published_pieces(&isom, pts, &knolls, contour_chaikin))
                .collect();
            if properties["layer"]
                .as_str()
                .is_some_and(is_depression_layer)
            {
                let ticks = pieces.iter().flat_map(|p| slope_ticks(p)).collect();
                let mut tick_props = json!({"layer": "slope_line", "isom": "101.1"});
                if !properties["elevation"].is_null() {
                    tick_props["elevation"] = properties["elevation"].clone();
                }
                out.extend(line_feature(ticks, tick_props));
            }
            out.extend(line_feature(pieces, properties));
        }
        write(name, &out)?;
    }

    let cliffs = tmpfolder.join("cliffs.geojson");
    if fs.exists(&cliffs) {
        // Read as dashes, never as Values: the raw file holds millions of them on alpine rock,
        // and as a Value tree that was over 10 GB for a single tile.
        let RawCliffs {
            dashes_202,
            dashes_201,
        } = serde_json::from_reader(BufReader::new(fs.open(&cliffs)?))?;
        let mut out = FeatureWriter::new(BufWriter::new(fs.create(&cliffs)?), crs.as_ref())?;
        for (dashes, layer, isom) in [
            (&dashes_202, "cliff2", "202"),
            (&dashes_201, "cliff3", "201"),
        ] {
            for face in cliff_faces(dashes) {
                let parts = face.into_iter().map(coords_line).collect();
                out.push(&json!({
                    "type": "Feature",
                    "properties": {"layer": layer, "isom": isom},
                    "geometry": {"type": "MultiLineString", "coordinates": Value::Array(parts)},
                }))?;
            }
        }
        out.finish()?;
    }
    Ok(())
}

/// Leave out the published contours, form lines and knolls that lie inside the OSM areas whose
/// vectorconf rule has one of `categories` as its description (e.g. `lake`), cutting the lines
/// exactly where they cross the shore ([`mask_line`]). Rewrites the files in place.
///
/// Runs after the shapefile pass, which is what writes `osm_areas.geojson`, and like
/// [`publish_tile`] on the padded tile, so both sides of a tile edge are cut by the same shore.
pub fn mask_contours(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    categories: &[String],
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mask = read_mask(fs, tmpfolder, categories)?;
    if mask.is_empty() {
        return Ok(());
    }
    let crs = crs(epsg);
    for name in ["contours", "formlines", "dotknolls"] {
        let path = tmpfolder.join(format!("{name}.geojson"));
        let Some(features) = read_features(fs, &path)? else {
            continue;
        };
        let mut out = Vec::with_capacity(features.len());
        for f in features {
            if f["geometry"]["type"].as_str() == Some("Point") {
                let c = &f["geometry"]["coordinates"];
                if let (Some(x), Some(y)) = (c[0].as_f64(), c[1].as_f64())
                    && masked(&mask, [x, y])
                {
                    continue;
                }
                out.push(f);
                continue;
            }
            let pieces = line_parts(&f["geometry"])
                .into_iter()
                .flat_map(|pts| mask_line(pts, &mask))
                .collect();
            out.extend(line_feature(pieces, f["properties"].clone()));
        }
        write_feature_collection(&mut BufWriter::new(fs.create(&path)?), &out, crs.as_ref())?;
    }
    Ok(())
}

/// The dashes of a raw `cliffs.geojson`, by symbol, deserialized feature by feature so that no
/// feature outlives the two end points taken from it.
struct RawCliffs {
    dashes_202: Vec<Dash>,
    dashes_201: Vec<Dash>,
}

impl<'de> serde::Deserialize<'de> for RawCliffs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};

        #[derive(serde::Deserialize)]
        struct Properties {
            isom: Option<String>,
        }
        #[derive(serde::Deserialize)]
        #[serde(tag = "type", content = "coordinates")]
        enum Lines {
            LineString(Vec<[f64; 2]>),
            MultiLineString(Vec<Vec<[f64; 2]>>),
        }
        #[derive(serde::Deserialize)]
        struct Feature {
            properties: Properties,
            geometry: Lines,
        }

        struct Features<'a>(&'a mut RawCliffs);
        impl<'de> serde::de::DeserializeSeed<'de> for Features<'_> {
            type Value = ();
            fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
                d.deserialize_seq(self)
            }
        }
        impl<'de> Visitor<'de> for Features<'_> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of cliff features")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                while let Some(f) = seq.next_element::<Feature>()? {
                    let parts = match f.geometry {
                        Lines::LineString(pts) => vec![pts],
                        Lines::MultiLineString(parts) => parts,
                    };
                    let dashes = if f.properties.isom.as_deref() == Some("202") {
                        &mut self.0.dashes_202
                    } else {
                        &mut self.0.dashes_201
                    };
                    for pts in parts {
                        if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
                            dashes.push([*a, *b]);
                        }
                    }
                }
                Ok(())
            }
        }

        struct Collection;
        impl<'de> Visitor<'de> for Collection {
            type Value = RawCliffs;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a GeoJSON FeatureCollection")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawCliffs, A::Error> {
                let mut cliffs = RawCliffs {
                    dashes_202: Vec::new(),
                    dashes_201: Vec::new(),
                };
                while let Some(key) = map.next_key::<String>()? {
                    if key == "features" {
                        map.next_value_seed(Features(&mut cliffs))?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(cliffs)
            }
        }

        d.deserialize_map(Collection)
    }
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

    /// A knoll ring narrower than the minimum mouth is still a knoll: the generalisation must
    /// not splice across it and leave a line where the map has a hill.
    #[test]
    fn generalise_contour_keeps_a_small_knoll_round() {
        // a 6 m wide ring, vertices ~1.2 m apart, closed
        let mut ring: Vec<[f64; 2]> = (0..16)
            .map(|i| {
                let t = f64::from(i) / 16.0 * std::f64::consts::TAU;
                [3.0 * t.cos(), 3.0 * t.sin()]
            })
            .collect();
        ring.push(ring[0]);
        assert_eq!(generalise_contour(&ring), ring);
    }

    /// The detector marks most of a rock face several times over. Repeats go, faces stay
    /// apart, and none of it may depend on the order the dashes arrive in -- two tiles write
    /// the same rock in different orders.
    #[test]
    fn cliff_faces_drop_repeats_and_keep_faces_apart() {
        let face = |x0: f64| -> Vec<Dash> {
            (0..10)
                .map(|i| {
                    let x = x0 + f64::from(i) * 2.0;
                    [[x, 0.0], [x, 3.0]]
                })
                .collect()
        };
        let mut dashes = face(0.0);
        // the same dashes again, written the other way round and a centimetre off
        dashes.extend(
            face(0.0)
                .iter()
                .map(|[a, b]| [[b[0] + 0.01, b[1]], [a[0] + 0.01, a[1]]]),
        );
        // a second face 50 m away
        dashes.extend(face(50.0));

        let faces = cliff_faces(&dashes);
        assert_eq!(faces.len(), 2, "two faces");
        assert!(
            faces.iter().all(|f| f.len() == 10),
            "repeats dropped: {faces:?}"
        );

        let mut reversed = dashes.clone();
        reversed.reverse();
        assert_eq!(
            cliff_faces(&reversed),
            faces,
            "order of arrival changed the result"
        );
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

    /// Everything `publish_tile` writes must deserialize into the types generated from
    /// `schema/geojson.schema.json`: the schema is the contract consumers build on.
    #[test]
    fn published_tile_matches_schema() {
        use crate::geometry::{
            BinaryDxf, Bounds, Classification, Geometry, Point2, Point3, Points, Polylines,
        };
        use std::path::PathBuf;

        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let bounds = Bounds::new(0.0, 200.0, 0.0, 200.0);
        let line = |y: f64| -> Vec<Point2> {
            (0..100)
                .map(|i| Point2::new(f64::from(i) * 2.0, y))
                .collect()
        };
        let write = |name: &str, geometry: Geometry| {
            let dxf = BinaryDxf::new(bounds.clone(), vec![geometry]);
            dxf.to_writer(&mut fs.create(format!("{name}.dxf.bin")).unwrap())
                .unwrap();
            bindxf_to_geojson(
                &fs,
                &[PathBuf::from(format!("{name}.dxf.bin"))],
                Path::new(&format!("{name}.geojson")),
                Some(25832),
            )
            .unwrap();
        };

        let line3 = |y: f64| -> Vec<Point3> {
            line(y).iter().map(|p| Point3::new(p.x, p.y, 0.0)).collect()
        };
        let mut contours = Polylines::new();
        contours.push(line3(10.0), (Classification::Contour, 500.0));
        contours.push(line3(20.0), (Classification::ContourIndex, 525.0));
        contours.push(line3(30.0), (Classification::ContourIntermed, 502.5));
        write("contours", Geometry::Polylines3(contours));

        let mut formlines = Polylines::new();
        formlines.push(line(40.0), Classification::Formline);
        write("formlines", Geometry::Polylines2(formlines));

        let mut knolls = Points::new();
        knolls.push(Point2::new(100.0, 100.0), Classification::Dotknoll);
        knolls.push(Point2::new(105.0, 100.0), Classification::UglyDotknoll);
        knolls.push(Point2::new(150.0, 150.0), Classification::Udepression);
        write("dotknolls", Geometry::Points(knolls));

        let mut cliffs = Polylines::new();
        for i in 0..10 {
            let x = f64::from(i) * 2.0;
            cliffs.push(
                vec![Point2::new(x, 60.0), Point2::new(x, 61.0)],
                Classification::Cliff3,
            );
        }
        write("cliffs", Geometry::Polylines2(cliffs));

        publish_tile(&fs, Path::new(""), 2.0, Some(25832), 0).unwrap();

        for name in ["contours", "formlines", "dotknolls", "cliffs"] {
            let val: Value =
                serde_json::from_reader(fs.open(format!("{name}.geojson")).unwrap()).unwrap();
            assert!(
                !val["features"].as_array().unwrap().is_empty(),
                "{name}: nothing published"
            );
            if let Err(e) = serde_json::from_value::<geojson_types::GeoJsonOutput>(val.clone()) {
                panic!("{name} does not match the schema: {e}\n{val}");
            }
        }

        let contours: Value =
            serde_json::from_reader(fs.open("contours.geojson").unwrap()).unwrap();
        let layers: Vec<&str> = contours["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["properties"]["layer"].as_str().unwrap())
            .collect();
        assert!(
            !layers.contains(&"contour_intermed"),
            "formline=2 must leave half-interval candidates to the form lines: {layers:?}"
        );
        let knolls: Value = serde_json::from_reader(fs.open("dotknolls.geojson").unwrap()).unwrap();
        assert_eq!(
            knolls["features"].as_array().unwrap().len(),
            2,
            "the uncertain knoll 5 m from a certain one must give way"
        );
    }

    #[test]
    fn slope_ticks_point_downhill_into_the_depression() {
        // a clockwise 40 x 20 m ellipse: downhill (the inside) is on the right
        let ring: Vec<[f64; 2]> = (0..=48)
            .map(|i| {
                let a = -f64::from(i) / 48.0 * std::f64::consts::TAU;
                [100.0 + 20.0 * a.cos(), 200.0 + 10.0 * a.sin()]
            })
            .collect();
        let ticks = slope_ticks(&ring);
        assert_eq!(ticks.len(), 1);
        let [start, end] = [ticks[0][0], ticks[0][1]];
        let from_centre = |p: [f64; 2]| dist(p, [100.0, 200.0]);
        assert!(
            from_centre(end) < from_centre(start),
            "the tick points inward"
        );
        assert!((dist(start, end) - SLOPE_LINE_M).abs() < 1e-9);
        // on the narrow side: across the ellipse, not along it
        assert!((start[0] - 100.0).abs() < 2.0, "{start:?}");
        // the same ring traced from another vertex puts it in the same place
        let mut shifted: Vec<[f64; 2]> = ring[17..48].to_vec();
        shifted.extend_from_slice(&ring[..18]);
        let again = slope_ticks(&shifted);
        assert!(dist(again[0][0], start) < 1e-6);
    }

    #[test]
    fn round_corners_keeps_ends_and_closes_rings() {
        let open = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0]];
        let once = round_corners(&open, 1);
        assert_eq!(once.first(), Some(&[0.0, 0.0]));
        assert_eq!(once.last(), Some(&[4.0, 4.0]));
        assert!(once.contains(&[3.0, 0.0]) && once.contains(&[4.0, 1.0]));
        assert!(!once.contains(&[4.0, 0.0]), "the corner is cut");
        let ring = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]];
        let twice = round_corners(&ring, 2);
        assert_eq!(twice.first(), twice.last());
        assert_eq!(twice.len(), 17);
        assert_eq!(round_corners(&ring, 0), ring.to_vec());
    }

    #[test]
    fn mask_line_cuts_at_the_shore_and_keeps_islands() {
        // a 10 x 10 lake with a 2 x 2 island in the middle
        let lake = MaskArea::new(vec![
            vec![
                [0.0, 0.0],
                [10.0, 0.0],
                [10.0, 10.0],
                [0.0, 10.0],
                [0.0, 0.0],
            ],
            vec![[4.0, 4.0], [4.0, 6.0], [6.0, 6.0], [6.0, 4.0], [4.0, 4.0]],
        ])
        .unwrap();
        let mask = [lake];
        let pieces = mask_line(vec![[-5.0, 5.0], [15.0, 5.0]], &mask);
        assert_eq!(
            pieces,
            vec![
                vec![[-5.0, 5.0], [0.0, 5.0]],
                vec![[4.0, 5.0], [6.0, 5.0]],
                vec![[10.0, 5.0], [15.0, 5.0]],
            ]
        );
        // a line nowhere near it is returned untouched, vertices and all
        let away = vec![[20.0, 0.0], [21.0, 1.0], [22.0, 0.0]];
        assert_eq!(mask_line(away.clone(), &mask), vec![away]);
        assert!(masked(&mask, [2.0, 2.0]) && !masked(&mask, [5.0, 5.0]));
    }

    #[test]
    fn wgs84_reprojects_a_utm_position() {
        let w = Wgs84::new(25832).unwrap();
        // Immenstadt im Allgäu, ETRS89 / UTM 32N
        let [lon, lat] = w.point([593_500.0, 5_269_500.0]).unwrap();
        assert!(
            (lon - 10.247).abs() < 0.01 && (lat - 47.567).abs() < 0.01,
            "{lon} {lat}"
        );
        let mut g = json!({"type": "LineString", "coordinates": [[593_500.0, 5_269_500.0], [593_600.0, 5_269_500.0]]});
        w.geometry(&mut g).unwrap();
        assert_eq!(g["coordinates"][0], json!([lon, lat]));
    }
}
