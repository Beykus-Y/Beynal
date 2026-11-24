// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Procedural anti-aliased SDF vector icons for Beyrix native UI.
//!
//! Renders crisp geometric vector shapes into cached RGBA bitmaps at exact
//! physical pixel sizes so icons look identical and razor-sharp in both the
//! OpenGL and Software backends at any HiDPI scale, without relying on
//! inconsistent Unicode or emoji fonts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

type IconCacheKey = (String, u32, [u8; 4]);
type IconCache = HashMap<IconCacheKey, (u64, Arc<Vec<u8>>)>;

static ICON_CACHE: OnceLock<Mutex<IconCache>> = OnceLock::new();

/// Rasterize (or fetch from cache) a crisp anti-aliased vector icon at `size_px`
/// with the given `[r, g, b, a]` color. Returns `(unique_image_id, rgba_bytes)`.
pub fn rasterize_icon(name: &str, size_px: u32, color: [f32; 4]) -> (u64, Arc<Vec<u8>>) {
    let sz = size_px.clamp(8, 96);
    let rgba8 = [
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ];
    let key = (name.to_string(), sz, rgba8);
    let cache = ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some((id, buf)) = guard.get(&key)
    {
        return (*id, Arc::clone(buf));
    }

    let mut data = vec![0u8; (sz * sz * 4) as usize];
    let inv = 1.0 / sz as f32;
    let aa_px = inv * 1.15;

    for py in 0..sz {
        let ny = (py as f32 + 0.5) * inv * 2.0 - 1.0; // -1..1
        for px in 0..sz {
            let nx = (px as f32 + 0.5) * inv * 2.0 - 1.0; // -1..1
            let dist = icon_sdf(name, nx, ny);
            let cov = (0.5 - dist / (aa_px * 2.0)).clamp(0.0, 1.0);
            if cov > 0.002 {
                let off = ((py * sz + px) * 4) as usize;
                data[off] = rgba8[0];
                data[off + 1] = rgba8[1];
                data[off + 2] = rgba8[2];
                data[off + 3] = (rgba8[3] as f32 * cov).round() as u8;
            }
        }
    }

    let arc = Arc::new(data);
    let id = hash_icon_id(name, sz, rgba8);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 512 {
            guard.clear();
        }
        guard.insert(key, (id, Arc::clone(&arc)));
    }
    (id, arc)
}

fn hash_icon_id(name: &str, sz: u32, rgba: [u8; 4]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= (sz as u64) << 32;
    h ^= u32::from_le_bytes(rgba) as u64;
    h | (1u64 << 63) // high bit marks procedural icon IDs
}

#[inline]
fn sd_box(px: f32, py: f32, bx: f32, by: f32) -> f32 {
    let dx = px.abs() - bx;
    let dy = py.abs() - by;
    (dx.max(0.0).hypot(dy.max(0.0))) + dx.max(dy).min(0.0)
}

#[inline]
fn sd_rounded_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    sd_box(px, py, bx - r, by - r) - r
}

#[inline]
fn sd_circle(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    (px - cx).hypot(py - cy) - r
}

#[inline]
fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay).max(1e-6)).clamp(0.0, 1.0);
    (pax - bax * h).hypot(pay - bay * h) - r
}

