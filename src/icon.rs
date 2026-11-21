//! The draisine icon (two wheels, beam, handlebar), drawn at runtime so no image files are needed.
//! The colour shows the status in the tray.

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
            Status::Off => [128, 128, 128],
            Status::Idle => [46, 160, 67],
            Status::Busy => [30, 120, 220],
            Status::Attention => [235, 140, 0],
            Status::Error => [215, 45, 45],
        }
    }
}

fn seg_dist(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * dx + (py - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// RGBA pixels, `size` x `size`.
pub fn rgba(size: u32, color: [u8; 3]) -> Vec<u8> {
    let s = size as f32;
    let aa = 1.2 / s; // anti-aliasing width in unit coordinates
    let mut px = vec![0u8; (size * size * 4) as usize];
    let wheels = [(0.24f32, 0.70f32), (0.76f32, 0.70f32)];
    let (r_out, ring) = (0.22f32, 0.075f32);
    let segs = [
        ((0.12, 0.44), (0.80, 0.44)), // beam
        ((0.24, 0.70), (0.30, 0.44)), // rear stay
        ((0.76, 0.70), (0.72, 0.44)), // fork
        ((0.72, 0.44), (0.68, 0.18)), // stem
        ((0.58, 0.18), (0.80, 0.18)), // handlebar
        ((0.34, 0.36), (0.50, 0.36)), // seat
    ];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let mut d = f32::MAX; // signed-ish distance to the shape (<= 0 inside)
            for (cx, cy) in wheels {
                let r = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                d = d.min((r - (r_out - ring / 2.0)).abs() - ring / 2.0); // ring
                d = d.min(r - 0.05); // hub
            }
            for (a, b) in segs {
                d = d.min(seg_dist(fx, fy, a, b) - 0.042);
            }
            let alpha = (0.5 - d / aa).clamp(0.0, 1.0);
            let i = ((y * size + x) * 4) as usize;
            px[i] = color[0];
            px[i + 1] = color[1];
            px[i + 2] = color[2];
            px[i + 3] = (alpha * 255.0) as u8;
        }
    }
    px
}
