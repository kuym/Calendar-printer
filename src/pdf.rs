//! Tiny drawing canvas over pdf-writer. Coordinates are in points with the
//! origin at the top-left of the page (y grows downward).
//!
//! Fonts: Roboto (embedded TrueType, bundled in the binary) for the calendar
//! itself, and the PDF base font Helvetica for the weekly detail list.
//! All text uses WinAnsiEncoding, so widths are exact per byte.

use std::sync::OnceLock;

use pdf_writer::types::FontFlags;
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str, TextStr};

use crate::metrics;

pub const LETTER_W: f32 = 612.0;
pub const LETTER_H: f32 = 792.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gray(pub f32);

pub const BLACK: Gray = Gray(0.0);
pub const WHITE: Gray = Gray(1.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Light,
    Bold,
    BoldItalic,
    Sans,
    SansBold,
}

const ALL_FONTS: [Font; 5] = [Font::Light, Font::Bold, Font::BoldItalic, Font::Sans, Font::SansBold];

struct Metrics {
    /// Advance widths in 1/1000 em, indexed by WinAnsi byte.
    widths: [f32; 256],
    ascent: f32,
    descent: f32,
    cap_height: f32,
    bbox: [f32; 4],
    italic_angle: f32,
}

impl Font {
    fn resource(self) -> Name<'static> {
        Name(match self {
            Font::Light => b"F1",
            Font::Bold => b"F2",
            Font::BoldItalic => b"F3",
            Font::Sans => b"F4",
            Font::SansBold => b"F5",
        })
    }

    fn base_name(self) -> &'static str {
        match self {
            Font::Light => "Roboto-Light",
            Font::Bold => "Roboto-Bold",
            Font::BoldItalic => "Roboto-BoldItalic",
            Font::Sans => "Helvetica",
            Font::SansBold => "Helvetica-Bold",
        }
    }

    /// Embedded TrueType data; None for base-14 fonts.
    fn data(self) -> Option<&'static [u8]> {
        match self {
            Font::Light => Some(include_bytes!("../fonts/Roboto-Light.ttf")),
            Font::Bold => Some(include_bytes!("../fonts/Roboto-Bold.ttf")),
            Font::BoldItalic => Some(include_bytes!("../fonts/Roboto-BoldItalic.ttf")),
            Font::Sans | Font::SansBold => None,
        }
    }

    fn metrics(self) -> &'static Metrics {
        static CACHE: [OnceLock<Metrics>; 5] = [const { OnceLock::new() }; 5];
        let i = ALL_FONTS.iter().position(|f| *f == self).unwrap();
        CACHE[i].get_or_init(|| match self.data() {
            Some(data) => truetype_metrics(data),
            None => {
                let table = if self == Font::Sans { &metrics::HELVETICA } else { &metrics::HELVETICA_BOLD };
                let mut widths = [0.0; 256];
                for (w, t) in widths.iter_mut().zip(table) {
                    *w = *t as f32;
                }
                // Arial's line metrics, which Helvetica stands in for.
                Metrics { widths, ascent: 905.0, descent: -212.0, cap_height: 718.0, bbox: [-166.0, -225.0, 1000.0, 931.0], italic_angle: 0.0 }
            }
        })
    }

    /// Distance from the top of the text line box to the baseline, as a fraction of size.
    pub fn ascent(self) -> f32 {
        self.metrics().ascent / 1000.0
    }
}

fn truetype_metrics(data: &[u8]) -> Metrics {
    let face = ttf_parser::Face::parse(data, 0).expect("bundled font parses");
    let scale = 1000.0 / face.units_per_em() as f32;
    let mut widths = [0.0; 256];
    for (b, w) in widths.iter_mut().enumerate().skip(32) {
        let c = decode_byte(b as u8);
        let gid = face.glyph_index(c).or_else(|| face.glyph_index('?'));
        *w = gid.and_then(|g| face.glyph_hor_advance(g)).unwrap_or(0) as f32 * scale;
    }
    let bb = face.global_bounding_box();
    Metrics {
        widths,
        ascent: face.ascender() as f32 * scale,
        descent: face.descender() as f32 * scale,
        cap_height: face.capital_height().unwrap_or(face.ascender()) as f32 * scale,
        bbox: [bb.x_min as f32 * scale, bb.y_min as f32 * scale, bb.x_max as f32 * scale, bb.y_max as f32 * scale],
        italic_angle: face.italic_angle(),
    }
}

const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–',
    '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

fn decode_byte(b: u8) -> char {
    if (0x80..=0x9F).contains(&b) { CP1252_HIGH[(b - 0x80) as usize] } else { b as char }
}

/// Map a char to its WinAnsiEncoding byte, if it has one.
fn win_ansi(c: char) -> Option<u8> {
    let cp = c as u32;
    if (0x20..0x7F).contains(&cp) || (0xA0..=0xFF).contains(&cp) {
        return Some(cp as u8);
    }
    if matches!(c, '\t' | '\n' | '\r') {
        return Some(b' ');
    }
    CP1252_HIGH.iter().position(|&h| h == c && (h as u32) > 0xFF).map(|i| 0x80 + i as u8)
}

