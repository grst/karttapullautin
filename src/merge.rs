use image::{RgbImage, Rgba, RgbaImage};
use log::info;
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::geometry::{
    BinaryDxf, Classification, Geometry, Point2, Point3, Points, Polylines, Ring, join_polylines,
};
use crate::io::bytes::FromToBytes;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::mapframe::WorldFile;
use crate::vec2d::Vec2D;
use image::buffer::ConvertBuffer;

fn merge_png(
    fs: &impl FileSystem,
    config: &Config,
    png_files: Vec<PathBuf>,
    outfilename: &str,
    scale: f64,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut xmin = f64::MAX;
    let mut ymin = f64::MAX;
    let mut xmax = f64::MIN;
    let mut ymax = f64::MIN;
    let mut min_res = f64::MAX;
    for png in png_files.iter() {
        let filename = png.as_path().file_name().unwrap().to_str().unwrap();
        let full_filename = format!("{batchoutfolder}/{filename}");
        let img = fs
            .read_image_png(&full_filename)
            .expect("Opening image failed");

        let width = img.width() as f64;
        let height = img.height() as f64;
        let pgw = full_filename.replace(".png", ".pgw");
        let input = Path::new(&pgw);
        if fs.exists(input) {
            let w = WorldFile::read(fs, input).expect("Can not read input file");
            let res = w.pixel_size_x;
            let tfw4 = w.x_origin;
            let tfw5 = w.y_origin;

            if res < min_res {
                min_res = res
            }
            if tfw4 < xmin {
                xmin = tfw4;
            }
            if (tfw4 + width * res) > xmax {
                xmax = tfw4 + width * res;
            }
            if tfw5 > ymax {
                ymax = tfw5;
            }
            if (tfw5 - height * res) < ymin {
                ymin = tfw5 - height * res;
            }
        }
    }
    let mut im = RgbaImage::from_pixel(
        ((xmax - xmin) / min_res / scale) as u32,
        ((ymax - ymin) / min_res / scale) as u32,
        Rgba([255, 255, 255, 0]),
    );
    for png in png_files.iter() {
        let filename = png.as_path().file_name().unwrap().to_str().unwrap();
        let png = format!("{batchoutfolder}/{filename}");
        let pgw = png.replace(".png", ".pgw");
        let png = Path::new(&png);
        let pgw = Path::new(&pgw);
        let filesize = fs.file_size(png).unwrap();
        if fs.exists(png) && fs.exists(pgw) && filesize > 0 {
            let img = fs.read_image_png(png).expect("Opening image failed");
            let width = img.width() as f64;
            let height = img.height() as f64;

            let w = WorldFile::read(fs, pgw).expect("Can not read input file");
            let res = w.pixel_size_x;
            let tfw4 = w.x_origin;
            let tfw5 = w.y_origin;

            let img2 = image::imageops::thumbnail(
                &img,
                (res / min_res / scale * width + 0.5) as u32,
                (res / min_res / scale * height + 0.5) as u32,
            );

            image::imageops::overlay(
                &mut im,
                &img2,
                ((tfw4 - xmin) / min_res / scale) as i64,
                ((ymax - tfw5) / min_res / scale) as i64,
            );
        }
    }

    // Merge outputs belong in batchoutfolder, next to the tiles they merge —
    // not in whatever the process cwd happens to be.
    let outfilename = format!("{batchoutfolder}/{outfilename}");

    let im_rgb8: RgbImage = im.convert();
    im_rgb8
        .write_to(
            &mut fs
                .create(format!("{outfilename}.jpg"))
                .expect("could not save output jpg"),
            image::ImageFormat::Jpeg,
        )
        .expect("could not save output jpg");

    im.write_to(
        &mut fs
            .create(format!("{outfilename}.png"))
            .expect("could not save output png"),
        image::ImageFormat::Png,
    )
    .expect("could not save output Png");

    let mut tfw_file = fs
        .create(format!("{outfilename}.pgw"))
        .expect("Unable to create file");
    WorldFile {
        pixel_size_x: min_res * scale,
        rotation_y: 0.0,
        rotation_x: 0.0,
        pixel_size_y: -min_res * scale,
        x_origin: xmin,
        y_origin: ymax,
    }
    .write(&mut tfw_file)
    .expect("Could not write to file");
    drop(tfw_file);
    fs.copy(
        Path::new(&format!("{outfilename}.pgw")),
        Path::new(&format!("{outfilename}.jgw")),
    )
    .expect("Could not copy file");
    Ok(())
}

pub fn pngmergevege(
    fs: &impl FileSystem,
    config: &Config,
    scale: f64,
    include_undergrowth: bool,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut png_files: Vec<PathBuf> = Vec::new();
    for path in fs.list(batchoutfolder).unwrap() {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if filename.ends_with("_vege.png")
            || (include_undergrowth && filename.ends_with("_undergrowth.png"))
        {
            png_files.push(path);
        }
    }
    if png_files.is_empty() {
        info!("No _vege.png files found in output directory");
        return Ok(());
    }

    let output_name = if include_undergrowth {
        "merged_vege_undergrowth"
    } else {
        "merged_vege"
    };

    merge_png(fs, config, png_files, output_name, scale).unwrap();
    Ok(())
}

