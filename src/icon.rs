//! The Draisine icon: a rounded badge in the status colour with the logo in white on top.
//! The logo is an embedded 256x256 alpha mask (`assets/logo_mask.png`, raw bytes in
//! `logo_mask.bin`), scaled to the requested size at runtime. The colour shows the status in the tray.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Off,
    Idle,
    Busy,
    Attention,
    Error,
}

impl Status {
    pub fn color(self) -> [u8; 3] {
        match self {
            Status::Off => [120, 126, 136],
            Status::Idle => [34, 154, 94],
            Status::Busy => [37, 110, 230],
            Status::Attention => [236, 136, 18],
            Status::Error => [214, 50, 60],
        }
    }
}

/// Signed distance to a rounded square centred at (0.5, 0.5).
fn rounded_square(px: f32, py: f32, half: f32, radius: f32) -> f32 {
    let qx = (px - 0.5).abs() - (half - radius);
    let qy = (py - 0.5).abs() - (half - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// 256x256 alpha mask of the logo, row-major (see `assets/logo_mask.png`).
const MASK: &[u8; 256 * 256] = include_bytes!("../assets/logo_mask.bin");
const M: usize = 256;

/// Summed-area table of the mask, (M+1) x (M+1), for box-filtered sampling at any size.
fn mask_sat() -> Vec<f32> {
    let mut t = vec![0f32; (M + 1) * (M + 1)];
    for y in 0..M {
        let mut row = 0f32;
        for x in 0..M {
            row += MASK[y * M + x] as f32 / 255.0;
            t[(y + 1) * (M + 1) + x + 1] = t[y * (M + 1) + x + 1] + row;
        }
    }
    t
}

/// Bilinear lookup in the summed-area table at fractional mask coordinates.
fn sat_at(t: &[f32], x: f32, y: f32) -> f32 {
    let (x, y) = (x.clamp(0.0, M as f32), y.clamp(0.0, M as f32));
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(M), (y0 + 1).min(M));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let g = |xx: usize, yy: usize| t[yy * (M + 1) + xx];
    let top = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * fx;
    let bot = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * fx;
    top + (bot - top) * fy
}

/// Average logo coverage over the mask rectangle (x0, y0)-(x1, y1).
fn coverage(t: &[f32], x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let area = (x1 - x0) * (y1 - y0);
    if area <= 0.0 {
        return 0.0;
    }
    let sum = sat_at(t, x1, y1) - sat_at(t, x0, y1) - sat_at(t, x1, y0) + sat_at(t, x0, y0);
    (sum / area).clamp(0.0, 1.0)
}

fn shade(c: [u8; 3], t: f32) -> [f32; 3] {
    // t > 0 lightens towards white, t < 0 darkens towards black.
    c.map(|v| {
        let v = v as f32;
        if t >= 0.0 { v + (255.0 - v) * t } else { v * (1.0 + t) }
    })
}

/// RGBA pixels (not premultiplied), `size` x `size`.
pub fn rgba(size: u32, color: [u8; 3]) -> Vec<u8> {
    let s = size as f32;
    let aa = 1.0 / s; // one pixel, in unit coordinates
    let top = shade(color, 0.16);
    let bottom = shade(color, -0.14);
    // Small icons use almost the full square, large ones keep a margin like app icons do.
    let half = if size <= 32 { 0.49 } else { 0.46 };
    // The logo fills most of the badge; small sizes get a bit more so details stay visible.
    let logo = 2.0 * half * if size <= 32 { 0.86 } else { 0.80 };
    let l0 = 0.5 - logo / 2.0;
    let k = M as f32 / (logo * s); // mask pixels per icon pixel
    let sat = mask_sat();
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let bg_a = (0.5 - rounded_square(fx, fy, half, half * 0.44) / aa).clamp(0.0, 1.0);
            if bg_a <= 0.0 {
                continue;
            }
            let (mx, my) = ((x as f32 - l0 * s) * k, (y as f32 - l0 * s) * k);
            let g_a = coverage(&sat, mx, my, mx + k, my + k);
            let t = fy.clamp(0.0, 1.0);
            let i = ((y * size + x) * 4) as usize;
            for k in 0..3 {
                let bg = top[k] + (bottom[k] - top[k]) * t;
                px[i + k] = (bg + (255.0 - bg) * g_a).round() as u8;
            }
            px[i + 3] = (bg_a * 255.0).round() as u8;
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_transparent_centre_opaque() {
        let px = rgba(64, Status::Idle.color());
        assert_eq!(px[3], 0); // top-left corner
        let c = ((32 * 64 + 32) * 4) as usize;
        assert_eq!(px[c + 3], 255);
    }

    /// Writes preview images: `DRAISINE_ICON_PREVIEW=/some/dir cargo test icon_preview -- --ignored`
    #[test]
    #[ignore]
    fn icon_preview() {
        let Ok(dir) = std::env::var("DRAISINE_ICON_PREVIEW") else {
            eprintln!("DRAISINE_ICON_PREVIEW not set, skipping");
            return;
        };
        for st in [Status::Off, Status::Idle, Status::Busy, Status::Attention, Status::Error] {
            for size in [16u32, 22, 32, 64, 256] {
                let path = format!("{dir}/{st:?}-{size}.rgba");
                std::fs::write(path, rgba(size, st.color())).unwrap();
            }
        }
    }
}
