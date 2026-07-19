//! GeoJSON output for the vector layers karttapullautin already produces, plus
//! cropping of the per-tile files to their tile bounds.
//!
//! Coordinates are written in the native projected CRS of the input data.

use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Geometry};
use crate::io::fs::FileSystem;

/// Suffixes of the per-tile GeoJSON files that batch mode crops to the tile bounds.
pub const GEOJSON_NAMES: &[&str] = &["contours", "formlines", "dotknolls"];

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

    #[test]
    fn bindxf_to_geojson_carries_elevation_and_isom() {
        use crate::geometry::{Bounds, Classification, Point3, Polylines};
        use crate::io::fs::FileSystem;

        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let mut lines: Polylines<Point3, (Classification, f64)> = Polylines::new();
        lines.push(
            vec![Point3::new(1.0, 2.0, 612.5), Point3::new(3.0, 4.0, 612.5)],
            (Classification::Contour, 612.5),
        );
        let dxf = BinaryDxf::new(Bounds::new(0.0, 10.0, 0.0, 10.0), vec![lines.into()]);
        dxf.to_writer(&mut fs.create("in.dxf.bin").unwrap())
            .unwrap();

        bindxf_to_geojson(
            &fs,
            Path::new("in.dxf.bin"),
            Path::new("out.geojson"),
            Some(25832),
        )
        .unwrap();

        let val: Value = serde_json::from_str(&fs.read_to_string("out.geojson").unwrap()).unwrap();
        assert_eq!(
            val["crs"]["properties"]["name"],
            "urn:ogc:def:crs:EPSG::25832"
        );
        let f = &val["features"][0];
        assert_eq!(f["geometry"]["type"], "LineString");
        // the per-polyline height rides along as `elevation`, as it does in the DXF
        assert_eq!(f["properties"]["elevation"], 612.5);
        // contours map to ISOM 101, and the internal layer name is kept alongside
        assert_eq!(f["properties"]["isom"], "101");
        assert_eq!(f["properties"]["layer"], "contour");
    }
}
