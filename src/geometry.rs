//! This mod contains structs for storing and loading different types of geometry, like Polylines
//! and a list of Points.
//!
//! These types also have helpers for exporting them to DXF format.

/// A 2D point
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point2 {
    /// The x coordinate of this point.
    pub x: f64,
    /// The y coordinate of this point.
    pub y: f64,
}

impl Point2 {
    /// Create a new point from the given coordinates.
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A 3D point (eg. 2D + height)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point3 {
    /// The x coordinate of this point.
    pub x: f64,
    /// The y coordinate of this point.
    pub y: f64,
    /// The z coordinate of this point (height).
    pub z: f64,
}

impl Point3 {
    /// Create a new point from the given coordinates.
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
}

/// A collection of points with associated classification. This classification is also used to put
/// the DXF objects into separate layers.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Points {
    points: Vec<Point2>,
    classification: Vec<Classification>,
}

impl Points {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            classification: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            points: Vec::with_capacity(capacity),
            classification: Vec::with_capacity(capacity),
        }
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Add a point to this collection.
    pub fn push(&mut self, point: Point2, class: Classification) {
        self.points.push(point);
        self.classification.push(class);
    }

    /// Iterate over the points in this collection.
    pub fn iter(&self) -> impl Iterator<Item = (&Point2, &Classification)> {
        self.points.iter().zip(self.classification.iter())
    }
}

impl IntoIterator for Points {
    type Item = (Point2, Classification);

    type IntoIter = std::iter::Zip<std::vec::IntoIter<Point2>, std::vec::IntoIter<Classification>>;

    fn into_iter(self) -> Self::IntoIter {
        self.points.into_iter().zip(self.classification)
    }
}

/// A collection polylines with associated classification.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Polylines<P, C> {
    polylines: Vec<Vec<P>>, // TODO: flatten to single vector?
    classification: Vec<C>,
}

impl<P, C> Polylines<P, C> {
    pub fn new() -> Self {
        Self {
            polylines: Vec::new(),
            classification: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            polylines: Vec::with_capacity(capacity),
            classification: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, polyline: Vec<P>, class: C) {
        self.polylines.push(polyline);
        self.classification.push(class);
    }

    pub fn pop(&mut self) {
        self.polylines.pop();
        self.classification.pop();
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<P>, &C)> {
        self.polylines.iter().zip(self.classification.iter())
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.polylines.len()
    }
}

impl<P, C> IntoIterator for Polylines<P, C> {
    type Item = (Vec<P>, C);

    type IntoIter = std::iter::Zip<std::vec::IntoIter<Vec<P>>, std::vec::IntoIter<C>>;

