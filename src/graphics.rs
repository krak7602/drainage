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

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }
}

const W: u32 = 1400;
const H: u32 = 520;
const ML: u32 = 8;
const MR: u32 = 8;
const MT: u32 = 10;
const MB: u32 = 10;

fn blend(img: &mut RgbaImage, x: i64, y: i64, c: (u8, u8, u8), a: f64) {
    if x < 0 || y < 0 || x >= W as i64 || y >= H as i64 {
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

/// Render per-model drift lines to an image. `series` is (rgb, points) where
/// points are (x=unix seconds, y=rate). ratatui draws the title/legend around.
pub fn render_drift(series: &[RgbSeries]) -> DynamicImage {
    let bg = (24, 24, 28);
    let mut img = RgbaImage::from_pixel(W, H, Rgba([bg.0, bg.1, bg.2, 255]));

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

    let px = |x: f64| -> i64 {
        (ML as f64 + (x - xmin) / (xmax - xmin) * (W - ML - MR) as f64).round() as i64
    };
    let py = |y: f64| -> i64 {
        ((H - MB) as f64 - (y / ymax) * (H - MT - MB) as f64).round() as i64
    };

    // Horizontal gridlines at 25% steps.
    for i in 0..=4 {
        let y = py(ymax * i as f64 / 4.0);
        for x in ML as i64..(W - MR) as i64 {
            blend(&mut img, x, y, (70, 70, 78), 0.5);
        }
    }

    for (rgb, pts) in series {
        if pts.len() < 2 {
            // A single point: draw a dot so it's still visible.
            if let Some(&(x, y)) = pts.first() {
                disk(&mut img, px(x), py(y), *rgb, 3);
            }
            continue;
        }
        // Area fill (translucent) then the line on top.
        for w in pts.windows(2) {
            let (x0, y0) = w[0];
            let (x1, y1) = w[1];
            let (a, b) = (px(x0), px(x1));
            let steps = (b - a).abs().max(1);
            for s in 0..=steps {
                let t = s as f64 / steps as f64;
                let x = a + (b - a) * s / steps;
                let yv = y0 + (y1 - y0) * t;
                let yp = py(yv);
                for yy in yp..(H - MB) as i64 {
                    blend(&mut img, x, yy, *rgb, 0.06);
                }
            }
        }
        // Thick polyline.
        for w in pts.windows(2) {
            let (p0, p1) = ((px(w[0].0), py(w[0].1)), (px(w[1].0), py(w[1].1)));
            let steps = ((p1.0 - p0.0).abs()).max((p1.1 - p0.1).abs()).max(1);
            for s in 0..=steps {
                let x = p0.0 + (p1.0 - p0.0) * s / steps;
                let y = p0.1 + (p1.1 - p0.1) * s / steps;
                disk(&mut img, x, y, *rgb, 2);
            }
        }
        // Emphasize the latest point.
        if let Some(&(x, y)) = pts.last() {
            disk(&mut img, px(x), py(y), *rgb, 3);
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
        let img = render_drift(&series);
        assert_eq!(img.width(), W);
        assert_eq!(img.height(), H);
    }

    #[test]
    fn empty_series_ok() {
        assert_eq!(render_drift(&[]).width(), W);
    }
}
