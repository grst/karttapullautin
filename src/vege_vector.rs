//! Vectorize the per-cell vegetation classification grids into ISOM-coded polygons:
//! connected-component labeling, ISOM minimum-area dissolve, cell-edge ring tracing,
//! Douglas-Peucker simplification and Chaikin corner rounding. Output is GeoJSON plus a
//! combined binary/text DXF with ISOM layer names for map program import.

use std::collections::HashMap;
use std::error::Error;
use std::io::BufWriter;
use std::path::Path;

use image::{GrayImage, Luma};
use imageproc::region_labelling::{Connectivity, connected_components};
use serde_json::Value;

use crate::config::Config;
use crate::geojson;
use crate::geometry::{BinaryDxf, Bounds, Classification, Point2, Polylines};
use crate::io::fs::FileSystem;
use crate::vec2d::Vec2D;

/// ISOM 2017-2 minimum footprint areas (m²) for open-land/vegetation area symbols,
/// from the spec's "Minimum area" parameters (footprints at 1:15,000, 1 mm = 15 m).
fn min_area_m2(isom: u16) -> f64 {
    match isom {
        401 => 64.0,    // Open land: 0.55 x 0.55 mm (8 x 8 m)
        402 => 900.0,   // Open land, scattered trees: 2 x 2 mm (30 x 30 m)
        403 => 225.0,   // Rough open land: 1 x 1 mm (15 x 15 m)
        404 => 1406.25, // Rough open land, scattered trees: 2.5 x 2.5 mm (37.5 x 37.5 m)
        405 => 225.0,   // Forest: openings in other screens 1 x 1 mm (general case)
        406 => 225.0,   // Vegetation, slow running: 1 x 1 mm (15 x 15 m)
        407 => 337.5,   // Slow running, good visibility: 1.5 x 1 mm (22.5 x 15 m)
        408 => 110.25,  // Vegetation, walk: 0.7 x 0.7 mm (10.5 x 10.5 m)
        409 => 225.0,   // Walk, good visibility: 1 x 1 mm (15 x 15 m)
        410 => 64.0,    // Vegetation, fight: 0.55 x 0.55 mm (8 x 8 m)
        412 => 2025.0,  // Cultivated land: 3 x 3 mm (45 x 45 m)
        413 => 900.0,   // Orchard: 2 x 2 mm (30 x 30 m)
        // codes outside the vegetation table: the smallest spec minimum, which keeps
        // polygons rather than dissolving them on a guess
        _ => 64.0,
    }
}

fn isom_to_class(isom: u16) -> Classification {
    match isom {
        403 => Classification::Veg403,
        406 => Classification::Veg406,
        407 => Classification::Veg407,
        408 => Classification::Veg408,
        _ => Classification::Veg410,
    }
}

/// One vegetation polygon: ISOM code + rings (first = exterior CCW, rest = holes CW).
/// Rings are open (first point not repeated).
type VegPolygon = (u16, Vec<Vec<Point2>>);

/// A vertex on the integer cell-corner lattice.
type V = (i64, i64);

/// Directed boundary edges keyed by start vertex: (end vertex, label on the other side).
type EdgeMap = HashMap<V, Vec<(V, u32)>>;

