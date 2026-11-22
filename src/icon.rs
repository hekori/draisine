//! The Draisine icon: a rounded badge in the status colour with the logo in white on top.
//! The logo is an embedded 256x256 alpha mask (`assets/logo_mask.png`, raw bytes in
//! `logo_mask.bin`), scaled to the requested size at runtime. Used for the window and the app logo.
//! The tray uses a separate white pixel-art glyph ([`tray_rgba`]) with a small status badge in
//! the bottom-right corner, like Dropbox: the colour and the symbol both carry the state.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No folder pairs set up yet: plain glyph, no badge.
    Off,
    /// Auto-sync off: grey ⏸.
    Paused,
    /// Auto-sync on, last run clean: green ✓.
    Idle,
    /// Sync running: blue ↻.
    Busy,
    /// Remote unreachable (no network): grey –.
    Offline,
    /// Deletes / overwrites waiting for confirmation: orange !.
    Attention,
    /// OAuth token expired or revoked, reconnect needed: orange lock.
    AuthExpired,
    /// Any other failure: red ✕.
    Error,
}

impl Status {
    pub fn color(self) -> [u8; 3] {
        match self {
            Status::Off | Status::Paused | Status::Offline => [120, 126, 136],
            Status::Idle => [34, 154, 94],
            Status::Busy => [37, 110, 230],
            Status::Attention | Status::AuthExpired => [236, 136, 18],
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

// ---------------------------------------------------------------- tray glyph

/// The tray glyph, pixel art on a 22x22 grid: a simplified logo (document with sync arrows,
/// two pages stacked behind, wheels on an axle, rail). `#` opaque, `+` / `-` pages fading out,
/// `o` half-covered pixels that round off the wheels.
const GLYPH: [&str; 22] = [
    "......................",
    "...########...........",
    "...#########..........",
    "...##########..+......",
    "...#######.###.+......",
    "...##.......##.+.-....",
    "...#######.###.+.-....",
    "...###########.+.-....",
    "...###.#######.+.-....",
    "...##.......##.+.-....",
    "...###.#######.+.-....",
    "...###########.+......",
    "...###########.+......",
    "......................",
    "....o###o...o###o.....",
    "....#####...#####.....",
    "....##.#######.##.....",
    "....#####...#####.....",
    "....o###o...o###o.....",
    "......................",
    ".####################.",
    "......................",
];

/// Glyph coverage (0..1) at a point in grid units (0..22, y down).
fn glyph(x: f32, y: f32) -> f32 {
    let (cx, cy) = (x.floor() as usize, y.floor() as usize);
    match GLYPH.get(cy).and_then(|row| row.as_bytes().get(cx)) {
        Some(b'#') => 1.0,
        Some(b'+') => 0.7,
        Some(b'o') => 0.5,
        Some(b'-') => 0.4,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------- tray badge

/// Badge disc in glyph grid units (22x22): centre and radius, plus the transparent gap that
/// separates it from the glyph on light and dark panels alike.
const BADGE_C: (f32, f32) = (16.6, 16.6);
const BADGE_R: f32 = 5.2;
const BADGE_GAP: f32 = 1.1;

/// Distance from p to the segment a-b.
fn seg(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py, bx, by) = (p.0 - a.0, p.1 - a.1, b.0 - a.0, b.1 - a.1);
    let h = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
    ((px - bx * h).powi(2) + (py - by * h).powi(2)).sqrt()
}

/// Whether p lies inside the triangle a-b-c (either winding).
fn in_tri(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let e = |u: (f32, f32), v: (f32, f32)| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (d1, d2, d3) = (e(a, b), e(b, c), e(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

impl Status {
    /// Whether the badge symbol covers (u, v), in badge units: disc of radius 1, y down.
    fn symbol(self, u: f32, v: f32) -> bool {
        let p = (u, v);
        const W: f32 = 0.16; // half stroke width
        match self {
            Status::Off => false,
            Status::Idle => seg(p, (-0.48, 0.02), (-0.14, 0.36)) < W || seg(p, (-0.14, 0.36), (0.48, -0.30)) < W,
            Status::Busy => {
                // Circular arrow: an open ring, arrow head at the gap's upper end.
                let r = (u * u + v * v).sqrt();
                let a = v.atan2(u).to_degrees(); // 0° = right, 90° = down
                let ring = (r - 0.48).abs() < 0.14 && !(-75.0..-15.0).contains(&a);
                ring || in_tri(p, (0.08, -0.82), (0.08, -0.14), (0.50, -0.48))
            }
            Status::Offline => seg(p, (-0.46, 0.0), (0.46, 0.0)) < W + 0.02,
            Status::Attention => seg(p, (0.0, -0.52), (0.0, 0.10)) < W || (u * u + (v - 0.46).powi(2)).sqrt() < 0.17,
            Status::AuthExpired => {
                let body = u.abs() < 0.42 && (-0.06..0.52).contains(&v);
                let r = (u * u + (v + 0.12).powi(2)).sqrt();
                let shackle = (r - 0.27).abs() < 0.10 && v < -0.06;
                body || shackle
            }
            Status::Error => seg(p, (-0.36, -0.36), (0.36, 0.36)) < W || seg(p, (-0.36, 0.36), (0.36, -0.36)) < W,
            Status::Paused => (0.12..0.38).contains(&u.abs()) && v.abs() < 0.44,
        }
    }
}

/// Tray icon: the white logo glyph on a transparent background, so it sits next to other
/// monochrome tray icons, with the status badge in the bottom-right corner (none for
/// [`Status::Off`]). Pixel-exact at 22 px and its multiples (44, 66, …).
pub fn tray_rgba(size: u32, status: Status) -> Vec<u8> {
    const SS: u32 = 8; // samples per axis and pixel
    const WHITE: [f32; 3] = [255.0, 255.0, 255.0];
    let badge = status != Status::Off;
    let bc = status.color().map(|c| c as f32);
    let unit = 22.0 / size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // Accumulate premultiplied colour and alpha over the samples.
            let (mut acc, mut a) = ([0f32; 3], 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let gx = (x as f32 + (sx as f32 + 0.5) / SS as f32) * unit;
                    let gy = (y as f32 + (sy as f32 + 0.5) / SS as f32) * unit;
                    let (dx, dy) = (gx - BADGE_C.0, gy - BADGE_C.1);
                    let d = (dx * dx + dy * dy).sqrt();
                    let (c, sa) = if badge && d < BADGE_R {
                        let sym = status.symbol(dx / BADGE_R, dy / BADGE_R);
                        (if sym { WHITE } else { bc }, 1.0)
                    } else if badge && d < BADGE_R + BADGE_GAP {
                        (WHITE, 0.0)
                    } else {
                        (WHITE, glyph(gx, gy))
                    };
                    for k in 0..3 {
                        acc[k] += c[k] * sa;
                    }
                    a += sa;
                }
            }
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                for k in 0..3 {
                    px[i + k] = (acc[k] / a).round() as u8;
                }
            }
            px[i + 3] = (a / (SS * SS) as f32 * 255.0).round() as u8;
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

    #[test]
    fn tray_badge() {
        let at = |px: &[u8], x: u32, y: u32| px[((y * 44 + x) * 4) as usize..][..4].to_vec();
        // No badge when nothing is set up: the right wheel stays white.
        let off = tray_rgba(44, Status::Off);
        assert_eq!(at(&off, 29, 31), vec![255, 255, 255, 255]);
        // Error badge: red disc edge, white cross in the centre, transparent gap around it.
        let err = tray_rgba(44, Status::Error);
        assert_eq!(at(&err, 33, 24), vec![214, 50, 60, 255]);
        assert_eq!(&at(&err, 33, 33)[..], &[255, 255, 255, 255]);
        assert_eq!(at(&err, 25, 41)[3], 0); // rail cut by the gap
        // Glyph away from the badge stays white.
        assert_eq!(at(&err, 8, 4), vec![255, 255, 255, 255]);
    }

    /// Writes preview images: `DRAISINE_ICON_PREVIEW=/some/dir cargo test icon_preview -- --ignored`
    #[test]
    #[ignore]
    fn icon_preview() {
        let Ok(dir) = std::env::var("DRAISINE_ICON_PREVIEW") else {
            eprintln!("DRAISINE_ICON_PREVIEW not set, skipping");
            return;
        };
        use Status::*;
        for st in [Off, Paused, Idle, Busy, Offline, Attention, AuthExpired, Error] {
            for size in [16u32, 22, 32, 64, 256] {
                let path = format!("{dir}/{st:?}-{size}.rgba");
                std::fs::write(path, rgba(size, st.color())).unwrap();
            }
            for size in [22u32, 44, 64] {
                let path = format!("{dir}/tray-{st:?}-{size}.rgba");
                std::fs::write(path, tray_rgba(size, st)).unwrap();
            }
        }
    }
}
