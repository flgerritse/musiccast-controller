//! App icon, drawn in code (own artwork, no brand assets): a teal-to-blue rounded
//! tile with a white speaker and three sound waves. Used for the window icon,
//! the small badge in the header, and the `--make-icon <file.png>` option that
//! writes a 1024 px PNG for packaging.

/// Signed distance to a rounded box centred at `c` with half-size `h` and corner radius `r`.
fn sd_round_box(px: f32, py: f32, c: (f32, f32), h: (f32, f32), r: f32) -> f32 {
    let qx = (px - c.0).abs() - h.0 + r;
    let qy = (py - c.1).abs() - h.1 + r;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Coverage (0..1) of a shape whose signed distance is `d`, one pixel wide edge.
fn cov(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

fn over(dst: &mut [f32; 4], src: [f32; 4]) {
    let a = src[3];
    for i in 0..3 {
        dst[i] = src[i] * a + dst[i] * (1.0 - a);
    }
    dst[3] = a + dst[3] * (1.0 - a);
}

/// Render the icon as straight RGBA bytes, `size` x `size`.
pub fn rgba(size: u32) -> Vec<u8> {
    let n = size as usize;
    let mut out = vec![0u8; n * n * 4];
    let aa = 1.2 / size as f32;
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0.0f32; 4];
            // 3x3 supersampling for smooth edges.
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 3.0) / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / 3.0) / size as f32;
                    let mut c = [0.0f32; 4];
                    // Background tile: vertical gradient teal -> deep blue.
                    let tile_d = sd_round_box(px, py, (0.5, 0.5), (0.47, 0.47), 0.22);
                    let tile = cov(tile_d, aa);
                    if tile > 0.0 {
                        let t = py;
                        let top = [0.13, 0.72, 0.66];
                        let bot = [0.11, 0.33, 0.85];
                        let col = [
                            top[0] + (bot[0] - top[0]) * t,
                            top[1] + (bot[1] - top[1]) * t,
                            top[2] + (bot[2] - top[2]) * t,
                        ];
                        c = [col[0], col[1], col[2], tile];
                    }
                    let mut layer = c;
                    // Speaker cabinet (white, rounded).
                    let body = cov(sd_round_box(px, py, (0.40, 0.52), (0.14, 0.28), 0.05), aa);
                    if body > 0.0 {
                        over(&mut layer, [0.97, 0.98, 1.0, body]);
                    }
                    // Woofer: dark cone with a white rim.
                    let d_woof = ((px - 0.40).powi(2) + (py - 0.62).powi(2)).sqrt();
                    let rim = cov(d_woof - 0.105, aa);
                    if rim > 0.0 {
                        over(&mut layer, [0.85, 0.89, 0.95, rim]);
                    }
                    let cone = cov(d_woof - 0.085, aa);
                    if cone > 0.0 {
                        over(&mut layer, [0.08, 0.12, 0.2, cone]);
                    }
                    let dust = cov(d_woof - 0.03, aa);
                    if dust > 0.0 {
                        over(&mut layer, [0.85, 0.89, 0.95, dust * 0.9]);
                    }
                    // Tweeter.
                    let d_tw = ((px - 0.40).powi(2) + (py - 0.36).powi(2)).sqrt();
                    let tw = cov(d_tw - 0.05, aa);
                    if tw > 0.0 {
                        over(&mut layer, [0.08, 0.12, 0.2, tw]);
                    }
                    // Three sound waves (arcs to the right of the speaker).
                    // Waves are clipped to the tile so none spill past its edge.
                    let dx = px - 0.60;
                    let dy = py - 0.5;
                    let ang = dy.atan2(dx).abs();
                    if tile > 0.0 && dx > -0.02 && ang < 0.7 {
                        let r = (dx * dx + dy * dy).sqrt();
                        for (rad, alpha) in [(0.13, 1.0), (0.21, 0.85), (0.29, 0.7)] {
                            let w = cov((r - rad).abs() - 0.022, aa);
                            if w > 0.0 {
                                over(&mut layer, [1.0, 1.0, 1.0, w * alpha * tile]);
                            }
                        }
                    }
                    // Composite the layer over the transparent tile edge: keep premultiplied sum.
                    acc[0] += layer[0] * layer[3];
                    acc[1] += layer[1] * layer[3];
                    acc[2] += layer[2] * layer[3];
                    acc[3] += layer[3];
                }
            }
            let k = 9.0;
            let a = acc[3] / k;
            let idx = (y * n + x) * 4;
            if a > 0.0 {
                out[idx] = ((acc[0] / acc[3]) * 255.0).round().clamp(0.0, 255.0) as u8;
                out[idx + 1] = ((acc[1] / acc[3]) * 255.0).round().clamp(0.0, 255.0) as u8;
                out[idx + 2] = ((acc[2] / acc[3]) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            out[idx + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}