pub fn pngmerge(
    fs: &impl FileSystem,
    config: &Config,
    scale: f64,
    depr: bool,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut png_files: Vec<PathBuf> = Vec::new();
    for path in fs.list(batchoutfolder).unwrap() {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if filename.ends_with(".png")
            && !filename.ends_with("_undergrowth.png")
            && !filename.ends_with("_undergrowth_bit.png")
            && !filename.ends_with("_vege.png")
            && !filename.ends_with("_vege_bit.png")
            && ((depr && filename.ends_with("_depr.png"))
                || (!depr && !filename.ends_with("_depr.png")))
        {
            png_files.push(path);
        }
    }

    if png_files.is_empty() {
        info!("No files to merge found in output directory");
        return Ok(());
    }
    let mut outfilename = "merged";
    if depr {
        outfilename = "merged_depr";
    }
    merge_png(fs, config, png_files, outfilename, scale).unwrap();
    Ok(())
}

pub fn bindxfmerge(fs: &impl FileSystem, config: &Config) -> anyhow::Result<()> {
    let batchoutfolder = &config.batchoutfolder;

    // These are the different file suffixes we expect:
    let suffixes_to_merge = [
        "contours",
        // "c2f", // No such files exist anymore
        // "c2",  // No such files exist anymore
        "c2g",
        "basemap",
        "c3g",
        "formlines",
        "dotknolls",
        "detected",
    ];

    // a list of files for each suffix
    let mut dxf_files: Vec<Vec<PathBuf>> = vec![Vec::new(); suffixes_to_merge.len()];

    for path in fs.list(batchoutfolder).unwrap() {
        if let Some(filename) = path.file_name() {
            let filename = filename.to_str().unwrap();

            // check if this file matches any of the suffiexes we expect
            for (i, suffix) in suffixes_to_merge.iter().enumerate() {
                if filename.ends_with(&format!("_{suffix}.dxf.bin")) {
                    dxf_files[i].push(path.clone());
                }
            }
        }
    }

    if dxf_files.iter().all(|f| f.is_empty()) {
        info!("No dxf files found in output directory");
        return Ok(());
    }

    // For now (originally) we always use the bounds of the first file loaded for all the generated
    // files. TODO: use the actual new bounds from the loaded files instead.
    let mut first_file_bounds = None;

    let mut all_geometries = Vec::<Geometry>::new();
    for (suffix, files) in suffixes_to_merge.iter().zip(dxf_files) {
        if files.is_empty() {
            info!("No files found for suffix: {suffix}, skipping...");
            continue;
        }

        info!("Merging {} files for suffix: {suffix}", files.len());

        let output_file = PathBuf::from(format!("merged_{suffix}.dxf.bin"));

        let mut geometries: Vec<Geometry> = Vec::with_capacity(files.len());

        for file in files {
            let loaded = BinaryDxf::from_reader(&mut fs.open(&file)?)?;

            // we always use the bounds of the first loaded file
            if first_file_bounds.is_none() {
                first_file_bounds = Some(loaded.bounds().clone());
            }

            let geometry = loaded.take_geometry();

            geometries.extend(geometry.iter().cloned());

            // for the contours, we filter out the intermediate contours for the all_geometries
            if *suffix == "contours" {
                for geo in geometry {
                    let filtered_geo: Geometry = match geo {
                        Geometry::Points(points) => {
                            let mut filtered_points = Points::with_capacity(points.len());

                            for (p, c) in points.into_iter() {
                                if !c.is_intermed() {
                                    filtered_points.push(p, c);
                                }
                            }

                            filtered_points.into()
                        }
                        Geometry::Polylines2(polylines) => {
                            let mut filtered_lines = Polylines::with_capacity(polylines.len());
                            for (l, c) in polylines.into_iter() {
                                if !c.is_intermed() {
                                    filtered_lines.push(l, c);
                                }
                            }
                            filtered_lines.into()
                        }
                        Geometry::Polylines3(polylines) => {
                            let mut filtered_lines = Polylines::with_capacity(polylines.len());
                            for (l, c) in polylines.into_iter() {
                                if !c.0.is_intermed() {
                                    filtered_lines.push(l, c);
                                }
                            }
                            filtered_lines.into()
                        }
                    };

                    all_geometries.push(filtered_geo);
                }
            } else {
                all_geometries.extend(geometry);
            }
        }

        // write output file
        let output = BinaryDxf::new(
            first_file_bounds
                .clone()
                .expect("this should be set since we load at least one file"),
            geometries,
        );
        output.to_writer(&mut fs.create(&output_file)?)?;

        if config.output_dxf {
            let output_file = PathBuf::from(format!("merged_{suffix}.dxf"));
            output.to_dxf(&mut fs.create(&output_file)?)?;
        }
    }

    // output all geometries to a single file
    if let Some(all_bounds) = first_file_bounds {
        let out_merged = BinaryDxf::new(all_bounds, all_geometries);
        out_merged.to_writer(&mut fs.create("merged.dxf.bin")?)?;

        if config.output_dxf {
            out_merged.to_dxf(&mut fs.create("merged.dxf")?)?;
        }
    }

    Ok(())
}