#[inline]
fn sd_triangle(px: f32, py: f32, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) -> f32 {
    let e0 = (p1.0 - p0.0, p1.1 - p0.1);
    let e1 = (p2.0 - p1.0, p2.1 - p1.1);
    let e2 = (p0.0 - p2.0, p0.1 - p2.1);
    let v0 = (px - p0.0, py - p0.1);
    let v1 = (px - p1.0, py - p1.1);
    let v2 = (px - p2.0, py - p2.1);
    let dot = |a: (f32, f32), b: (f32, f32)| a.0 * b.0 + a.1 * b.1;
    let pq0 = (
        v0.0 - e0.0 * (dot(v0, e0) / dot(e0, e0)).clamp(0.0, 1.0),
        v0.1 - e0.1 * (dot(v0, e0) / dot(e0, e0)).clamp(0.0, 1.0),
    );
    let pq1 = (
        v1.0 - e1.0 * (dot(v1, e1) / dot(e1, e1)).clamp(0.0, 1.0),
        v1.1 - e1.1 * (dot(v1, e1) / dot(e1, e1)).clamp(0.0, 1.0),
    );
    let pq2 = (
        v2.0 - e2.0 * (dot(v2, e2) / dot(e2, e2)).clamp(0.0, 1.0),
        v2.1 - e2.1 * (dot(v2, e2) / dot(e2, e2)).clamp(0.0, 1.0),
    );
    let s = (e0.0 * e2.1 - e0.1 * e2.0).signum();
    let d0 = (dot(pq0, pq0), s * (v0.0 * e0.1 - v0.1 * e0.0));
    let d1 = (dot(pq1, pq1), s * (v1.0 * e1.1 - v1.1 * e1.0));
    let d2 = (dot(pq2, pq2), s * (v2.0 * e2.1 - v2.1 * e2.0));
    let min_d = d0.0.min(d1.0).min(d2.0);
    let min_s = d0.1.min(d1.1).min(d2.1);
    -min_d.sqrt() * min_s.signum()
}