/// Vectorize one class grid. `code_of` maps a non-zero cell value to its ISOM code,
/// `median_radii` mirrors the raster median filtering (0 = skip pass), `epsilon` is the
/// Douglas-Peucker tolerance in meters (0 disables simplification and smoothing).
fn grid_to_polygons(
    grid: &Vec2D<u8>,
    origin: (f64, f64),
    cell: f64,
    code_of: &dyn Fn(u8) -> u16,
    median_radii: [u32; 2],
    epsilon: f64,
) -> Vec<VegPolygon> {
    let (w, h) = (grid.width() as u32, grid.height() as u32);
    if w == 0 || h == 0 {
        return Vec::new();
    }

    // work in image space with pixel (x,y) == grid (x,y); no flips anywhere
    let mut img = GrayImage::from_fn(w, h, |x, y| Luma([grid[(x as usize, y as usize)]]));
    for r in median_radii {
        if r > 0 {
            img = imageproc::filter::median_filter(&img, r, r);
        }
    }

    // dissolve components below the ISOM minimum area into their surroundings.
    // Iterated to a fixpoint: dissolving component B after A dissolved into it orphans
    // A's cells as a new small component, so one pass is not enough on noisy data.
    // Terminates: a pass that changes cells only ever merges components (dissolved cells
    // join a neighbouring class), so the component count strictly decreases each pass.
    let mut labels = connected_components(&img, Connectivity::Four, Luma([0u8]));
    loop {
        let mut comp_cells: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                let l = labels.get_pixel(x, y)[0];
                if l != 0 {
                    comp_cells.entry(l).or_default().push((x, y));
                }
            }
        }
        let mut changed = false;
        for cells in comp_cells.values() {
            let class = img.get_pixel(cells[0].0, cells[0].1)[0];
            let min_cells = (min_area_m2(code_of(class)) / (cell * cell)).ceil() as usize;
            if cells.len() >= min_cells {
                continue;
            }
            // dissolve into the most frequent surrounding class (out-of-grid = background)
            let mut counts: HashMap<u8, usize> = HashMap::new();
            for &(x, y) in cells {
                let mut visit = |nx: i64, ny: i64| {
                    let c = if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        0
                    } else {
                        img.get_pixel(nx as u32, ny as u32)[0]
                    };
                    if c != class {
                        *counts.entry(c).or_default() += 1;
                    }
                };
                visit(x as i64 - 1, y as i64);
                visit(x as i64 + 1, y as i64);
                visit(x as i64, y as i64 - 1);
                visit(x as i64, y as i64 + 1);
            }
            let new = counts
                .into_iter()
                .max_by_key(|&(_, n)| n)
                .map(|(c, _)| c)
                .unwrap_or(0);
            for &(x, y) in cells {
                img.put_pixel(x, y, Luma([new]));
            }
            changed = true;
        }
        labels = connected_components(&img, Connectivity::Four, Luma([0u8]));
        if !changed {
            break;
        }
    }

    // trace each component's boundary rings
    let lab = |x: i64, y: i64| -> u32 {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            0
        } else {
            labels.get_pixel(x as u32, y as u32)[0]
        }
    };

    // Boundary chains are shared by exactly two components. Simplifying each chain ONCE
    // and reusing it for both sides keeps adjacent polygons perfectly coincident (no
    // hairline slivers). Chains are anchored at junction vertices where 3+ labels meet
    // (or where the same pair pinches diagonally - splitting there keeps the walk
    // decomposition identical on both sides).
    let mut junctions: std::collections::HashSet<(i64, i64)> = Default::default();
    for y in 0..=(h as i64) {
        for x in 0..=(w as i64) {
            let d = [lab(x - 1, y - 1), lab(x, y - 1), lab(x - 1, y), lab(x, y)];
            let mut s = d;
            s.sort_unstable();
            let mut distinct = 1;
            for i in 1..4 {
                if s[i] != s[i - 1] {
                    distinct += 1;
                }
            }
            let checker = d[0] == d[3] && d[1] == d[2] && d[0] != d[1];
            if distinct >= 3 || checker {
                junctions.insert((x, y));
            }
        }
    }

    // directed cell edges, CCW around each cell, with the label on the other side
    let mut comp_edges: HashMap<u32, (u8, EdgeMap)> = HashMap::new();
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let l = lab(x, y);
            if l == 0 {
                continue;
            }
            let entry = comp_edges
                .entry(l)
                .or_insert_with(|| (img.get_pixel(x as u32, y as u32)[0], HashMap::new()));
            let edges = &mut entry.1;
            let o = lab(x, y - 1);
            if o != l {
                edges.entry((x, y)).or_default().push(((x + 1, y), o));
            }
            let o = lab(x + 1, y);
            if o != l {
                edges
                    .entry((x + 1, y))
                    .or_default()
                    .push(((x + 1, y + 1), o));
            }
            let o = lab(x, y + 1);
            if o != l {
                edges
                    .entry((x + 1, y + 1))
                    .or_default()
                    .push(((x, y + 1), o));
            }
            let o = lab(x - 1, y);
            if o != l {
                edges.entry((x, y + 1)).or_default().push(((x, y), o));
            }
        }
    }

    // simplified shared chains, cached so both sides get the identical polyline; the
    // second requester always traverses the chain in the opposite direction
    let mut chain_cache: HashMap<(u32, u32, V, V, V), Vec<Point2>> = HashMap::new();
    let to_world = |v: V| Point2::new(origin.0 + v.0 as f64 * cell, origin.1 + v.1 as f64 * cell);

    let mut polygons = Vec::new();
    for (label, (class, edges)) in comp_edges {
        let code = code_of(class);
        let mut exteriors: Vec<Vec<Point2>> = Vec::new();
        let mut holes: Vec<Vec<Point2>> = Vec::new();
        for (vs, others) in chain_rings(edges) {
            let n = vs.len();
            let junct_pos: Vec<usize> = (0..n).filter(|&i| junctions.contains(&vs[i])).collect();
            let mut ring_pts: Vec<Point2> = Vec::new();
            if junct_pos.is_empty() {
                // island: one closed chain between exactly two components
                let other = others[0];
                let vmin = *vs.iter().min().unwrap();
                let key = (label.min(other), label.max(other), vmin, vmin, vmin);
                if let Some(cached) = chain_cache.get(&key) {
                    ring_pts = cached.iter().rev().cloned().collect();
                } else {
                    let mut pts: Vec<Point2> = vs.iter().map(|&v| to_world(v)).collect();
                    if epsilon > 0.0 {
                        pts = simplify_closed(pts, epsilon);
                        pts = chaikin_closed(&pts);
                    }
                    chain_cache.insert(key, pts.clone());
                    ring_pts = pts;
                }
            } else {
                for k in 0..junct_pos.len() {
                    let a = junct_pos[k];
                    let b = junct_pos[(k + 1) % junct_pos.len()];
                    let mut chain = vec![vs[a]];
                    let mut i = (a + 1) % n;
                    loop {
                        chain.push(vs[i]);
                        if i == b {
                            break;
                        }
                        i = (i + 1) % n;
                    }
                    let other = others[a];
                    let (e0, e1) = (
                        *chain.first().unwrap().min(chain.last().unwrap()),
                        *chain.first().unwrap().max(chain.last().unwrap()),
                    );
                    // interior min disambiguates parallel chains between the same pair
                    let vmin = chain[1..chain.len().saturating_sub(1)]
                        .iter()
                        .min()
                        .copied()
                        .unwrap_or(chain[0]);
                    let key = (label.min(other), label.max(other), e0, e1, vmin);
                    let pts: Vec<Point2> = if let Some(cached) = chain_cache.get(&key) {
                        cached.iter().rev().cloned().collect()
                    } else {
                        let mut pts: Vec<Point2> = chain.iter().map(|&v| to_world(v)).collect();
                        if epsilon > 0.0 {
                            pts = dp(&pts, epsilon);
                            pts = chaikin_open(&pts);
                        }
                        chain_cache.insert(key, pts.clone());
                        pts
                    };
                    let skip = if ring_pts.is_empty() { 0 } else { 1 };
                    ring_pts.extend(pts.into_iter().skip(skip));
                }
                if ring_pts.len() > 1 && ring_pts.first() == ring_pts.last() {
                    ring_pts.pop();
                }
            }
            if ring_pts.len() < 3 {
                continue;
            }
            if signed_area(&ring_pts) > 0.0 {
                exteriors.push(ring_pts);
            } else {
                holes.push(ring_pts);
            }
        }
        if exteriors.is_empty() {
            continue;
        }
        // one 4-connected component has exactly one exterior; if point-touching pinches
        // produced several, attach the holes to the largest and emit the rest hole-less
        exteriors.sort_by(|a, b| {
            signed_area(b)
                .abs()
                .partial_cmp(&signed_area(a).abs())
                .unwrap()
        });
        let mut it = exteriors.into_iter();
        let mut rings = vec![it.next().unwrap()];
        rings.extend(holes);
        polygons.push((code, rings));
        for extra in it {
            // pinch fragments below the ISOM minimum are dropped, not emitted
            if signed_area(&extra) >= min_area_m2(code) {
                polygons.push((code, vec![extra]));
            }
        }
    }
    polygons
}

