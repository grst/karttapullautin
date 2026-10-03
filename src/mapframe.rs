//! The map frame: 600 dpi at 1:10 000. One map inch is 10 000 ground inches, 254 m, so the
//! sheet has 600 / 254 pixels per ground metre before `scalefactor`.
//!
//! Call sites keep their own operator order (`d * DPI / GROUND_METRES_PER_INCH / scalefactor`
//! or the inverse) because `(d * 600.0) / 254.0` and `d * (600.0 / 254.0)` round differently;
//! `PX_PER_METRE` is only for sites that already computed the fused constant first.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::io::fs::FileSystem;

/// Dots per map inch of the rendered sheet.
pub const DPI: f64 = 600.0;
/// Ground metres covered by one map inch at 1:10 000.
pub const GROUND_METRES_PER_INCH: f64 = 254.0;
/// Pixels per ground metre before `scalefactor`, as one fused constant.
pub const PX_PER_METRE: f64 = DPI / GROUND_METRES_PER_INCH;

/// A six-line world file: pixel size x, rotation, rotation, pixel size y, x origin, y origin.
///
/// The world file places the map frame in ground coordinates; the origin is the centre of the
/// upper-left pixel.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldFile {
    pub pixel_size_x: f64,
    pub rotation_y: f64,
    pub rotation_x: f64,
    pub pixel_size_y: f64,
    pub x_origin: f64,
    pub y_origin: f64,
}

impl WorldFile {
    /// Parse six lines; each line is trimmed (so `\r\n` files work). Errors on fewer than six
    /// lines or a value that is not an f64. Lines after the sixth are ignored.
    pub fn parse<R: BufRead>(reader: R) -> anyhow::Result<Self> {
        let mut values = [0.0f64; 6];
        let mut lines = reader.lines();
        for (i, value) in values.iter_mut().enumerate() {
            let Some(line) = lines.next() else {
                anyhow::bail!("world file has only {i} lines, expected 6");
            };
            let line = line?;
            let text = line.trim();
            *value = text.parse::<f64>().map_err(|e| {
                anyhow::anyhow!("world file line {}: {text:?} is not a number: {e}", i + 1)
            })?;
        }
        Ok(Self {
            pixel_size_x: values[0],
            rotation_y: values[1],
            rotation_x: values[2],
            pixel_size_y: values[3],
            x_origin: values[4],
            y_origin: values[5],
        })
    }

    /// Open `path` on `fs` and parse it as a world file.
    pub fn read(fs: &impl FileSystem, path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let reader = fs
            .open(path)
            .map_err(|e| anyhow::anyhow!("cannot open world file {}: {e}", path.display()))?;
        Self::parse(reader)
    }

    /// Write the six values with `{}` formatting and `\r\n` line endings, in field order.
    pub fn write<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        write!(
            writer,
            "{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n",
            self.pixel_size_x,
            self.rotation_y,
            self.rotation_x,
            self.pixel_size_y,
            self.x_origin,
            self.y_origin
        )
    }

    /// The literal unit-resolution header that `vegetation.pgw` and `_vege.pgw` carry: one
    /// pixel per ground metre, north up, followed by the origin.
    pub fn write_unit_resolution<W: Write>(
        writer: &mut W,
        x_origin: f64,
        y_origin: f64,
    ) -> std::io::Result<()> {
        write!(
            writer,
            "1.0\r\n0.0\r\n0.0\r\n-1.0\r\n{}\r\n{}\r\n",
            x_origin, y_origin
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_per_metre_is_the_literal() {
        assert_eq!(PX_PER_METRE.to_bits(), (600.0f64 / 254.0).to_bits());
    }

    /// Documents why two constants exist: swapping the fused constant into a call site that
    /// multiplies first changes the rounding of some values.
    #[test]
    fn operator_order_is_preserved() {
        for (d, s) in [(3.0, 1.0), (1234.567, 1.0), (98765.4321, 2.0), (0.1, 0.5)] {
            assert_eq!(
                (d * DPI / GROUND_METRES_PER_INCH / s).to_bits(),
                (d * 600.0 / 254.0 / s).to_bits()
            );
            assert_eq!(
                (d / DPI * GROUND_METRES_PER_INCH * s).to_bits(),
                (d / 600.0 * 254.0 * s).to_bits()
            );
        }
        let any_differs = (1..1000).any(|i| {
            let d = i as f64 * 1.001;
            (d * DPI / GROUND_METRES_PER_INCH).to_bits() != (d * PX_PER_METRE).to_bits()
        });
        assert!(any_differs, "fused and unfused constants never differed");
    }

    #[test]
    fn parse_reads_crlf_and_write_reproduces_it() {
        let text = "2.5\r\n0\r\n0\r\n-2.5\r\n123456.25\r\n7891011.5\r\n";
        let w = WorldFile::parse(text.as_bytes()).unwrap();
        assert_eq!(
            w,
            WorldFile {
                pixel_size_x: 2.5,
                rotation_y: 0.0,
                rotation_x: 0.0,
                pixel_size_y: -2.5,
                x_origin: 123456.25,
                y_origin: 7891011.5,
            }
        );
        let mut out = Vec::new();
        w.write(&mut out).unwrap();
        assert_eq!(out, text.as_bytes());
        assert_eq!(WorldFile::parse(out.as_slice()).unwrap(), w);
    }

    #[test]
    fn parse_rejects_five_lines() {
        assert!(WorldFile::parse("1\r\n0\r\n0\r\n-1\r\n5\r\n".as_bytes()).is_err());
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(WorldFile::parse("1\r\n0\r\nabc\r\n-1\r\n5\r\n6\r\n".as_bytes()).is_err());
    }

    #[test]
    fn write_unit_resolution_matches_the_legacy_text() {
        let mut out = Vec::new();
        WorldFile::write_unit_resolution(&mut out, 1.5, 2.0).unwrap();
        assert_eq!(out, b"1.0\r\n0.0\r\n0.0\r\n-1.0\r\n1.5\r\n2\r\n");
    }
}
