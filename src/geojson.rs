//! GeoJSON output for vector features (vegetation polygons, OSM features, contours),
//! plus bbox cropping and batch merging of the produced files.
//!
//! Coordinates are written in the native projected CRS of the input data.

use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use log::info;
use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Classification, Geometry};
use crate::io::fs::FileSystem;

/// Suffixes of per-tile GeoJSON files that batch mode crops and merges.
pub const GEOJSON_NAMES: &[&str] = &[
    "contours",
    "vegetation",
    "yellow",
    "undergrowth",
    "osm_lines",
    "osm_areas",
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

/// Convert a binary DXF file (contours, cliffs, knolls...) to GeoJSON. Polylines become
/// LineStrings with `layer` and (when known) `isom` properties, points become Points.
pub fn bindxf_to_geojson(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
    let mut feats = Vec::new();
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

/// Crop all features of a GeoJSON file to the bbox and write the result.
#[allow(clippy::too_many_arguments)]
pub fn crop_geojson(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
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
            out.push(nf);
        }
    }
    // the input's crs declaration (if any) is carried over verbatim
    write_feature_collection(
        &mut BufWriter::new(fs.create(output)?),
        &out,
        val.get("crs"),
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
    if closed && p.first() != p.last() {
        if let Some(f) = p.first().copied() {
            p.push(f);
        }
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

    let mut chains = Vec::new();
    for members in clusters.values() {
        // start from the point farthest from the cluster centroid
        let n = members.len() as f64;
        let cx = members.iter().map(|&i| mids[i][0]).sum::<f64>() / n;
        let cy = members.iter().map(|&i| mids[i][1]).sum::<f64>() / n;
        let mut rest: Vec<usize> = members.clone();
        let start_pos = rest
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                dist(mids[**a], [cx, cy])
                    .partial_cmp(&dist(mids[**b], [cx, cy]))
                    .unwrap()
            })
            .map(|(p, _)| p)
            .unwrap();
        let mut path = vec![mids[rest.swap_remove(start_pos)]];
        while !rest.is_empty() {
            let last = *path.last().unwrap();
            let next_pos = rest
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    dist(mids[**a], last)
                        .partial_cmp(&dist(mids[**b], last))
                        .unwrap()
                })
                .map(|(p, _)| p)
                .unwrap();
            path.push(mids[rest.swap_remove(next_pos)]);
        }
        // 2-opt on the open path: reversing path[i+1..=j] swaps edges (i,i+1)/(j,j+1)
        // for (i,j)/(i+1,j+1); when j is the last point only edge (i,i+1) is replaced.
        // Clusters are a handful of dashes, so O(n²) passes are cheap.
        let mut improved = true;
        while improved {
            improved = false;
            let n = path.len();
            for i in 0..n.saturating_sub(2) {
                for j in i + 2..n {
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
        }
        let len: f64 = path.windows(2).map(|w| dist(w[0], w[1])).sum();
        // single-dash clusters have zero path length; use the dash length itself
        if len.max(2.9) >= CLIFF_MIN_LEN_M {
            chains.push(path);
        }
    }
    chains
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

/// Combine every merged vector output into single-file `output.dxf` and
/// `output.geojson`, plus `output.ocdCrt` for OCAD's layer-to-symbol conversion.
pub fn export_combined(
    fs: &impl FileSystem,
    epsg: Option<u32>,
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

    // contours, cliffs, formlines, dotknolls from the merged binary DXF (if present).
    // Upstream bindxfmerge writes merged.dxf.bin to the working directory, not the
    // output folder, so look in both.
    let merged_bin = [
        format!("{batchoutfolder}/merged.dxf.bin"),
        "merged.dxf.bin".into(),
    ]
    .into_iter()
    .find(|p| fs.exists(p));
    let have_merged_bin = merged_bin.is_some();
    let mut cliff_mids_202: Vec<[f64; 2]> = Vec::new();
    let mut cliff_mids_201: Vec<[f64; 2]> = Vec::new();
    let mut brown_points: Vec<([f64; 2], Classification)> = Vec::new();
    if let Some(merged_bin) = &merged_bin {
        let dxf = BinaryDxf::from_reader(&mut fs.open(merged_bin)?)?;
        for geom in dxf.take_geometry() {
            match geom {
                Geometry::Polylines2(pl) => {
                    for (p, c) in pl.into_iter() {
                        // internal knoll-detector artifact, not a map symbol
                        if matches!(c, Classification::Knoll1010) {
                            continue;
                        }
                        let mut pts: Vec<[f64; 2]> = p.iter().map(|pt| [pt.x, pt.y]).collect();
                        let is_cliff = matches!(
                            c,
                            Classification::Cliff2
                                | Classification::Cliff3
                                | Classification::Cliff4
                        );
                        if is_cliff {
                            // collect dash midpoints; chained into cliff lines below
                            if !pts.is_empty() {
                                let mid = [
                                    (pts[0][0] + pts[pts.len() - 1][0]) / 2.0,
                                    (pts[0][1] + pts[pts.len() - 1][1]) / 2.0,
                                ];
                                if matches!(c, Classification::Cliff2) {
                                    cliff_mids_202.push(mid);
                                } else {
                                    cliff_mids_201.push(mid);
                                }
                            }
                            continue;
                        }
                        // one Chaikin pass takes the segment jitter out of contour-family
                        // lines (OCAD can additionally fit Bezier curves on import)
                        let smooth = c.is_contour()
                            || c.is_depression()
                            || matches!(
                                c,
                                Classification::Formline
                                    | Classification::FormlineDepression
                                    | Classification::ContourSimple
                            );
                        if smooth && pts.len() > 3 {
                            let as_p2: Vec<crate::geometry::Point2> = pts
                                .iter()
                                .map(|q| crate::geometry::Point2::new(q[0], q[1]))
                                .collect();
                            let closed = pts.first() == pts.last();
                            let sm = if closed {
                                let mut r =
                                    crate::vege_vector::chaikin_closed(&as_p2[..as_p2.len() - 1]);
                                if let Some(f) = r.first().cloned() {
                                    r.push(f);
                                }
                                r
                            } else {
                                crate::vege_vector::chaikin_open(&as_p2)
                            };
                            pts = sm.iter().map(|q| [q.x, q.y]).collect();
                        }
                        let layer = layer_isom(c.to_layer()).unwrap_or(c.to_layer()).to_string();
                        grow(&pts);
                        dxf_curves_entity(&mut body, &layer, &pts, c.is_area(), None);
                        feats.push(feature(
                            "LineString",
                            coords_line(curve_points(&layer, &pts, c.is_area())),
                            &layer_props(c.to_layer()),
                        ));
                        layers.insert(layer);
                    }
                }
                Geometry::Polylines3(pl) => {
                    for (p, (c, h)) in pl.into_iter() {
                        let layer = layer_isom(c.to_layer()).unwrap_or(c.to_layer()).to_string();
                        let pts: Vec<[f64; 2]> = p.iter().map(|pt| [pt.x, pt.y]).collect();
                        grow(&pts);
                        dxf_curves_entity(&mut body, &layer, &pts, false, Some(h));
                        let mut f = feature(
                            "LineString",
                            coords_line(curve_points(&layer, &pts, false)),
                            &layer_props(c.to_layer()),
                        );
                        f["properties"]["elevation"] = json!(h);
                        feats.push(f);
                        layers.insert(layer);
                    }
                }
                Geometry::Points(points) => {
                    for (p, c) in points.into_iter() {
                        if matches!(c, Classification::Knoll1010) {
                            continue;
                        }
                        brown_points.push(([p.x, p.y], c));
                    }
                }
            }
        }
    }

    // knoll/depression point symbols: ISOM 109/110/111 must not touch or overlap each
    // other (12 m footprint length). Greedy spacing filter over points ranked by
    // certainty: the detector's definite symbols (dotknoll/udepression) win over the
    // uncertain "ugly" variants when two candidates are closer than the minimum.
    {
        use std::fmt::Write as _;
        const POINT_MIN_SPACING_M: f64 = 12.0;
        brown_points.sort_by_key(|(_, c)| c.to_layer().starts_with("ugly"));
        let mut kept: Vec<[f64; 2]> = Vec::new();
        for (p, class) in &brown_points {
            let kp_layer = class.to_layer();
            if !kept.iter().all(|k| dist(*k, *p) >= POINT_MIN_SPACING_M) {
                continue;
            }
            kept.push(*p);
            let layer = layer_isom(kp_layer).unwrap_or(kp_layer).to_string();
            grow(&[*p]);
            let _ = write!(
                body,
                "POINT\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 50\r\n0\r\n  0\r\n",
                p[0], p[1]
            );
            feats.push(feature(
                "Point",
                json!([r2(p[0]), r2(p[1])]),
                &layer_props(kp_layer),
            ));
            layers.insert(layer);
        }
    }

    // chained cliff lines (dashes clustered per face, sub-minimum faces dropped)
    for (mids, layer) in [(&cliff_mids_202, "202"), (&cliff_mids_201, "201")] {
        for chain in chain_cliff_dashes(mids) {
            grow(&chain);
            dxf_curves_entity(&mut body, layer, &chain, false, None);
            feats.push(feature(
                "LineString",
                coords_line(curve_points(layer, &chain, false)),
                &[("layer", layer), ("isom", layer)],
            ));
            layers.insert(layer.to_string());
        }
    }

    // all merged GeoJSON outputs: polygons become closed polylines, lines stay open
    for name in GEOJSON_NAMES {
        // contours come from merged.dxf.bin (full classification) when it exists
        if *name == "contours" && have_merged_bin {
            continue;
        }
        let path = format!("{batchoutfolder}/merged_{name}.geojson");
        if !fs.exists(&path) {
            continue;
        }
        let val: Value = serde_json::from_reader(BufReader::new(fs.open(&path)?))?;
        let empty = Vec::new();
        for f in val["features"].as_array().unwrap_or(&empty) {
            let props = &f["properties"];
            let layer = props["isom"]
                .as_str()
                .or(props["layer"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let coords = &f["geometry"]["coordinates"];
            // the GeoJSON gets the same fitted curve as the DXF, sampled to a polyline
            let sampled: Value = match f["geometry"]["type"].as_str().unwrap_or("") {
                "LineString" => {
                    let pts = parse_line(coords);
                    grow(&pts);
                    dxf_curves_entity(&mut body, &layer, &pts, false, None);
                    layers.insert(layer.clone());
                    coords_line(curve_points(&layer, &pts, false))
                }
                "MultiLineString" => {
                    let mut parts = Vec::new();
                    for part in coords.as_array().unwrap_or(&empty) {
                        let pts = parse_line(part);
                        grow(&pts);
                        dxf_curves_entity(&mut body, &layer, &pts, false, None);
                        parts.push(coords_line(curve_points(&layer, &pts, false)));
                    }
                    layers.insert(layer.clone());
                    Value::Array(parts)
                }
                "Polygon" => {
                    let mut rings = Vec::new();
                    for ring in coords.as_array().unwrap_or(&empty) {
                        let pts = parse_line(ring);
                        grow(&pts);
                        dxf_curves_entity(&mut body, &layer, &pts, true, None);
                        rings.push(coords_line(curve_points(&layer, &pts, true)));
                    }
                    layers.insert(layer.clone());
                    Value::Array(rings)
                }
                _ => continue,
            };
            let mut nf = f.clone();
            nf["geometry"]["coordinates"] = sampled;
            feats.push(nf);
        }
    }

    if body.is_empty() {
        info!("No vector outputs found, skipping combined DXF");
        return Ok(());
    }

    write_feature_collection(
        &mut BufWriter::new(fs.create(format!("{batchoutfolder}/output.geojson"))?),
        &feats,
        crs(epsg).as_ref(),
    )?;

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
    for name in GEOJSON_NAMES {
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
}