/// Chain directed edges into closed rings, tracking the other-side label per segment.
/// Every vertex has equal in/out degree, so a walk always finds an outgoing edge until
/// it closes on its start.
fn chain_rings(mut edges: EdgeMap) -> Vec<(Vec<V>, Vec<u32>)> {
    let mut rings = Vec::new();
    while let Some((&start, _)) = edges.iter().next() {
        let mut ring = vec![start];
        let mut others = Vec::new();
        let mut cur = start;
        loop {
            let Some(nexts) = edges.get_mut(&cur) else {
                // inconsistent topology; drop this partial ring rather than panic
                ring.clear();
                break;
            };
            let (next, other) = nexts.pop().unwrap();
            if nexts.is_empty() {
                edges.remove(&cur);
            }
            others.push(other);
            if next == start {
                break;
            }
            ring.push(next);
            cur = next;
        }
        if ring.len() >= 3 {
            rings.push((ring, others));
        }
    }
    rings
}

/// Shoelace formula; positive = counter-clockwise. Ring is open.
fn signed_area(ring: &[Point2]) -> f64 {
    let n = ring.len();
    let mut s = 0.0;
    for i in 0..n {
        let p = &ring[i];
        let q = &ring[(i + 1) % n];
        s += p.x * q.y - q.x * p.y;
    }
    s / 2.0
}