    fn into_iter(self) -> Self::IntoIter {
        self.polylines.into_iter().zip(self.classification)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Geometry {
    Points(Points),

    /// Polylines2 is used for 2D polylines with a classification.
    Polylines2(Polylines<Point2, Classification>),

    /// Polylines3 is used for 2D polylines with a height (z coordinate).
    Polylines3(Polylines<Point3, (Classification, f64)>), // Classification + height
}

impl From<Points> for Geometry {
    fn from(points: Points) -> Self {
        Geometry::Points(points)
    }
}
impl From<Polylines<Point2, Classification>> for Geometry {
    fn from(polylines: Polylines<Point2, Classification>) -> Self {
        Geometry::Polylines2(polylines)
    }
}
impl From<Polylines<Point3, (Classification, f64)>> for Geometry {
    fn from(polylines: Polylines<Point3, (Classification, f64)>) -> Self {
        Geometry::Polylines3(polylines)
    }
}

/// The version of the BinaryDxf file format. If any content of the [`BinaryDxf`] struct changes,
/// including any sub-fields (basically anything in this mod) we need to increase this version.
const BINARY_DXF_VERSION: usize = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BinaryDxf {
    /// the version of the program that created this file, used to detect stale temp files
    version: String,
    bounds: Bounds,
    data: Vec<Geometry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub xmin: f64,
    pub xmax: f64,
    pub ymin: f64,
    pub ymax: f64,
}

impl Bounds {
    pub fn new(xmin: f64, xmax: f64, ymin: f64, ymax: f64) -> Self {
        Self {
            xmin,
            xmax,
            ymin,
            ymax,
        }
    }
}

impl BinaryDxf {
    pub fn new(bounds: Bounds, data: Vec<Geometry>) -> Self {
        Self {
            version: BINARY_DXF_VERSION.to_string(),
            bounds,
            data,
        }
    }

    pub fn bounds(&self) -> &Bounds {
        &self.bounds
    }

    /// Get the points in this geometry, or [`None`] if does not contain [`Polylines`] data.
    pub fn take_geometry(self) -> Vec<Geometry> {
        self.data
    }

    /// Serialize this object to a writer.
    pub fn to_writer<W: std::io::Write>(&self, writer: &mut W) -> anyhow::Result<()> {
        crate::util::write_object(writer, self)
    }
    /// Read this object from a reader. Returns an error if the version does not match.
    pub fn from_reader<R: std::io::Read>(reader: &mut R) -> anyhow::Result<Self> {
        let object: Self = crate::util::read_object(reader)?;

        // Prevously we were using the crate version, which is a string starting
        // with 2.X.X, but now we use a simple integer version. Version 1 supports all those
        // "2.X.X" versions as well.
        if (object.version == BINARY_DXF_VERSION.to_string())
            || (BINARY_DXF_VERSION == 1 && object.version.starts_with("2."))
        {
            Ok(object)
        } else {
            anyhow::bail!(
                "This DXF.BIN file version is not supported by this executable. Please re-run this command with an executable that supports this version (dxf.bin file version: {})",
                object.version,
            );
        }
    }

    /// Write this geometry to a DXF file.
    pub fn to_dxf<W: std::io::Write>(&self, writer: &mut W) -> anyhow::Result<()> {
        write!(
            writer,
            "  0\r\nSECTION\r\n  2\r\nHEADER\r\n  9\r\n$EXTMIN\r\n 10\r\n{}\r\n 20\r\n{}\r\n  9\r\n$EXTMAX\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\nENDSEC\r\n  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\n",
            self.bounds.xmin, self.bounds.ymin, self.bounds.xmax, self.bounds.ymax
        )?;

        for geom in &self.data {
            match geom {
                Geometry::Points(points) => {
                    for (point, class) in points.points.iter().zip(&points.classification) {
                        let layer = class.to_layer();

                        write!(
                            writer,
                            "POINT\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 50\r\n0\r\n  0\r\n",
                            point.x, point.y
                        )?;
                    }
                }
                Geometry::Polylines2(polylines) => {
                    for (polyline, class) in
                        polylines.polylines.iter().zip(&polylines.classification)
                    {
                        let layer = class.to_layer();
                        if class.is_area() {
                            // closed polyline (flag 70=1) so importers treat it as an area ring
                            write!(
                                writer,
                                "POLYLINE\r\n 66\r\n1\r\n  8\r\n{layer}\r\n 70\r\n1\r\n  0\r\n"
                            )?;
                        } else {
                            write!(writer, "POLYLINE\r\n 66\r\n1\r\n  8\r\n{layer}\r\n  0\r\n")?;
                        }

                        for p in polyline {
                            write!(
                                writer,
                                "VERTEX\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\n",
                                p.x, p.y,
                            )?;
                        }
                        write!(writer, "SEQEND\r\n  0\r\n")?;
                    }
                }
                Geometry::Polylines3(polylines) => {
                    for (polyline, (class, height)) in
                        polylines.polylines.iter().zip(&polylines.classification)
                    {
                        let layer = class.to_layer();

                        write!(
                            writer,
                            "POLYLINE\r\n 66\r\n1\r\n  8\r\n{layer}\r\n 38\r\n{height}\r\n  0\r\n"
                        )?;

                        for p in polyline {
                            write!(
                                writer,
                                "VERTEX\r\n  8\r\n{}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 30\r\n{}\r\n  0\r\n",
                                layer, p.x, p.y, height
                            )?;
                        }
                        write!(writer, "SEQEND\r\n  0\r\n")?;
                    }
                }
            }
        }

        writer.write_all("ENDSEC\r\n  0\r\nEOF\r\n".as_bytes())?;
        Ok(())
    }
}

/// Classification used for contour generation
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Classification {
    /// Used in first contour generation step
    ContourSimple,

    /// Used in second contour generation step (smoothjoin)
    Contour,
    ContourIndex,
    ContourIntermed,
    ContourIndexIntermed,

    Depression,
    DepressionIndex,
    DepressionIntermed,
    DepressionIndexIntermed,

    /// Use for formlines (generated in render)
    Formline,
    FormlineDepression,

    /// Used in dotknoll detections
    Dotknoll,
    Udepression,
    UglyDotknoll,
    UglyUdepression,

    /// Comes from knolldetector
    Knoll1010,

    /// Used for cliff generations
    Cliff2,
    Cliff3,
    Cliff4,

    /// The tick drawn inside a depression contour so it reads as a depression rather
    /// than a knoll. ISOM 2017-2 makes it part of symbol 101 ("a depression has to have
    /// at least one slope line"), not a symbol of its own, so it carries contour weight
    /// and maps to 101 downstream. Generated in merge, alongside the ring it belongs to.
    SlopeLine,
    SmallDepression,

    /// ISOM vegetation area symbols (closed polygon rings).
    /// Keep at the end so bincode variant indices of older variants stay stable.
    Veg403,
    Veg406,
    Veg407,
    Veg408,
    Veg410,
}

impl Classification {
    /// Get the layer name for this classification.
    pub fn to_layer(&self) -> &str {
        match self {
            Self::ContourSimple => "cont",

            Self::Contour => "contour",
            Self::ContourIndex => "contour_index",
            Self::ContourIntermed => "contour_intermed",
            Self::ContourIndexIntermed => "contour_index_intermed",

            Self::Depression => "depression",
            Self::DepressionIndex => "depression_index",
            Self::DepressionIntermed => "depression_intermed",
            Self::DepressionIndexIntermed => "depression_index_intermed",

            Self::Formline => "formline",
            Self::FormlineDepression => "formline_depression",

            Self::Dotknoll => "dotknoll",
            Self::Udepression => "udepression",
            Self::UglyDotknoll => "uglydotknoll",
            Self::UglyUdepression => "uglyudepression",

            Self::Knoll1010 => "1010",

            Self::Cliff2 => "cliff2",
            Self::Cliff3 => "cliff3",
            Self::Cliff4 => "cliff4",

            Self::SlopeLine => "slope_line",
            Self::SmallDepression => "small_depression",
            Self::Veg403 => "403",
            Self::Veg406 => "406",
            Self::Veg407 => "407",
            Self::Veg408 => "408",
            Self::Veg410 => "410",
        }
    }

    /// Whether this classification is an area symbol whose polylines are closed rings.
    pub fn is_area(&self) -> bool {
        matches!(
            self,
            Self::Veg403 | Self::Veg406 | Self::Veg407 | Self::Veg408 | Self::Veg410
        )
    }

    pub fn is_contour(&self) -> bool {
        matches!(
            self,
            Self::Contour | Self::ContourIndex | Self::ContourIntermed | Self::ContourIndexIntermed
        )
    }

    pub fn is_depression(&self) -> bool {
        matches!(
            self,
            Self::Depression
                | Self::DepressionIndex
                | Self::DepressionIntermed
                | Self::DepressionIndexIntermed
        )
    }

    pub fn is_index(&self) -> bool {
        matches!(
            self,
            Self::ContourIndex
                | Self::ContourIndexIntermed
                | Self::DepressionIndex
                | Self::DepressionIndexIntermed
        )
    }

    pub fn is_intermed(&self) -> bool {
        matches!(
            self,
            Self::ContourIntermed
                | Self::ContourIndexIntermed
                | Self::DepressionIntermed
                | Self::DepressionIndexIntermed
        )
    }
}

/// A closed contour line, first vertex repeated last.
#[derive(Debug, Clone)]
pub struct Ring {
    points: Vec<Point2>,
}

impl Ring {
    /// Build from parallel x/y slices. Closes the ring (pushes a copy of the first vertex)
    /// only when the last vertex differs from the first, so an already closed input gets no
    /// extra edge. Panics if the slices differ in length.
    pub fn from_xy(x: &[f64], y: &[f64]) -> Self {
        assert_eq!(x.len(), y.len(), "x and y must have the same length");
        let mut points: Vec<Point2> = x
            .iter()
            .zip(y.iter())
            .map(|(&x, &y)| Point2::new(x, y))
            .collect();
        if let (Some(first), Some(last)) = (points.first(), points.last())
            && (first.x != last.x || first.y != last.y)
        {
            let first = first.clone();
            points.push(first);
        }
        Self { points }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.points.len()
    }

    /// Even-odd ray cast towards -x over consecutive edges (prev -> cur). Exact for concave
    /// shapes. Uses exactly the legacy arithmetic so results are bit-identical to the inline
    /// loops it replaces. An empty ring contains nothing.
    pub fn contains(&self, p: Point2) -> bool {
        let mut hit = 0;
        for w in self.points.windows(2) {
            let (x0, y0) = (w[0].x, w[0].y);
            let (x1, y1) = (w[1].x, w[1].y);
            if ((y0 <= p.y && p.y < y1) || (y1 <= p.y && p.y < y0))
                && (p.x < (x1 - x0) * (p.y - y0) / (y1 - y0) + x0)
            {
                hit += 1;
            }
        }
        hit % 2 == 1
    }

    /// Shortest distance from `p` to any edge of the ring.
    ///
    /// Legacy behaviour kept on purpose: an empty ring yields `f64::MAX.sqrt()`.
    pub fn distance_to_point(&self, p: Point2) -> f64 {
        let n = self.points.len();
        let mut best = f64::MAX;
        for i in 0..n {
            let j = (i + 1) % n;
            let (ax, ay) = (self.points[i].x, self.points[i].y);
            let (bx, by) = (self.points[j].x, self.points[j].y);
            let (dx, dy) = (bx - ax, by - ay);
            let l2 = dx * dx + dy * dy;
            let t = if l2 == 0.0 {
                0.0
            } else {
                (((p.x - ax) * dx + (p.y - ay) * dy) / l2).clamp(0.0, 1.0)
            };
            let (cx, cy) = (ax + t * dx, ay + t * dy);
            best = best.min((p.x - cx).powi(2) + (p.y - cy).powi(2));
        }
        best.sqrt()
    }
}

/// Join polylines whose quantized endpoints (1 mm) meet, end to end.
///
/// Lines with `len() >= max_vertices` are emptied and never joined (`usize::MAX` = no limit).
/// Returns one `Vec<Point2>` per input slot, same length and order: absorbed donors and
/// dropped lines come back empty so callers keep indexing by input position.
///
/// Slot 0 is never chosen as a join partner: the lookup tables use index 0 as "no line".
/// This is a legacy quirk kept deliberately so output stays identical; see ticket 18.
pub fn join_polylines<C>(lines: &Polylines<Point2, C>, max_vertices: usize) -> Vec<Vec<Point2>> {
    use rustc_hash::FxHashMap;

    // Internal type used to index into the hashmaps and vectors.
    // Since using f64 coordinates directly has problems with rounding (and do not impl Eq and
    // Hash), we can use an integer representation of the coordinates to index into the HashMaps.
    // By multiplying by 1000, we can keep a precision of 3 decimal places, which is sufficient for
    // what we need.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct Key {
        x: i64,
        y: i64,
    }
    impl Key {
        fn new(x: f64, y: f64) -> Self {
            Key {
                x: (x * 1000.0) as i64,
                y: (y * 1000.0) as i64,
            }
        }
        /// Just a unique key for the case where we don't have a valid point.
        fn none() -> Self {
            Self {
                x: i64::MAX,
                y: i64::MAX,
            }
        }
    }

    let mut heads1: FxHashMap<Key, usize> = FxHashMap::default();
    let mut heads2: FxHashMap<Key, usize> = FxHashMap::default();
    let mut heads = Vec::<Key>::with_capacity(lines.len());
    let mut tails = Vec::<Key>::with_capacity(lines.len());
    let mut out = Vec::<Vec<Point2>>::with_capacity(lines.len());

    for (j, (line, _c)) in lines.iter().enumerate() {
        if line.len() < max_vertices {
            let first = line.first().unwrap();
            let last = line.last().unwrap();

            let head = Key::new(first.x, first.y);
            let tail = Key::new(last.x, last.y);

            heads.push(head);
            tails.push(tail);
            out.push(line.clone());

            if *heads1.get(&head).unwrap_or(&0) == 0 {
                heads1.insert(head, j);
            } else {
                heads2.insert(head, j);
            }
            if *heads1.get(&tail).unwrap_or(&0) == 0 {
                heads1.insert(tail, j);
            } else {
                heads2.insert(tail, j);
            }
        } else {
            heads.push(Key::none());
            tails.push(Key::none());
            out.push(vec![]);
        }
    }

    for l in 0..lines.len() {
        let mut to_join = 0;
        if !out[l].is_empty() {
            let mut end_loop = false;
            while !end_loop {
                let tmp = *heads1.get(&heads[l]).unwrap_or(&0);
                if tmp != 0 && tmp != l && !out[tmp].is_empty() {
                    to_join = tmp;
                } else {
                    let tmp = *heads2.get(&heads[l]).unwrap_or(&0);
                    if tmp != 0 && tmp != l && !out[tmp].is_empty() {
                        to_join = tmp;
                    } else {
                        let tmp = *heads2.get(&tails[l]).unwrap_or(&0);
                        if tmp != 0 && tmp != l && !out[tmp].is_empty() {
                            to_join = tmp;
                        } else {
                            let tmp = *heads1.get(&tails[l]).unwrap_or(&0);
                            if tmp != 0 && tmp != l && !out[tmp].is_empty() {
                                to_join = tmp;
                            } else {
                                end_loop = true;
                            }
                        }
                    }
                }
                if !end_loop {
                    if tails[l] == heads[to_join] {
                        heads2.insert(tails[l], 0);
                        heads1.insert(tails[l], 0);
                        let mut donor = out[to_join].clone();
                        out[l].append(&mut donor);
                        tails[l] = tails[to_join];
                        out[to_join].clear();
                    } else if tails[l] == tails[to_join] {
                        heads2.insert(tails[l], 0);
                        heads1.insert(tails[l], 0);
                        let mut donor = out[to_join].clone();
                        donor.reverse();
                        out[l].append(&mut donor);
                        tails[l] = heads[to_join];
                        out[to_join].clear();
                    } else if heads[l] == tails[to_join] {
                        heads2.insert(heads[l], 0);
                        heads1.insert(heads[l], 0);
                        let donor = out[to_join].clone();
                        out[l].splice(0..0, donor);
                        heads[l] = heads[to_join];
                        out[to_join].clear();
                    } else if heads[l] == heads[to_join] {
                        heads2.insert(heads[l], 0);
                        heads1.insert(heads[l], 0);
                        let mut donor = out[to_join].clone();
                        donor.reverse();
                        out[l].splice(0..0, donor);
                        heads[l] = tails[to_join];
                        out[to_join].clear();
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square_closed() -> Ring {
        Ring::from_xy(&[0.0, 1.0, 1.0, 0.0, 0.0], &[0.0, 0.0, 1.0, 1.0, 0.0])
    }

    fn pl(lines: &[&[(f64, f64)]]) -> Polylines<Point2, ()> {
        let mut out = Polylines::new();
        for line in lines {
            out.push(line.iter().map(|&(x, y)| Point2::new(x, y)).collect(), ());
        }
        out
    }

    fn xy(line: &[Point2]) -> Vec<(f64, f64)> {
        line.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn ring_contains_unit_square() {
        let ring = unit_square_closed();
        assert!(ring.contains(Point2::new(0.5, 0.5)));
        assert!(!ring.contains(Point2::new(2.0, 0.5)));
        assert!(!ring.contains(Point2::new(-1.0, 0.5)));
    }

    #[test]
    fn ring_contains_concave_l_shape() {
        let ring = Ring::from_xy(
            &[0.0, 2.0, 2.0, 1.0, 1.0, 0.0, 0.0],
            &[0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 0.0],
        );
        assert!(ring.contains(Point2::new(0.5, 1.5)));
        assert!(ring.contains(Point2::new(1.5, 0.5)));
        // the notch
        assert!(!ring.contains(Point2::new(1.5, 1.5)));
    }

    /// Pins the legacy half-open convention. (0.5, 0) on the bottom edge: that edge has
    /// y0 == y1 and never matches, the right edge (1,0)->(1,1) does (0 <= 0 < 1 and 0.5 < 1),
    /// the left edge (0,1)->(0,0) does not (0.5 < 0 is false): one hit, inside. (1, 0.5) on
    /// the right edge: the right edge fails 1.0 < 1, the left edge fails 1.0 < 0: outside.
    #[test]
    fn ring_contains_point_on_edge_legacy_convention() {
        let ring = unit_square_closed();
        assert!(ring.contains(Point2::new(0.5, 0.0)));
        assert!(!ring.contains(Point2::new(1.0, 0.5)));
    }

    #[test]
    fn ring_contains_empty_is_false() {
        let ring = Ring::from_xy(&[], &[]);
        assert!(!ring.contains(Point2::new(0.0, 0.0)));
    }

    #[test]
    fn ring_from_xy_closes_open_input_only() {
        let open = Ring::from_xy(&[0.0, 1.0, 1.0, 0.0], &[0.0, 0.0, 1.0, 1.0]);
        let closed = unit_square_closed();
        assert_eq!(open.len(), 5);
        assert_eq!(closed.len(), 5);
        for p in [(0.5, 0.5), (2.0, 0.5), (-1.0, 0.5), (0.5, 0.0), (1.0, 0.5)] {
            assert_eq!(
                open.contains(Point2::new(p.0, p.1)),
                closed.contains(Point2::new(p.0, p.1))
            );
        }
    }

    #[test]
    fn ring_distance_to_point_unit_square() {
        let ring = unit_square_closed();
        assert_eq!(ring.distance_to_point(Point2::new(0.5, 2.0)), 1.0);
        assert_eq!(ring.distance_to_point(Point2::new(0.5, 0.25)), 0.25);
    }

    #[test]
    fn join_polylines_head_to_tail() {
        let lines = pl(&[&[(0.0, 0.0), (1.0, 0.0)], &[(1.0, 0.0), (2.0, 0.0)]]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(joined.len(), 2);
        assert_eq!(
            xy(&joined[0]),
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.0), (2.0, 0.0)]
        );
        assert!(joined[1].is_empty());
    }

    #[test]
    fn join_polylines_tail_to_tail_reverses_donor() {
        let lines = pl(&[&[(0.0, 0.0), (1.0, 0.0)], &[(2.0, 0.0), (1.0, 0.0)]]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(
            xy(&joined[0]),
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.0), (2.0, 0.0)]
        );
        assert!(joined[1].is_empty());
    }

    #[test]
    fn join_polylines_drops_lines_at_max_vertices() {
        let lines = pl(&[
            &[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)],
            &[(2.0, 0.0), (3.0, 0.0)],
        ]);
        let joined = join_polylines(&lines, 3);
        assert!(joined[0].is_empty());
        assert_eq!(xy(&joined[1]), vec![(2.0, 0.0), (3.0, 0.0)]);
    }

    #[test]
    fn join_polylines_empty_input() {
        let lines: Polylines<Point2, ()> = Polylines::new();
        assert!(join_polylines(&lines, usize::MAX).is_empty());
    }

    /// Pins the legacy sentinel: slot 0 is "no line" in the lookup tables, so line 0's
    /// registration at K is overwritten by line 1, line 2 lands in the second table, and
    /// after line 0 absorbs line 1 the key K is retired, leaving line 2 untouched.
    #[test]
    fn join_polylines_slot_zero_sentinel() {
        let lines = pl(&[
            &[(0.0, 0.0), (-1.0, 0.0)],
            &[(0.0, 0.0), (0.0, 1.0)],
            &[(0.0, 0.0), (1.0, 1.0)],
        ]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(
            xy(&joined[0]),
            vec![(0.0, 1.0), (0.0, 0.0), (0.0, 0.0), (-1.0, 0.0)]
        );
        assert!(joined[1].is_empty());
        assert_eq!(xy(&joined[2]), vec![(0.0, 0.0), (1.0, 1.0)]);
    }

    #[test]
    fn test_classification_size_is_single_byte() {
        assert_eq!(
            std::mem::size_of::<super::Classification>(),
            1,
            "Classification should be a single byte"
        );
    }
}
