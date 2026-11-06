//! Ikona słuchawek rysowana w kodzie (bez plików graficznych), z antyaliasingiem.

pub const SIZE: u32 = 32;

pub enum IconState {
    Idle,
    Active,
}

/// Zwraca bufor RGBA SIZE x SIZE.
pub fn rgba(state: IconState) -> Vec<u8> {
    let color: [u8; 3] = match state {
        IconState::Idle => [0x9a, 0xa0, 0xa6],
        IconState::Active => [0x2e, 0xcc, 0x71],
    };
    const SS: u32 = 4; // supersampling 4x4
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if inside(px, py) {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[color[0], color[1], color[2], alpha]);
        }
    }
    out
}

/// Kształt słuchawek: pałąk (łuk) + dwie muszle (zaokrąglone prostokąty).
fn inside(x: f32, y: f32) -> bool {
    let (cx, cy) = (16.0, 18.0);
    let r = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    let band = y <= cy && (11.0..=14.0).contains(&r);
    let band_sides = y > cy && y <= 21.0 && (11.0..=14.0).contains(&(x - cx).abs());
    band || band_sides || rounded_rect(x, y, 2.0, 17.0, 10.0, 30.0, 2.5) || rounded_rect(x, y, 22.0, 17.0, 30.0, 30.0, 2.5)
}

fn rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}