fn perp_dist(p: &Point2, a: &Point2, b: &Point2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    ((p.x - a.x) * dy - (p.y - a.y) * dx).abs() / len
}

/// Douglas-Peucker on an open polyline (endpoints kept).
pub(crate) fn dp(pts: &[Point2], eps: f64) -> Vec<Point2> {
    let n = pts.len();
    if n <= 2 {
        return pts.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (mut maxd, mut idx) = (0.0f64, i);
        for k in i + 1..j {
            let d = perp_dist(&pts[k], &pts[i], &pts[j]);
            if d > maxd {
                maxd = d;
                idx = k;
            }
        }
        if maxd > eps {
            keep[idx] = true;
            stack.push((i, idx));
            stack.push((idx, j));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|&(_, k)| k)
        .map(|(p, _)| p.clone())
        .collect()
}

/// Douglas-Peucker for a closed ring: split at the vertex farthest from vertex 0,
/// simplify both halves, rejoin. Ring is open (no repeated first point).
pub(crate) fn simplify_closed(ring: Vec<Point2>, eps: f64) -> Vec<Point2> {
    if ring.len() <= 4 {
        return ring;
    }
    let far = (1..ring.len())
        .max_by(|&a, &b| {
            let da = (ring[a].x - ring[0].x).powi(2) + (ring[a].y - ring[0].y).powi(2);
            let db = (ring[b].x - ring[0].x).powi(2) + (ring[b].y - ring[0].y).powi(2);
            da.partial_cmp(&db).unwrap()
        })
        .unwrap();
    let mut first = dp(&ring[..=far], eps);
    let mut second: Vec<Point2> = ring[far..].to_vec();
    second.push(ring[0].clone());
    let second = dp(&second, eps);
    // first ends at ring[far], second starts there and ends at ring[0] = first[0]
    first.extend_from_slice(&second[1..second.len() - 1]);
    first
}

/// One iteration of Chaikin corner cutting on an open polyline, endpoints preserved.
pub(crate) fn chaikin_open(pts: &[Point2]) -> Vec<Point2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0].clone()];
    for w in pts.windows(2) {
        out.push(Point2::new(
            0.75 * w[0].x + 0.25 * w[1].x,
            0.75 * w[0].y + 0.25 * w[1].y,
        ));
        out.push(Point2::new(
            0.25 * w[0].x + 0.75 * w[1].x,
            0.25 * w[0].y + 0.75 * w[1].y,
        ));
    }
    out.push(pts[pts.len() - 1].clone());
    out
}

/// One iteration of Chaikin corner cutting on a closed ring (open representation).
pub(crate) fn chaikin_closed(ring: &[Point2]) -> Vec<Point2> {
    let n = ring.len();
    let mut out = Vec::with_capacity(2 * n);
    for i in 0..n {
        let p = &ring[i];
        let q = &ring[(i + 1) % n];
        out.push(Point2::new(
            0.75 * p.x + 0.25 * q.x,
            0.75 * p.y + 0.25 * q.y,
        ));
        out.push(Point2::new(
            0.25 * p.x + 0.75 * q.x,
            0.25 * p.y + 0.75 * q.y,
        ));
    }
    out
}

fn write_geojson_file(
    fs: &impl FileSystem,
    path: &Path,
    polygons: &[VegPolygon],
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut feats = Vec::with_capacity(polygons.len());
    for (code, rings) in polygons {
        let code = code.to_string();
        let coords = Value::Array(
            rings
                .iter()
                .map(|r| {
                    // close the ring for GeoJSON
                    let closed = r.iter().chain(std::iter::once(&r[0]));
                    geojson::coords_line(closed.map(|p| [p.x, p.y]))
                })
                .collect(),
        );
        feats.push(geojson::feature("Polygon", coords, &[("isom", &code)]));
    }
    geojson::write_feature_collection(
        &mut BufWriter::new(fs.create(path)?),
        &feats,
        geojson::crs(epsg).as_ref(),
    )
}