/// Emoji, pictographs, and invisible joiners: dropped rather than shown as '?'.
fn is_droppable(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x2060..=0x206F | 0x2190..=0x2BFF | 0xFE00..=0xFE0F | 0x1F000..=0x1FAFF | 0xE0000..=0xE007F)
}

/// Encode text for WinAnsi fonts. Emoji and symbols are dropped; other
/// unsupported letters become '?'.
pub fn encode(text: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut dropped = false;
    for c in text.chars() {
        match win_ansi(c) {
            Some(b) => out.push(b),
            None if is_droppable(c) => dropped = true,
            None => out.push(b'?'),
        }
    }
    if dropped {
        // Tidy the spaces a dropped emoji leaves behind.
        out.dedup_by(|a, b| *a == b' ' && *b == b' ');
        while out.first() == Some(&b' ') {
            out.remove(0);
        }
        while out.last() == Some(&b' ') {
            out.pop();
        }
    }
    out
}

pub fn bytes_width(bytes: &[u8], font: Font, size: f32) -> f32 {
    let w = &font.metrics().widths;
    bytes.iter().map(|&b| w[b as usize]).sum::<f32>() * size / 1000.0
}

pub fn text_width(text: &str, font: Font, size: f32) -> f32 {
    bytes_width(&encode(text), font, size)
}