/// Evaluate the signed distance (negative inside) for a named icon in [-1, 1]^2.
fn icon_sdf(name: &str, x: f32, y: f32) -> f32 {
    match name {
        "play" => sd_triangle(x, y, (-0.36, -0.58), (-0.36, 0.58), (0.58, 0.0)) - 0.06,
        "pause" => {
            let l = sd_rounded_box(x + 0.30, y, 0.16, 0.54, 0.07);
            let r = sd_rounded_box(x - 0.30, y, 0.16, 0.54, 0.07);
            l.min(r)
        }
        "next" => {
            let tri = sd_triangle(x, y, (-0.54, -0.48), (-0.54, 0.48), (0.24, 0.0)) - 0.05;
            let bar = sd_rounded_box(x - 0.44, y, 0.10, 0.48, 0.05);
            tri.min(bar)
        }
        "prev" | "previous" => {
            let tri = sd_triangle(x, y, (0.54, -0.48), (0.54, 0.48), (-0.24, 0.0)) - 0.05;
            let bar = sd_rounded_box(x + 0.44, y, 0.10, 0.48, 0.05);
            tri.min(bar)
        }
        "close" | "x" => {
            let a = sd_segment(x, y, -0.44, -0.44, 0.44, 0.44, 0.10);
            let b = sd_segment(x, y, 0.44, -0.44, -0.44, 0.44, 0.10);
            a.min(b)
        }
        "check" => {
            let a = sd_segment(x, y, -0.48, 0.04, -0.14, 0.42, 0.11);
            let b = sd_segment(x, y, -0.14, 0.42, 0.54, -0.38, 0.11);
            a.min(b)
        }
        "chevron-down" => {
            let a = sd_segment(x, y, -0.44, -0.18, 0.0, 0.28, 0.10);
            let b = sd_segment(x, y, 0.0, 0.28, 0.44, -0.18, 0.10);
            a.min(b)
        }
        "chevron-up" => {
            let a = sd_segment(x, y, -0.44, 0.22, 0.0, -0.24, 0.10);
            let b = sd_segment(x, y, 0.0, -0.24, 0.44, 0.22, 0.10);
            a.min(b)
        }
        "chevron-right" => {
            let a = sd_segment(x, y, -0.18, -0.44, 0.28, 0.0, 0.10);
            let b = sd_segment(x, y, 0.28, 0.0, -0.18, 0.44, 0.10);
            a.min(b)
        }
        "plus" => {
            let h = sd_segment(x, y, -0.48, 0.0, 0.48, 0.0, 0.10);
            let v = sd_segment(x, y, 0.0, -0.48, 0.0, 0.48, 0.10);
            h.min(v)
        }
        "minus" => sd_segment(x, y, -0.48, 0.0, 0.48, 0.0, 0.10),
        "keyboard" => {
            let outer = sd_rounded_box(x, y, 0.78, 0.50, 0.14).abs() - 0.08;
            let k1 = sd_rounded_box(x + 0.42, y + 0.12, 0.09, 0.08, 0.03);
            let k2 = sd_rounded_box(x + 0.14, y + 0.12, 0.09, 0.08, 0.03);
            let k3 = sd_rounded_box(x - 0.14, y + 0.12, 0.09, 0.08, 0.03);
            let k4 = sd_rounded_box(x - 0.42, y + 0.12, 0.09, 0.08, 0.03);
            let space = sd_rounded_box(x, y - 0.20, 0.36, 0.07, 0.03);
            outer.min(k1).min(k2).min(k3).min(k4).min(space)
        }
        "audio" | "music" | "volume" => {
            // Crisp eighth-note pair
            let n1 = sd_circle(x, y, -0.34, 0.42, 0.20);
            let n2 = sd_circle(x, y, 0.34, 0.28, 0.20);
            let s1 = sd_segment(x, y, -0.18, 0.38, -0.18, -0.44, 0.07);
            let s2 = sd_segment(x, y, 0.50, 0.24, 0.50, -0.56, 0.07);
            let beam = sd_segment(x, y, -0.18, -0.44, 0.50, -0.56, 0.09);
            n1.min(n2).min(s1).min(s2).min(beam)
        }
        "xray" | "activity" => {
            // Heartbeat / diagnostic pulse waveform
            let s1 = sd_segment(x, y, -0.78, 0.05, -0.36, 0.05, 0.08);
            let s2 = sd_segment(x, y, -0.36, 0.05, -0.16, -0.52, 0.08);
            let s3 = sd_segment(x, y, -0.16, -0.52, 0.16, 0.54, 0.08);
            let s4 = sd_segment(x, y, 0.16, 0.54, 0.38, 0.05, 0.08);
            let s5 = sd_segment(x, y, 0.38, 0.05, 0.78, 0.05, 0.08);
            s1.min(s2).min(s3).min(s4).min(s5)
        }
        "undo" => {
            let shaft = sd_segment(x, y, -0.42, -0.10, 0.32, -0.10, 0.08);
            let bend = sd_segment(x, y, 0.32, -0.10, 0.50, 0.24, 0.08);
            let tail = sd_segment(x, y, 0.50, 0.24, 0.10, 0.44, 0.08);
            let h1 = sd_segment(x, y, -0.46, -0.10, -0.14, -0.42, 0.08);
            let h2 = sd_segment(x, y, -0.46, -0.10, -0.14, 0.22, 0.08);
            shaft.min(bend).min(tail).min(h1).min(h2)
        }
        "redo" => icon_sdf("undo", -x, y),
        "refresh" | "reload" => {
            let ring = (x.hypot(y) - 0.54).abs() - 0.085;
            let gap = sd_box(x - 0.45, y + 0.30, 0.26, 0.26);
            let arc = ring.max(-gap);
            let a1 = sd_segment(x, y, 0.46, -0.28, 0.46, -0.64, 0.08);
            let a2 = sd_segment(x, y, 0.46, -0.28, 0.12, -0.28, 0.08);
            arc.min(a1).min(a2)
        }
        "layers" => {
            let top = sd_segment(x, y, -0.60, -0.34, 0.60, -0.34, 0.09);
            let mid = sd_segment(x, y, -0.60, 0.0, 0.60, 0.0, 0.09);
            let bot = sd_segment(x, y, -0.60, 0.34, 0.60, 0.34, 0.09);
            top.min(mid).min(bot)
        }
        "sliders" | "settings" => {
            let l1 = sd_segment(x, y, -0.68, -0.38, 0.68, -0.38, 0.065);
            let l2 = sd_segment(x, y, -0.68, 0.0, 0.68, 0.0, 0.065);
            let l3 = sd_segment(x, y, -0.68, 0.38, 0.68, 0.38, 0.065);
            let k1 = sd_circle(x, y, -0.22, -0.38, 0.16);
            let k2 = sd_circle(x, y, 0.28, 0.0, 0.16);
            let k3 = sd_circle(x, y, -0.10, 0.38, 0.16);
            l1.min(l2).min(l3).min(k1).min(k2).min(k3)
        }
        "save" => {
            let outer = sd_rounded_box(x, y, 0.64, 0.64, 0.12).abs() - 0.08;
            let top = sd_rounded_box(x, y + 0.36, 0.32, 0.22, 0.04);
            let bot = sd_rounded_box(x, y - 0.28, 0.36, 0.22, 0.04);
            outer.min(top).min(bot)
        }
        "lock" => {
            let body = sd_rounded_box(x, y - 0.22, 0.48, 0.38, 0.10);
            let shackle = ((x.hypot(y + 0.26) - 0.30).abs() - 0.08).max(y + 0.16);
            body.min(shackle)
        }
        "warning" => {
            let tri = sd_triangle(x, y, (0.0, -0.64), (0.66, 0.56), (-0.66, 0.56)).abs() - 0.07;
            let stem = sd_segment(x, y, 0.0, -0.20, 0.0, 0.16, 0.075);
            let dot = sd_circle(x, y, 0.0, 0.36, 0.08);
            tri.min(stem).min(dot)
        }
        "error" => {
            let ring = (x.hypot(y) - 0.64).abs() - 0.08;
            let a = sd_segment(x, y, -0.28, -0.28, 0.28, 0.28, 0.085);
            let b = sd_segment(x, y, 0.28, -0.28, -0.28, 0.28, 0.085);
            ring.min(a).min(b)
        }
        "sparkle" | "sparkles" | "background" => {
            let v = sd_segment(x, y, 0.0, -0.66, 0.0, 0.66, 0.08);
            let h = sd_segment(x, y, -0.66, 0.0, 0.66, 0.0, 0.08);
            let d1 = sd_segment(x, y, -0.40, -0.40, 0.40, 0.40, 0.06);
            let d2 = sd_segment(x, y, 0.40, -0.40, -0.40, 0.40, 0.06);
            v.min(h).min(d1).min(d2)
        }
        "eye" => {
            let top = ((x * 0.72).hypot(y + 0.52) - 0.84).abs() - 0.07;
            let bot = ((x * 0.72).hypot(y - 0.52) - 0.84).abs() - 0.07;
            let almond = top.min(bot).max(x.abs() - 0.68);
            let pupil = sd_circle(x, y, 0.0, 0.0, 0.22);
            almond.min(pupil)
        }
        "trash" => {
            let lid = sd_segment(x, y, -0.52, -0.44, 0.52, -0.44, 0.075);
            let handle = sd_segment(x, y, -0.18, -0.60, 0.18, -0.60, 0.07);
            let box_d = sd_rounded_box(x, y - 0.14, 0.38, 0.46, 0.08).abs() - 0.07;
            lid.min(handle).min(box_d)
        }
        _ => sd_circle(x, y, 0.0, 0.0, 0.34),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_icons_rasterize_with_non_zero_antialiased_alpha() {
        for name in [
            "play", "pause", "prev", "next", "keyboard", "audio", "xray", "close", "check",
            "sliders", "layers", "undo", "redo", "save", "refresh",
        ] {
            let (id, rgba) = rasterize_icon(name, 24, [1.0, 1.0, 1.0, 1.0]);
            assert_ne!(id, 0);
            assert_eq!(rgba.len(), 24 * 24 * 4);
            let opaque = rgba.chunks_exact(4).filter(|p| p[3] > 200).count();
            let aa = rgba
                .chunks_exact(4)
                .filter(|p| p[3] > 0 && p[3] <= 200)
                .count();
            assert!(opaque > 10, "icon {name} should have visible interior");
            assert!(aa > 4, "icon {name} should have anti-aliased edges");
        }
    }
}