/// Vectorize and write all vegetation vector outputs. Called from `makevege` when
/// `vectorvege=1`. Grids: `green` = greenshade index per block cell, `yellow` = 0/1 per
/// 3 m cell (origin shifted +1.5 m, see makevege's 2x2 sum window), `ug` = 0/1 per
/// block*6 cell.
#[allow(clippy::too_many_arguments)]
pub fn export_all(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    green: &Vec2D<u8>,
    yellow: &Vec2D<u8>,
    ug: &Vec2D<u8>,
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    block: f64,
) -> Result<(), Box<dyn Error>> {
    log::info!("Vectorizing vegetation...");
    let eps = config.vegesimplify;

    // mirror the raster median filtering, radius converted from meters to cells
    let radius = |m: u32, cell: f64| -> u32 {
        if m > 1 {
            (((m / 2) as f64 / cell).round() as u32).max(1)
        } else {
            0
        }
    };
    let green_med = [radius(config.med, block), radius(config.med2, block)];
    let yellow_med = if config.proceed_yellows {
        [radius(config.med, 3.0), radius(config.med2, 3.0)]
    } else {
        [radius(config.medyellow, 3.0), 0]
    };

    let map = &config.greenshadeisom;
    let green_code = |c: u8| -> u16 {
        map.get((c as usize).saturating_sub(1))
            .or(map.last())
            .copied()
            .unwrap_or(410)
    };

    let green_polys = grid_to_polygons(green, (xmin, ymin), block, &green_code, green_med, eps);
    let yellow_polys = grid_to_polygons(
        yellow,
        (xmin + 1.5, ymin + 1.5),
        3.0,
        &|_| 403,
        yellow_med,
        eps,
    );
    let ug_polys = grid_to_polygons(ug, (xmin, ymin), block * 6.0, &|_| 407, [0, 0], eps);

    write_geojson_file(
        fs,
        &tmpfolder.join("vegetation.geojson"),
        &green_polys,
        config.epsg,
    )?;
    write_geojson_file(
        fs,
        &tmpfolder.join("yellow.geojson"),
        &yellow_polys,
        config.epsg,
    )?;
    write_geojson_file(
        fs,
        &tmpfolder.join("undergrowth.geojson"),
        &ug_polys,
        config.epsg,
    )?;

    // combined DXF, light-to-dark layer order for map program draw order
    let order = |code: u16| -> usize {
        [403u16, 406, 408, 410, 407]
            .iter()
            .position(|&c| c == code)
            .unwrap_or(5)
    };
    let mut all: Vec<&VegPolygon> = green_polys
        .iter()
        .chain(&yellow_polys)
        .chain(&ug_polys)
        .collect();
    all.sort_by_key(|(code, _)| order(*code));

    let mut lines: Polylines<Point2, Classification> = Polylines::new();
    for (code, rings) in all {
        let class = isom_to_class(*code);
        for ring in rings {
            let mut closed = ring.clone();
            closed.push(ring[0].clone());
            lines.push(closed, class);
        }
    }
    let dxf = BinaryDxf::new(Bounds::new(xmin, xmax, ymin, ymax), vec![lines.into()]);
    dxf.to_writer(&mut fs.create(tmpfolder.join("vegetation.dxf.bin"))?)?;
    if config.output_dxf {
        dxf.to_dxf(&mut fs.create(tmpfolder.join("vegetation.dxf"))?)?;
    }
    log::info!("Done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x8 grid: one class-1 region with a hole, plus a checkerboard corner where two
    /// class-1 cells touch diagonally. Also a tiny isolated patch that must dissolve.
    #[test]
    fn trace_hole_and_checkerboard() {
        let mut grid = Vec2D::new(8, 8, 0u8);
        // 5x5 block of class 1 at (1..6, 1..6) with a hole at (3,3)
        for x in 1..6 {
            for y in 1..6 {
                grid[(x, y)] = 1;
            }
        }
        grid[(3, 3)] = 0;
        // checkerboard corner: diagonal touch at (6,6)
        grid[(6, 6)] = 1;
        // no isolated patch here; dissolve tested separately

        // min area tiny so nothing dissolves (cell=10 -> 100 m² per cell)
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 10.0, &|_| 410, [0, 0], 0.0);

        // the diagonal cell is its own component -> 2 polygons
        assert_eq!(polys.len(), 2, "expected big region + diagonal cell");
        let big = polys
            .iter()
            .find(|(_, rings)| rings[0].len() > 4)
            .expect("big region present");
        assert_eq!(big.1.len(), 2, "big region should have exterior + hole");
        assert!(signed_area(&big.1[0]) > 0.0, "exterior must be CCW");
        assert!(signed_area(&big.1[1]) < 0.0, "hole must be CW");
        // exterior area = 25 cells - 1 hole cell => shoelace = 24 cells * 100 m²... the
        // exterior ring itself encloses 25 cells (the hole is a separate ring)
        assert_eq!(signed_area(&big.1[0]), 25.0 * 100.0);
        assert_eq!(signed_area(&big.1[1]), -100.0);
    }

    #[test]
    fn dissolve_small_patch() {
        let mut grid = Vec2D::new(10, 10, 0u8);
        // big class-1 region
        for x in 0..10 {
            for y in 0..5 {
                grid[(x, y)] = 1;
            }
        }
        // single-cell class-2 island inside it: 9 m² << min area, must dissolve into 1
        grid[(4, 2)] = 2;
        let code_of = |c: u8| -> u16 {
            match c {
                1 => 410,
                _ => 406,
            }
        };
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 3.0, &code_of, [0, 0], 0.0);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].0, 410);
        assert_eq!(polys[0].1.len(), 1, "island dissolved, no hole");
    }

    /// Speckled multi-class grid: after the dissolve fixpoint no emitted polygon may be
    /// below its ISOM minimum area (the exact bug seen on real data with one pass).
    #[test]
    fn dissolve_reaches_fixpoint() {
        let mut grid = Vec2D::new(20, 20, 0u8);
        // deterministic speckle of classes 1..=3 over a solid class-1 base
        for x in 0..20 {
            for y in 0..12 {
                grid[(x, y)] = 1;
            }
        }
        for x in 0..20usize {
            for y in 12..20usize {
                grid[(x, y)] = ((x * 7 + y * 13) % 4) as u8; // 0..=3 speckle
            }
        }
        let code_of = |c: u8| -> u16 {
            match c {
                1 => 406,
                2 => 408,
                _ => 410,
            }
        };
        // cell 3 m => 9 m² per cell, minimums are 225/110/68 m²
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 3.0, &code_of, [0, 0], 0.0);
        assert!(!polys.is_empty());
        for (code, rings) in &polys {
            let a = signed_area(&rings[0]);
            assert!(
                a >= min_area_m2(*code),
                "polygon of code {code} below minimum: {a} m²"
            );
        }
    }

    /// Two adjacent classes with a jagged seam: after simplification + smoothing both
    /// polygons must contain the exact same vertices along the shared boundary (the
    /// white-sliver regression).
    #[test]
    fn adjacent_polygons_share_simplified_boundary() {
        let mut grid = Vec2D::new(12, 12, 0u8);
        for y in 0..12usize {
            let split = 6 + (y % 2); // jagged seam
            for x in 0..split {
                grid[(x, y)] = 1;
            }
            for x in split..12 {
                grid[(x, y)] = 2;
            }
        }
        let code_of = |c: u8| -> u16 {
            match c {
                1 => 406,
                _ => 410,
            }
        };
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 5.0, &code_of, [0, 0], 2.0);
        assert_eq!(polys.len(), 2);
        let seam = |rings: &Vec<Vec<Point2>>| -> std::collections::BTreeSet<(i64, i64)> {
            rings[0]
                .iter()
                .filter(|p| p.x > 29.9 && p.x < 35.1)
                .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
                .collect()
        };
        let (a, b) = (seam(&polys[0].1), seam(&polys[1].1));
        assert!(a.len() >= 2, "seam vertices expected");
        assert_eq!(
            a, b,
            "shared boundary must be point-identical on both sides"
        );
    }

    #[test]
    fn simplify_keeps_shape() {
        // staircase square ~ 10x10 with unit steps
        let mut ring = Vec::new();
        for i in 0..10 {
            ring.push(Point2::new(i as f64, 0.0));
        }
        for i in 0..10 {
            ring.push(Point2::new(10.0, i as f64));
        }
        for i in 0..10 {
            ring.push(Point2::new(10.0 - i as f64, 10.0));
        }
        for i in 0..10 {
            ring.push(Point2::new(0.0, 10.0 - i as f64));
        }
        let simplified = simplify_closed(ring, 0.5);
        assert!(simplified.len() <= 8, "collinear points removed");
        let area = signed_area(&simplified).abs();
        assert!((area - 100.0).abs() < 1.0);
    }
}