/// Word-wrap to `max_w`. Words longer than a line are broken.
pub fn wrap(text: &str, font: Font, size: f32, max_w: f32) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut words: std::collections::VecDeque<Vec<u8>> =
        encode(text).split(|&b| b == b' ').filter(|w| !w.is_empty()).map(<[u8]>::to_vec).collect();
    while let Some(word) = words.pop_front() {
        let candidate = if current.is_empty() { word.clone() } else { [current.as_slice(), b" ", &word].concat() };
        if bytes_width(&candidate, font, size) <= max_w {
            current = candidate;
        } else if current.is_empty() {
            let mut n = 1;
            while n < word.len() && bytes_width(&word[..n + 1], font, size) <= max_w {
                n += 1;
            }
            // Prefer breaking after a hyphen or slash ("10:05-" / "10:30am").
            if let Some(i) = word[..n].iter().rposition(|&b| b == b'-' || b == b'/').filter(|&i| i > 0 && n < word.len()) {
                n = i + 1;
            }
            lines.push(word[..n].to_vec());
            if n < word.len() {
                words.push_front(word[n..].to_vec());
            }
        } else {
            lines.push(std::mem::take(&mut current));
            words.push_front(word);
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

pub struct Page {
    pub w: f32,
    pub h: f32,
    c: Content,
}

impl Page {
    pub fn letter() -> Page {
        Page { w: LETTER_W, h: LETTER_H, c: Content::new() }
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, g: Gray) {
        self.c.set_fill_gray(g.0);
        self.c.rect(x, self.h - y - h, w, h).fill_nonzero();
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, g: Gray) {
        self.c.set_stroke_gray(g.0).set_line_width(width);
        self.c.move_to(x1, self.h - y1).line_to(x2, self.h - y2).stroke();
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, g: Gray) {
        let (x, y) = (cx, self.h - cy);
        let k = 0.5523 * r;
        self.c.set_fill_gray(g.0);
        self.c
            .move_to(x + r, y)
            .cubic_to(x + r, y + k, x + k, y + r, x, y + r)
            .cubic_to(x - k, y + r, x - r, y + k, x - r, y)
            .cubic_to(x - r, y - k, x - k, y - r, x, y - r)
            .cubic_to(x + k, y - r, x + r, y - k, x + r, y)
            .close_path()
            .fill_nonzero();
    }

    /// Draw pre-encoded text with its baseline at `y`.
    pub fn bytes(&mut self, x: f32, y: f32, font: Font, size: f32, g: Gray, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.c.set_fill_gray(g.0);
        self.c.begin_text().set_font(font.resource(), size).next_line(x, self.h - y).show(Str(bytes)).end_text();
    }

    pub fn text(&mut self, x: f32, y: f32, font: Font, size: f32, g: Gray, text: &str) {
        self.bytes(x, y, font, size, g, &encode(text));
    }

    pub fn text_right(&mut self, right: f32, y: f32, font: Font, size: f32, g: Gray, text: &str) {
        let b = encode(text);
        let w = bytes_width(&b, font, size);
        self.bytes(right - w, y, font, size, g, &b);
    }

    pub fn text_center(&mut self, cx: f32, y: f32, font: Font, size: f32, g: Gray, text: &str) {
        let b = encode(text);
        let w = bytes_width(&b, font, size);
        self.bytes(cx - w / 2.0, y, font, size, g, &b);
    }

    /// Draw a QR code for `data` filling a `size`-point square at (x, y).
    pub fn qr(&mut self, x: f32, y: f32, size: f32, data: &str) {
        let Ok(code) = qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::M) else { return };
        let n = code.width();
        let colors = code.to_colors();
        let m = size / n as f32;
        self.c.set_fill_gray(0.0);
        for row in 0..n {
            for col in 0..n {
                if colors[row * n + col] == qrcode::Color::Dark {
                    // Slight overlap avoids hairline gaps between modules.
                    self.c.rect(x + col as f32 * m, self.h - y - (row + 1) as f32 * m, m + 0.05, m + 0.05);
                }
            }
        }
        self.c.fill_nonzero();
    }
}

/// Assemble pages into a PDF file.
pub fn write_pdf(title: &str, pages: Vec<Page>) -> Vec<u8> {
    let mut pdf = Pdf::new();
    let mut next = 1;
    let mut alloc = || {
        let r = Ref::new(next);
        next += 1;
        r
    };
    let catalog = alloc();
    let tree = alloc();
    let info = alloc();
    let font_ids: Vec<Ref> = ALL_FONTS.iter().map(|_| alloc()).collect();
    let page_ids: Vec<(Ref, Ref)> = pages.iter().map(|_| (alloc(), alloc())).collect();

    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree).kids(page_ids.iter().map(|(p, _)| *p)).count(page_ids.len() as i32);
    for (page, (page_id, content_id)) in pages.into_iter().zip(&page_ids) {
        let mut p = pdf.page(*page_id);
        p.media_box(Rect::new(0.0, 0.0, page.w, page.h));
        p.parent(tree);
        p.contents(*content_id);
        let mut resources = p.resources();
        let mut fonts = resources.fonts();
        for (font, id) in ALL_FONTS.iter().zip(&font_ids) {
            fonts.pair(font.resource(), *id);
        }
        fonts.finish();
        resources.finish();
        p.finish();
        pdf.stream(*content_id, &page.c.finish());
    }

    for (font, id) in ALL_FONTS.iter().zip(&font_ids) {
        let m = font.metrics();
        let Some(data) = font.data() else {
            pdf.type1_font(*id).base_font(Name(font.base_name().as_bytes())).encoding_predefined(Name(b"WinAnsiEncoding"));
            continue;
        };
        let (descriptor, file) = (alloc(), alloc());
        let name = Name(font.base_name().as_bytes());
        {
            let mut d = pdf.indirect(*id).dict();
            d.pair(Name(b"Type"), Name(b"Font"));
            d.pair(Name(b"Subtype"), Name(b"TrueType"));
            d.pair(Name(b"BaseFont"), name);
            d.pair(Name(b"Encoding"), Name(b"WinAnsiEncoding"));
            d.pair(Name(b"FirstChar"), 32);
            d.pair(Name(b"LastChar"), 255);
            d.insert(Name(b"Widths")).array().items(m.widths[32..].iter().copied());
            d.pair(Name(b"FontDescriptor"), descriptor);
        }
        let mut flags = FontFlags::NON_SYMBOLIC;
        if m.italic_angle != 0.0 {
            flags |= FontFlags::ITALIC;
        }
        pdf.font_descriptor(descriptor)
            .name(name)
            .flags(flags)
            .bbox(Rect::new(m.bbox[0], m.bbox[1], m.bbox[2], m.bbox[3]))
            .italic_angle(m.italic_angle)
            .ascent(m.ascent)
            .descent(m.descent)
            .cap_height(m.cap_height)
            .stem_v(if *font == Font::Light { 50.0 } else { 120.0 })
            .font_file2(file);
        pdf.stream(file, data).pair(Name(b"Length1"), data.len() as i32);
    }

    pdf.document_info(info).title(TextStr(title)).creator(TextStr("calprint"));
    pdf.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_win_ansi() {
        assert_eq!(encode("Café “Night” – 🎉 party"), b"Caf\xe9 \x93Night\x94 \x96 party".to_vec());
        assert_eq!(encode("日本"), b"??".to_vec());
        assert_eq!(encode("Zürich"), b"Z\xfcrich".to_vec());
        assert_eq!(encode("October "), b"October ".to_vec());
    }

    #[test]
    fn measures() {
        // Helvetica 'H' is 722/1000 em.
        assert!((text_width("H", Font::Sans, 10.0) - 7.22).abs() < 1e-4);
        // Roboto metrics come from the embedded font.
        assert!(text_width("Monday", Font::Bold, 12.0) > 30.0);
        assert!(Font::Light.ascent() > 0.8 && Font::Light.ascent() < 1.0);
    }

    #[test]
    fn wraps() {
        let lines = wrap("B: Pick up C @ School 4:30pm", Font::Light, 10.0, 68.0);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| bytes_width(l, Font::Light, 10.0) <= 68.0));

        let lines = wrap("Denise PTC 10:05-10:30am", Font::Bold, 10.0, 68.0);
        assert_eq!(lines[1], b"10:05-".to_vec());
        assert_eq!(lines[2], b"10:30am".to_vec());

        let lines = wrap("Supercalifragilisticexpialidocious", Font::Bold, 10.0, 40.0);
        assert!(lines.len() >= 2 && lines.iter().all(|l| bytes_width(l, Font::Bold, 10.0) <= 40.0));
    }
}
