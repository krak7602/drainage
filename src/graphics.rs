//! Optional high-fidelity charts via a terminal graphics protocol (Kitty /
//! iTerm2 / Sixel). We detect support at runtime; if a real graphics protocol
//! is present we render rasterized charts, otherwise the caller falls back to
//! the Unicode (Octant/heatmap) charts.
//!
//! Charts are drawn by hand onto an RGBA buffer (no `plotters`/font dependency,
//! so it cross-compiles cleanly), and ratatui draws the labels/legend around it.

use image::{DynamicImage, Rgba, RgbaImage};
use ratatui_image::picker::{Picker, ProtocolType};

/// One chart series for image rendering: (rgb color, points as (x, y)).
pub type RgbSeries = ((u8, u8, u8), Vec<(f64, f64)>);

/// RGB twins of the TUI's `PALETTE` (same order), for drawing into images.
pub const RGB_PALETTE: [(u8, u8, u8); 6] = [
    (80, 200, 220),  // cyan
    (210, 110, 210), // magenta
    (220, 200, 90),  // yellow
    (110, 200, 120), // green
    (110, 150, 240), // blue
    (230, 120, 110), // light red
];

pub struct Charts {
    picker: Option<Picker>,
}

impl Charts {
    /// Query the terminal. Enabled only if a real graphics protocol is present
    /// (not the Unicode halfblocks fallback, where our own charts look better).
    pub fn init() -> Self {
        let picker = Picker::from_query_stdio()
            .ok()
            .filter(|p| p.protocol_type() != ProtocolType::Halfblocks);
        Self { picker }
    }

    /// A disabled instance (no graphics) — for tests / headless use.
    #[allow(dead_code)]
    pub fn disabled() -> Self {
        Self { picker: None }
    }

    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }
}

const ML: u32 = 8;
const MR: u32 = 8;
const MT: u32 = 12;
const MB: u32 = 12;

fn blend(img: &mut RgbaImage, x: i64, y: i64, c: (u8, u8, u8), a: f64) {
    let (iw, ih) = img.dimensions();
    if x < 0 || y < 0 || x >= iw as i64 || y >= ih as i64 {
        return;
    }
    let p = img.get_pixel_mut(x as u32, y as u32);
    let mix = |d: u8, s: u8| (d as f64 * (1.0 - a) + s as f64 * a).round() as u8;
    *p = Rgba([mix(p[0], c.0), mix(p[1], c.1), mix(p[2], c.2), 255]);
}

fn disk(img: &mut RgbaImage, x: i64, y: i64, c: (u8, u8, u8), r: i64) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                blend(img, x + dx, y + dy, c, 1.0);
            }
        }
    }
}

/// Render per-model drift lines to an image of the given pixel size (matched to
/// the pane so it fills crisply). `series` is (rgb, points) where points are
/// (x=unix seconds, y=rate). ratatui draws the title/legend around it.
pub fn render_drift(series: &[RgbSeries], w: u32, h: u32) -> DynamicImage {
    let bg = (24, 24, 28);
    let mut img = RgbaImage::from_pixel(w, h, Rgba([bg.0, bg.1, bg.2, 255]));

    // Data bounds.
    let (mut xmin, mut xmax, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY, 1.0f64);
    for (_, pts) in series {
        for &(x, y) in pts {
            xmin = xmin.min(x);
            xmax = xmax.max(x);
            ymax = ymax.max(y);
        }
    }
    if !xmin.is_finite() || xmax <= xmin {
        return DynamicImage::ImageRgba8(img);
    }
    ymax *= 1.15;

    let px =
        |x: f64| -> i64 { (ML as f64 + (x - xmin) / (xmax - xmin) * (w - ML - MR) as f64).round() as i64 };
    let py =
        |y: f64| -> i64 { ((h - MB) as f64 - (y / ymax) * (h - MT - MB) as f64).round() as i64 };

    // Horizontal gridlines at 25% steps.
    for i in 0..=4 {
        let y = py(ymax * i as f64 / 4.0);
        for x in ML as i64..(w - MR) as i64 {
            blend(&mut img, x, y, (70, 70, 78), 0.5);
        }
    }

    let lw = (h / 200).clamp(1, 4) as i64; // line thickness scales with size
    for (rgb, pts) in series {
        if pts.len() < 2 {
            if let Some(&(x, y)) = pts.first() {
                disk(&mut img, px(x), py(y), *rgb, lw + 1);
            }
            continue;
        }
        // Area fill (translucent) under the line.
        let base = (h - MB) as i64;
        for seg in pts.windows(2) {
            let (a, b) = (px(seg[0].0), px(seg[1].0));
            let steps = (b - a).abs().max(1);
            for s in 0..=steps {
                let t = s as f64 / steps as f64;
                let x = a + (b - a) * s / steps;
                let yp = py(seg[0].1 + (seg[1].1 - seg[0].1) * t);
                for yy in yp..base {
                    blend(&mut img, x, yy, *rgb, 0.07);
                }
            }
        }
        // Thick polyline on top.
        for seg in pts.windows(2) {
            let (p0, p1) = ((px(seg[0].0), py(seg[0].1)), (px(seg[1].0), py(seg[1].1)));
            let steps = (p1.0 - p0.0).abs().max((p1.1 - p0.1).abs()).max(1);
            for s in 0..=steps {
                let x = p0.0 + (p1.0 - p0.0) * s / steps;
                let y = p0.1 + (p1.1 - p0.1) * s / steps;
                disk(&mut img, x, y, *rgb, lw);
            }
        }
        if let Some(&(x, y)) = pts.last() {
            disk(&mut img, px(x), py(y), *rgb, lw + 1);
        }
    }

    DynamicImage::ImageRgba8(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_drift_without_panic() {
        let series = vec![
            ((80, 200, 220), vec![(0.0, 10.0), (1.0, 15.0), (2.0, 12.0)]),
            ((210, 110, 210), vec![(0.0, 5.0), (2.0, 8.0)]),
            ((220, 200, 90), vec![(1.0, 3.0)]), // single point
        ];
        let img = render_drift(&series, 1400, 520);
        assert_eq!(img.width(), 1400);
        assert_eq!(img.height(), 520);
    }

    #[test]
    fn empty_series_ok() {
        assert_eq!(render_drift(&[], 800, 400).width(), 800);
    }
}
