//! Deterministic text → grayscale PNG frames.
//!
//! The current glyph set is font8x8 (ASCII + Latin-1). CJK has no glyph yet:
//! such characters render as boxes and are counted, so a Chinese archive is
//! reported unreadable and falls back rather than pretending to be legible.
//! A CJK-capable renderer is a dependency of this plugin, not of the core.
use font8x8::legacy::{BASIC_LEGACY, LATIN_LEGACY};

const CELL: u32 = 8;

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
    glyph(c).is_some()
}

fn glyph(c: char) -> Option<[u8; 8]> {
    match c as u32 {
        code @ 0x20..=0x7E => Some(BASIC_LEGACY[code as usize]),
        code @ 0xA0..=0xFF => Some(LATIN_LEGACY[(code - 0xA0) as usize]),
        _ => None,
    }
}

/// Wrap `text` at `columns` and paginate into frames of `rows` text rows.
pub fn render(text: &str, columns: u32, rows: u32) -> Rendered {
    let mut lines: Vec<Vec<char>> = Vec::new();
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            continue;
        }
        lines.extend(chars.chunks(columns as usize).map(<[char]>::to_vec));
    }
    let mut unrenderable = 0;
    let mut pngs = Vec::new();
    for page in lines.chunks(rows as usize) {
        let width = columns * CELL;
        let height = page.len() as u32 * CELL;
        let mut pixels = vec![255u8; (width * height) as usize];
        for (row, line) in page.iter().enumerate() {
            for (col, &c) in line.iter().enumerate() {
                let bitmap = glyph(c).unwrap_or_else(|| {
                    unrenderable += 1;
                    [0xFF, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xFF]
                });
                for (y, bits) in bitmap.iter().enumerate() {
                    for x in 0..8 {
                        if bits >> x & 1 == 1 {
                            let px = col as u32 * CELL + x;
                            let py = row as u32 * CELL + y as u32;
                            pixels[(py * width + px) as usize] = 0;
                        }
                    }
                }
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
