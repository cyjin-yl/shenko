//! Deterministic text → grayscale PNG frames.
//!
//! Glyphs come from GNU Unifont (whole BMP: 8×16 halfwidth, 16×16 fullwidth),
//! so Chinese, Latin, digits, paths and code share one legible grid. Each
//! fullwidth glyph takes two cells. Characters outside the BMP (emoji, rare
//! ideographs) have no glyph: they render as boxes and are counted, so such a
//! record is reported unreadable rather than shipped as if legible.
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;
const MISSING: unifont::Glyph = unifont::Glyph::Halfwidth([
    0xFF, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xFF,
]);

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

pub struct Rendered {
    pub pngs: Vec<Frame>,
    pub unrenderable: u64,
}

pub fn has_glyph(c: char) -> bool {
    unifont::get_glyph(c).is_some()
}

/// Wrap `text` at `columns` halfwidth cells and paginate into frames of
/// `rows` text rows. A fullwidth glyph never straddles a line end.
pub fn render(text: &str, columns: u32, rows: u32) -> Rendered {
    let mut unrenderable = 0;
    let mut lines: Vec<Vec<&'static unifont::Glyph>> = Vec::new();
    for line in text.split('\n') {
        let mut current = Vec::new();
        let mut used = 0;
        for c in line.chars() {
            let glyph = unifont::get_glyph(c).unwrap_or_else(|| {
                unrenderable += 1;
                &MISSING
            });
            let cells = glyph.get_width() as u32 / CELL_W;
            if used + cells > columns {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(glyph);
            used += cells;
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }

    let mut pngs = Vec::new();
    for page in lines.chunks(rows as usize) {
        let width = columns * CELL_W;
        let height = page.len() as u32 * CELL_H;
        let mut pixels = vec![255u8; (width * height) as usize];
        for (row, line) in page.iter().enumerate() {
            let mut x0 = 0;
            for glyph in line {
                for y in 0..CELL_H as usize {
                    for x in 0..glyph.get_width() {
                        if glyph.get_pixel(x, y) {
                            let px = x0 + x as u32;
                            let py = row as u32 * CELL_H + y as u32;
                            pixels[(py * width + px) as usize] = 0;
                        }
                    }
                }
                x0 += glyph.get_width() as u32;
            }
        }
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("in-memory PNG header");
            writer.write_image_data(&pixels).expect("in-memory PNG data");
        }
        pngs.push(Frame { width, height, png });
    }
    Rendered { pngs, unrenderable }
}