/// The slope-line tick for a depression ring, or `None` when the ring is too small to
/// carry the symbol.
///
/// ISOM 2017-2 (symbol 101) requires at least one slope line on a depression, drawn
/// perpendicular to the contour and pointing downslope — i.e. into the ring. Its length
/// is 0.4 OM on the 1:15,000 original, 0.6 mm at the 1:10,000 we render, which is 6 m on
/// the ground; coordinates here are ground metres. A depression below ISOM's minimum
/// size (1.1 x 0.7 OM -> 1.65 x 1.05 mm -> 16.5 x 10.5 m) is not drawable as a contour
/// depression at all — those are the small-depression symbol's job — so it gets no tick.
///
/// Direction is decided by testing whether the tick's far end lands INSIDE the ring, not
/// by aiming at the ring's centroid: a depression ring is often a long crescent, and a
/// crescent's centroid lies outside it, so the centroid test pointed the tick out of the
/// depression — the "wrong side" seen on Nyrup Hegn.
///
/// Placement then picks, among candidates spread around the ring, the one whose tick end
/// sits deepest inside — furthest from any part of the ring. That keeps the tick clear of
/// the contour instead of crossing it where the depression pinches, which is where a
/// fixed position lands on an elongated ring.
fn decorate_depression(
    el_x: &[f64],
    el_y: &[f64],
    h: f64,
) -> Option<(Vec<Point3>, Classification)> {
    const LENGTH_M: f64 = 6.0;
    const MIN_WIDTH_M: f64 = 10.5;
    const MIN_LENGTH_M: f64 = 16.5;
    /// Positions tried around the ring; the best-clearance one wins.
    const CANDIDATES: usize = 12;

    let n = el_x.len();
    if n < 2 {
        return None;
    }
    let (mut xmin, mut xmax) = (f64::MAX, f64::MIN);
    let (mut ymin, mut ymax) = (f64::MAX, f64::MIN);
    for (&x, &y) in el_x.iter().zip(el_y.iter()) {
        xmin = xmin.min(x);
        xmax = xmax.max(x);
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    let (w, hgt) = (xmax - xmin, ymax - ymin);
    if w.max(hgt) < MIN_LENGTH_M || w.min(hgt) < MIN_WIDTH_M {
        // draw a small depression
        let center = ((xmax + xmin) / 2.0, (ymax + ymin + LENGTH_M) / 2.0);
        let steps = 8;
        let mut points = Vec::with_capacity(steps);
        let radius = LENGTH_M;
        for i in 0..steps {
            // Angle from 0 to PI (semi-circle)
            let angle = std::f64::consts::PI * ((i as f64) / ((steps - 1) as f64) + 1.0);
            let x = center.0 + radius * angle.cos();
            let y = center.1 + radius * angle.sin();
            points.push(Point3::new(x, y, h));
        }
        return Some((points, Classification::SmallDepression));
    }

    let ring = Ring::from_xy(el_x, el_y);
    let mut best: Option<(f64, [f64; 4])> = None;
    for k in 0..CANDIDATES {
        let i = k * n / CANDIDATES;
        let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
        let (tx, ty) = (el_x[next] - el_x[prev], el_y[next] - el_y[prev]);
        let len = (tx * tx + ty * ty).sqrt();
        if len == 0.0 {
            continue;
        }
        // Both perpendiculars; keep whichever ends up inside the ring.
        for (nx, ny) in [(-ty / len, tx / len), (ty / len, -tx / len)] {
            let (ex, ey) = (el_x[i] + nx * LENGTH_M, el_y[i] + ny * LENGTH_M);
            if !ring.contains(Point2::new(ex, ey)) {
                continue;
            }
            let clearance = ring.distance_to_point(Point2::new(ex, ey));
            if best.is_none_or(|(b, _)| clearance > b) {
                best = Some((clearance, [el_x[i], el_y[i], ex, ey]));
            }
        }
    }
    let (_, [sx, sy, ex, ey]) = best?;
    Some((
        vec![Point3::new(sx, sy, h), Point3::new(ex, ey, h)],
        Classification::SlopeLine,
    ))
}

/// Whether a contour at height `h` runs with the lower ground on its left: a vote over its
/// segments, each comparing the height half a grid cell to its right against `h`.
fn downhill_on_left(
    xyz: &Vec2D<f64>,
    xstart: f64,
    ystart: f64,
    size: f64,
    x: &[f64],
    y: &[f64],
    h: f64,
) -> bool {
    if h.is_nan() {
        return false;
    }
    // bilinear height at a world position, clamped to the grid
    let height = |wx: f64, wy: f64| -> f64 {
        let gx = ((wx - xstart) / size).clamp(0.0, (xyz.width() - 1) as f64);
        let gy = ((wy - ystart) / size).clamp(0.0, (xyz.height() - 1) as f64);
        let (ix, iy) = (
            (gx.floor() as usize).min(xyz.width().saturating_sub(2)),
            (gy.floor() as usize).min(xyz.height().saturating_sub(2)),
        );
        let (fx, fy) = (gx - ix as f64, gy - iy as f64);
        let a = xyz[(ix, iy)] * (1.0 - fx) + xyz[(ix + 1, iy)] * fx;
        let b = xyz[(ix, iy + 1)] * (1.0 - fx) + xyz[(ix + 1, iy + 1)] * fx;
        a * (1.0 - fy) + b * fy
    };
    let mut vote = 0i64;
    for k in 0..x.len().saturating_sub(1) {
        let (dx, dy) = (x[k + 1] - x[k], y[k + 1] - y[k]);
        let len = (dx * dx + dy * dy).sqrt();
        if len == 0.0 {
            continue;
        }
        // the right-hand normal of the direction of travel
        let (nx, ny) = (dy / len * size * 0.5, -dx / len * size * 0.5);
        let (mx, my) = ((x[k] + x[k + 1]) / 2.0, (y[k] + y[k + 1]) / 2.0);
        if height(mx + nx, my + ny) > h {
            vote += 1;
        } else {
            vote -= 1;
        }
    }
    vote > 0
}

pub fn smoothjoin(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
) -> Result<(), Box<dyn Error>> {
    info!("Smooth curves...");

    let &Config {
        scalefactor,
        inidotknolls,
        smoothing,
        curviness,
        mut indexcontours,
        formline,
        depression_length,
        contour_interval,
        ..
    } = config;

    let halfinterval = contour_interval / 2.0 * scalefactor;
    if formline > 0.0 {
        indexcontours = 5.0 * contour_interval;
    }

    let interval = halfinterval;

    let heightmap_in = tmpfolder.join("xyz_knolls.hmap");
    let hmap = HeightMap::from_bytes(&mut fs.open(heightmap_in)?)?;

    // in world coordinates
    let xstart = hmap.xoffset;
    let ystart = hmap.yoffset;
    let size = hmap.scale;
    let xmax = (hmap.grid.width() - 1) as u64;
    let ymax = (hmap.grid.height() - 1) as u64;
    let xyz = hmap.grid;

    let mut steepness = Vec2D::new((xmax + 1) as usize, (ymax + 1) as usize, f64::NAN);

    for i in 1..xmax as usize {
        for j in 1..ymax as usize {
            let mut low: f64 = f64::MAX;
            let mut high: f64 = f64::MIN;
            for ii in i - 1..i + 2 {
                for jj in j - 1..j + 2 {
                    let tmp = xyz[(ii, jj)];
                    if tmp < low {
                        low = tmp;
                    }
                    if tmp > high {
                        high = tmp;
                    }
                }
            }
            steepness[(i, j)] = high - low;
        }
    }

    // read the binary input
    let input = tmpfolder.join("out.dxf.bin");
    let input_dxf =
        BinaryDxf::from_reader(&mut fs.open(input)?).expect("Unable to read out.dxf.bin");

    let input_bounds = input_dxf.bounds().clone(); // store the bounds for usage in the output
    let Geometry::Polylines2(input_lines) = input_dxf.take_geometry().swap_remove(0) else {
        return Err(anyhow::anyhow!("out.dxf.bin does not contain polylines").into());
    };

    let mut out2_lines = Polylines::<Point3, (Classification, f64)>::new();

    let depr_output = tmpfolder.join("depressions.txt");
    let mut depr_fp = fs.create(depr_output).expect("Unable to create file");

    let mut dotknolls = Vec::new();

    let knollhead_output = tmpfolder.join("knollheads.txt");
    let mut knollhead_fp = fs.create(knollhead_output).expect("Unable to create file");

    let joined = join_polylines(&input_lines, usize::MAX);
    // TODO: this is not very efficient (collecting all x and y separately into Vecs), but it means the logic further down can stay the same
    let mut el_x: Vec<Vec<f64>> = joined
        .iter()
        .map(|l| l.iter().map(|p| p.x).collect())
        .collect();
    let mut el_y: Vec<Vec<f64>> = joined
        .iter()
        .map(|l| l.iter().map(|p| p.y).collect())
        .collect();
    for l in 0..input_lines.len() {
        let mut el_x_len = el_x[l].len();
        if el_x_len > 0 {
            let mut skip = false;
            let mut depression = 1;
            if el_x_len < 3 {
                skip = true;
                el_x[l].clear();
            }
            let mut h = f64::NAN;
            if !skip {
                let mut mm: isize = (((el_x_len - 1) as f64) / 3.0).floor() as isize - 1;
                if mm < 0 {
                    mm = 0;
                }
                let mut m = mm as usize;
                while m < el_x_len {
                    let xm = el_x[l][m];
                    let ym = el_y[l][m];
                    if (xm - xstart) / size == ((xm - xstart) / size).floor() {
                        let xx = ((xm - xstart) / size) as usize;
                        let yy = ((ym - ystart) / size) as usize;
                        let h1 = xyz[(xx, yy)];
                        if yy < xyz.height() - 1 {
                            let h2 = xyz[(xx, yy + 1)];
                            let h3 = h1 * (yy as f64 + 1.0 - (ym - ystart) / size)
                                + h2 * ((ym - ystart) / size - yy as f64);
                            h = (h3 / interval + 0.5).floor() * interval;
                        } else {
                            h = (h1 / interval + 0.5).floor() * interval;
                        }
                        break;
                    } else if m < el_x_len - 1
                        && (ym - ystart) / size == ((ym - ystart) / size).floor()
                    {
                        let xx = ((xm - xstart) / size) as usize;
                        let yy = ((ym - ystart) / size) as usize;
                        let h1 = xyz[(xx, yy)];
                        if xx < xyz.width() - 1 {
                            let h2 = xyz[(xx + 1, yy)];
                            let h3 = h1 * (xx as f64 + 1.0 - (xm - xstart) / size)
                                + h2 * ((xm - xstart) / size - xx as f64);
                            h = (h3 / interval + 0.5).floor() * interval;
                        } else {
                            h = (h1 / interval + 0.5).floor() * interval;
                        }
                        break;
                    } else {
                        m += 1;
                    }
                }
            }
            if !skip
                && el_x_len < depression_length
                && el_x[l].first() == el_x[l].last()
                && el_y[l].first() == el_y[l].last()
            {
                let mut mm: isize = (((el_x_len - 1) as f64) / 3.0).floor() as isize - 1;
                if mm < 0 {
                    mm = 0;
                }
                let mut m = mm as usize;
                let mut x_avg = el_x[l][m];
                let mut y_avg = el_y[l][m];
                while m < el_x_len {
                    let xm = (el_x[l][m] - xstart) / size;
                    let ym = (el_y[l][m] - ystart) / size;
                    if m < el_x_len - 3
                        && ym == ym.floor()
                        && (xm - xm.floor()).abs() > 0.5
                        && ym.floor() != ((el_y[l][0] - ystart) / size).floor()
                        && xm.floor() != ((el_x[l][0] - xstart) / size).floor()
                    {
                        x_avg = xm.floor() * size + xstart;
                        y_avg = el_y[l][m].floor();
                        m += el_x_len;
                    }
                    m += 1;
                }
                let foo_x = ((x_avg - xstart) / size) as usize;
                let foo_y = ((y_avg - ystart) / size) as usize;

                let h_center = xyz[(foo_x, foo_y)];

                let xtest = foo_x as f64 * size + xstart;
                let ytest = foo_y as f64 * size + ystart;

                let inside = Ring::from_xy(&el_x[l], &el_y[l]).contains(Point2::new(xtest, ytest));
                depression = 1;
                if (h_center < h && inside) || (h_center > h && !inside) {
                    depression = -1;
                    write!(&mut depr_fp, "{},{}", el_x[l][0], el_y[l][0])
                        .expect("Unable to write file");
                    for k in 1..el_x[l].len() {
                        write!(&mut depr_fp, "|{},{}", el_x[l][k], el_y[l][k])
                            .expect("Unable to write file");
                    }
                    writeln!(&mut depr_fp).expect("Unable to write file");
                }
                if !skip {
                    // Check if knoll is distinct enough
                    let mut steepcounter = 0;
                    let mut minele = f64::MAX;
                    let mut maxele = f64::MIN;
                    for k in 0..(el_x_len - 1) {
                        let xx = ((el_x[l][k] - xstart) / size + 0.5) as usize;
                        let yy = ((el_y[l][k] - ystart) / size + 0.5) as usize;
                        let ss = steepness[(xx, yy)];
                        if minele > h - 0.5 * ss {
                            minele = h - 0.5 * ss;
                        }
                        if maxele < h + 0.5 * ss {
                            maxele = h + 0.5 * ss;
                        }
                        if ss > 1.0 {
                            steepcounter += 1;
                        }
                    }

                    if (steepcounter as f64) < 0.4 * (el_x_len as f64 - 1.0)
                        && el_x_len < 41
                        && depression as f64 * h_center - 1.9 < minele
                    {
                        if maxele - 0.45 * scalefactor * inidotknolls < minele {
                            skip = true;
                        }
                        if el_x_len < 33 && maxele - 0.75 * scalefactor * inidotknolls < minele {
                            skip = true;
                        }
                        if el_x_len < 19 && maxele - 0.9 * scalefactor * inidotknolls < minele {
                            skip = true;
                        }
                    }
                    if (steepcounter as f64) < inidotknolls * (el_x_len - 1) as f64 && el_x_len < 15
                    {
                        skip = true;
                    }
                }
            }
            if el_x_len < 5 {
                skip = true;
            }
            if !skip && el_x_len < 15 {
                // dot knoll
                let mut x_avg = 0.0;
                let mut y_avg = 0.0;
                for k in 0..(el_x_len - 1) {
                    x_avg += el_x[l][k];
                    y_avg += el_y[l][k];
                }
                x_avg /= (el_x_len - 1) as f64;
                y_avg /= (el_x_len - 1) as f64;

                dotknolls.push(super::knolls::Dotknoll {
                    x: x_avg,
                    y: y_avg,
                    is_knoll: depression == 1,
                });

                skip = true;
            }

            if !skip {
                // not skipped, lets save first coordinate pair for later form line knoll PIP analysis
                write!(&mut knollhead_fp, "{} {}\r\n", el_x[l][0], el_y[l][0])
                    .expect("Unable to write to file");
                // One direction for every contour, downhill on the right. The tracer's direction
                // depends on where it happened to start, so the same contour can run either way
                // in two tiles that share it -- and the smoothing below is not symmetric, so it
                // then comes out differently on the two sides of the tile edge. A fixed
                // direction also tells a consumer which side is downhill, which is what a slope
                // line on a depression needs.
                if downhill_on_left(&xyz, xstart, ystart, size, &el_x[l], &el_y[l], h) {
                    el_x[l].reverse();
                    el_y[l].reverse();
                }
                // adaptive generalization
                if el_x_len > 101 {
                    let mut newx: Vec<f64> = vec![];
                    let mut newy: Vec<f64> = vec![];
                    newx.push(el_x[l][0]);
                    newy.push(el_y[l][0]);

                    // On flat ground a vertex is kept where the line enters a new 4 m cell of a
                    // fixed world grid. The spacing is the same ~4 m a "4 m from the last kept
                    // vertex" walk gives, but the choice depends on where the vertex is and not
                    // on where the walk started -- which differs between two tiles that share
                    // the line, and used to leave their halves visibly apart at the tile edge.
                    let cell4 = |k: usize| {
                        (
                            (el_x[l][k] / 4.0).floor() as i64,
                            (el_y[l][k] / 4.0).floor() as i64,
                        )
                    };
                    for k in 1..(el_x_len - 1) {
                        let xx = ((el_x[l][k] - xstart) / size + 0.5) as usize;
                        let yy = ((el_y[l][k] - ystart) / size + 0.5) as usize;
                        let ss = steepness[(xx, yy)];
                        if ss.is_nan() || ss < 0.5 {
                            if cell4(k) != cell4(k - 1) {
                                newx.push(el_x[l][k]);
                                newy.push(el_y[l][k]);
                            }
                        } else {
                            newx.push(el_x[l][k]);
                            newy.push(el_y[l][k]);
                        }
                    }
                    newx.push(el_x[l][el_x_len - 1]);
                    newy.push(el_y[l][el_x_len - 1]);

                    el_x[l].clear();
                    el_x[l].append(&mut newx);
                    el_y[l].clear();
                    el_y[l].append(&mut newy);
                    el_x_len = el_x[l].len();
                }
                // Smoothing
                let mut dx: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut dy: Vec<f64> = vec![f64::NAN; el_x_len];

                for k in 2..(el_x_len - 3) {
                    dx[k] = (el_x[l][k - 2]
                        + el_x[l][k - 1]
                        + el_x[l][k]
                        + el_x[l][k + 1]
                        + el_x[l][k + 2]
                        + el_x[l][k + 3])
                        / 6.0;
                    dy[k] = (el_y[l][k - 2]
                        + el_y[l][k - 1]
                        + el_y[l][k]
                        + el_y[l][k + 1]
                        + el_y[l][k + 2]
                        + el_y[l][k + 3])
                        / 6.0;
                }

                let mut xa: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut ya: Vec<f64> = vec![f64::NAN; el_x_len];
                for k in 1..(el_x_len - 1) {
                    xa[k] = (el_x[l][k - 1] + el_x[l][k] / (0.01 + smoothing) + el_x[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    ya[k] = (el_y[l][k - 1] + el_y[l][k] / (0.01 + smoothing) + el_y[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }

                if el_x[l].first() == el_x[l].last() && el_y[l].first() == el_y[l].last() {
                    let vx = (el_x[l][1] + el_x[l][0] / (0.01 + smoothing) + el_x[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (el_y[l][1] + el_y[l][0] / (0.01 + smoothing) + el_y[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    xa[0] = vx;
                    ya[0] = vy;
                    xa[el_x_len - 1] = vx;
                    ya[el_x_len - 1] = vy;
                } else {
                    xa[0] = el_x[l][0];
                    ya[0] = el_y[l][0];
                    xa[el_x_len - 1] = el_x[l][el_x_len - 1];
                    ya[el_x_len - 1] = el_y[l][el_x_len - 1];
                }
                for k in 1..(el_x_len - 1) {
                    el_x[l][k] = (xa[k - 1] + xa[k] / (0.01 + smoothing) + xa[k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    el_y[l][k] = (ya[k - 1] + ya[k] / (0.01 + smoothing) + ya[k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }
                if xa.first() == xa.last() && ya.first() == ya.last() {
                    let vx = (xa[1] + xa[0] / (0.01 + smoothing) + xa[el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (ya[1] + ya[0] / (0.01 + smoothing) + ya[el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    el_x[l][0] = vx;
                    el_y[l][0] = vy;
                    el_x[l][el_x_len - 1] = vx;
                    el_y[l][el_x_len - 1] = vy;
                } else {
                    el_x[l][0] = xa[0];
                    el_y[l][0] = ya[0];
                    el_x[l][el_x_len - 1] = xa[el_x_len - 1];
                    el_y[l][el_x_len - 1] = ya[el_x_len - 1];
                }

                for k in 1..(el_x_len - 1) {
                    xa[k] = (el_x[l][k - 1] + el_x[l][k] / (0.01 + smoothing) + el_x[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    ya[k] = (el_y[l][k - 1] + el_y[l][k] / (0.01 + smoothing) + el_y[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }

                if el_x[l].first() == el_x[l].last() && el_y[l].first() == el_y[l].last() {
                    let vx = (el_x[l][1] + el_x[l][0] / (0.01 + smoothing) + el_x[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (el_y[l][1] + el_y[l][0] / (0.01 + smoothing) + el_y[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    xa[0] = vx;
                    ya[0] = vy;
                    xa[el_x_len - 1] = vx;
                    ya[el_x_len - 1] = vy;
                } else {
                    xa[0] = el_x[l][0];
                    ya[0] = el_y[l][0];
                    xa[el_x_len - 1] = el_x[l][el_x_len - 1];
                    ya[el_x_len - 1] = el_y[l][el_x_len - 1];
                }

                #[allow(clippy::manual_memcpy)]
                for k in 0..el_x_len {
                    el_x[l][k] = xa[k];
                    el_y[l][k] = ya[k];
                }

                let mut dx2: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut dy2: Vec<f64> = vec![f64::NAN; el_x_len];
                for k in 2..(el_x_len - 3) {
                    dx2[k] = (el_x[l][k - 2]
                        + el_x[l][k - 1]
                        + el_x[l][k]
                        + el_x[l][k + 1]
                        + el_x[l][k + 2]
                        + el_x[l][k + 3])
                        / 6.0;
                    dy2[k] = (el_y[l][k - 2]
                        + el_y[l][k - 1]
                        + el_y[l][k]
                        + el_y[l][k + 1]
                        + el_y[l][k + 2]
                        + el_y[l][k + 3])
                        / 6.0;
                }
                for k in 3..(el_x_len - 3) {
                    let vx = el_x[l][k] + (dx[k] - dx2[k]) * curviness;
                    let vy = el_y[l][k] + (dy[k] - dy2[k]) * curviness;
                    el_x[l][k] = vx;
                    el_y[l][k] = vy;
                }

                let mut layer = if depression == -1 {
                    Classification::Depression
                } else {
                    Classification::Contour
                };

                if indexcontours != 0.0
                    && (((h / interval + 0.5).floor() * interval) / indexcontours).floor()
                        - ((h / interval + 0.5).floor() * interval) / indexcontours
                        == 0.0
                {
                    // "Add" Index flag
                    layer = match layer {
                        Classification::Contour => Classification::ContourIndex,
                        Classification::Depression => Classification::DepressionIndex,
                        other => other,
                    };
                }
                if formline > 0.0
                    && (((h / interval + 0.5).floor() * interval) / (2.0 * interval)).floor()
                        - ((h / interval + 0.5).floor() * interval) / (2.0 * interval)
                        != 0.0
                {
                    // "Add" Intermed flag
                    layer = match layer {
                        Classification::Contour => Classification::ContourIntermed,
                        Classification::ContourIndex => Classification::ContourIndexIntermed,
                        Classification::Depression => Classification::DepressionIntermed,
                        Classification::DepressionIndex => Classification::DepressionIndexIntermed,
                        other => other,
                    };
                }

                out2_lines.push(
                    el_x[l]
                        .iter()
                        .zip(el_y[l].iter())
                        .map(|(&x, &y)| Point3::new(x, y, h))
                        .collect(),
                    (layer, h),
                );

                // ISOM 2017-2, symbol 101: "a depression has to have at least one slope
                // line". Without one a depression ring is indistinguishable from a knoll
                // — the reader cannot tell which way the ground goes. KP classified
                // depressions but never drew the tick, and the vector output then folded
                // `depression` into plain 101, so the distinction was lost for good.
                // if the return element happens to be a small depression, lets remove the
                // original countour
                if config.decorate_depressions
                    && layer.is_depression()
                    && let Some((form, class)) = decorate_depression(&el_x[l], &el_y[l], h)
                {
                    if class == Classification::SmallDepression {
                        out2_lines.pop();
                    }
                    out2_lines.push(form, (class, h));
                }
            } // -- if not dotkoll
        }
    }

    crate::util::write_object(
        &mut fs.create(tmpfolder.join("dotknolls.bin"))?,
        &super::knolls::Dotknolls { dotknolls },
    )?;

    let out2_dxf = BinaryDxf::new(input_bounds, vec![out2_lines.into()]);

    let output = tmpfolder.join("out2.dxf.bin");
    let mut fp = fs.create(output).expect("Unable to create file");
    out2_dxf.to_writer(&mut fp)?;

    if config.output_dxf {
        out2_dxf.to_dxf(&mut fs.create(tmpfolder.join("out2.dxf"))?)?;
    }

    info!("Done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Classification;
    use super::decorate_depression;

    /// Every contour leaves smoothjoin with its downhill side on the right: that is what
    /// makes the smoothing the same in two tiles that trace the line in opposite directions,
    /// and what tells a consumer which way a slope line points.
    #[test]
    fn downhill_side_is_found() {
        use super::downhill_on_left;
        use crate::vec2d::Vec2D;
        // ground rising to the north: height = 2 m per cell of y
        let mut grid = Vec2D::new(10, 10, 0.0);
        for x in 0..10 {
            for y in 0..10 {
                grid[(x, y)] = 2.0 * y as f64;
            }
        }
        // a contour at 9 m, running east: downhill (south) is on its right
        let x: Vec<f64> = (1..9).map(|i| i as f64).collect();
        let y = vec![4.5; x.len()];
        assert!(!downhill_on_left(&grid, 0.0, 0.0, 1.0, &x, &y, 9.0));
        // the same contour running west has downhill on its left
        let xr: Vec<f64> = x.iter().rev().copied().collect();
        assert!(downhill_on_left(&grid, 0.0, 0.0, 1.0, &xr, &y, 9.0));
    }
    use crate::geometry::{Point2, Ring};
    // A closed ring approximating a circle of the given ground radius, in metres.
    fn ring(radius: f64) -> (Vec<f64>, Vec<f64>) {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..=24 {
            let a = i as f64 / 24.0 * std::f64::consts::TAU;
            x.push(100.0 + radius * a.cos());
            y.push(200.0 + radius * a.sin());
        }
        (x, y)
    }

    #[test]
    fn the_tick_points_into_the_depression() {
        let (x, y) = ring(20.0);
        let (tick, class) =
            decorate_depression(&x, &y, 12.5).expect("a 40 m depression carries a slope line");
        assert!(class == Classification::SlopeLine);
        let (start, end) = (&tick[0], &tick[1]);
        let d_start = ((start.x - 100.0).powi(2) + (start.y - 200.0).powi(2)).sqrt();
        let d_end = ((end.x - 100.0).powi(2) + (end.y - 200.0).powi(2)).sqrt();
        assert!(
            d_end < d_start,
            "tick must point inward: {d_start} -> {d_end}"
        );
        assert_eq!(start.z, 12.5);
    }

    #[test]
    fn the_tick_is_the_isom_length() {
        let (x, y) = ring(20.0);
        let (tick, _class) = decorate_depression(&x, &y, 0.0).unwrap();
        // 0.4 OM -> 0.6 mm at 1:10,000 -> 6 m on the ground.
        let len = ((tick[1].x - tick[0].x).powi(2) + (tick[1].y - tick[0].y).powi(2)).sqrt();
        assert!((len - 6.0).abs() < 1e-9, "{len}");
    }

    #[test]
    fn a_ring_below_the_isom_minimum_gets_no_tick() {
        // Under 16.5 x 10.5 m a contour depression may not be drawn at all.
        let (x, y) = ring(4.0);
        let (_tick, class) = decorate_depression(&x, &y, 0.0).unwrap();
        assert!(class == Classification::SmallDepression);
    }

    #[test]
    fn winding_does_not_flip_the_tick_outward() {
        let (x, y) = ring(20.0);
        let (rx, ry): (Vec<f64>, Vec<f64>) = (
            x.iter().rev().copied().collect(),
            y.iter().rev().copied().collect(),
        );
        let (tick, _class) = decorate_depression(&rx, &ry, 0.0).unwrap();
        let d_start = ((tick[0].x - 100.0).powi(2) + (tick[0].y - 200.0).powi(2)).sqrt();
        let d_end = ((tick[1].x - 100.0).powi(2) + (tick[1].y - 200.0).powi(2)).sqrt();
        assert!(d_end < d_start, "reversed winding must still point inward");
    }

    /// A crescent: its centroid falls OUTSIDE the ring, which is what sent the tick to
    /// the wrong side on Nyrup Hegn. Shaped like a C opening to the right.
    fn crescent() -> (Vec<f64>, Vec<f64>) {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..=40 {
            // outer arc, 270 degrees
            let a = i as f64 / 40.0 * (1.5 * std::f64::consts::PI) + 0.25 * std::f64::consts::PI;
            x.push(100.0 + 30.0 * a.cos());
            y.push(200.0 + 30.0 * a.sin());
        }
        for i in (0..=40).rev() {
            // inner arc back
            let a = i as f64 / 40.0 * (1.5 * std::f64::consts::PI) + 0.25 * std::f64::consts::PI;
            x.push(100.0 + 20.0 * a.cos());
            y.push(200.0 + 20.0 * a.sin());
        }
        x.push(x[0]);
        y.push(y[0]);
        (x, y)
    }

    #[test]
    fn a_crescent_ring_still_gets_an_inward_tick() {
        let (x, y) = crescent();
        let (tick, _class) = decorate_depression(&x, &y, 0.0).expect("crescent is big enough");
        assert!(
            Ring::from_xy(&x, &y).contains(Point2::new(tick[1].x, tick[1].y)),
            "tick end must be inside the ring, not outside it"
        );
    }

    #[test]
    fn the_tick_keeps_clear_of_the_contour() {
        let (x, y) = crescent();
        let (tick, _class) = decorate_depression(&x, &y, 0.0).unwrap();
        // The crescent is 10 m wide, so a 6 m tick placed well has room to spare; the
        // failure this guards is a tick laid along or across the ring itself.
        assert!(Ring::from_xy(&x, &y).distance_to_point(Point2::new(tick[1].x, tick[1].y)) > 0.5);
    }
}
