// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Terminal inline graphics protocols (Kitty Graphics Protocol & SIXEL) and
//! Base64 utilities for OSC 52 / APC payloads.
//!
//! Security & resource limits:
//! - File (`t=f`), temp-file (`t=t`), and shared-memory (`t=s`) transports in
//!   Kitty Graphics are intentionally disabled and answered with `ENOTSUP` so
//!   untrusted terminal output can never read or delete local files.
//! - Image dimensions are validated *before* pixel decoding (`image::Limits` +
//!   header dimension probe for PNG/JPEG/WebP, exact byte-length validation for
//!   raw RGB/RGBA) to prevent decompression bombs and unbounded allocations.
//! - Placement cell spans and cursor scroll advances are strictly bounded so
//!   malicious parameters like `r=99999999` can never hang the terminal.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

pub const MAX_IMAGE_DIM: u32 = 2048;
pub const MAX_IMAGE_PIXELS: usize = (MAX_IMAGE_DIM as usize) * (MAX_IMAGE_DIM as usize);
pub const MAX_SINGLE_IMAGE_BYTES: usize = MAX_IMAGE_PIXELS * 4; // 16 MiB RGBA max per image
pub const MAX_TOTAL_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_APC_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SIXEL_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SIXEL_OPS: usize = 2_000_000;
pub const MAX_PLACEMENT_COLS: usize = 512;
pub const MAX_PLACEMENT_ROWS: usize = 256;
pub const MAX_PLACEMENTS: usize = 64;
pub const MAX_STORED_IMAGES: usize = 64;

/// A placed inline terminal image anchored to an absolute grid row.
#[derive(Clone, Debug)]
pub struct TerminalImage {
    pub id: u64,
    pub kitty_id: u32,
    pub abs_row: usize,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub src_w: u32,
    pub src_h: u32,
    pub rgba: Arc<Vec<u8>>,
    pub alternate: bool,
}

/// Decode standard Base64 (ignoring ASCII whitespace, tolerant of missing `=`)
/// with a strict decoded output byte cap enforced *before* and *during* decoding.
pub fn base64_decode_capped(input: &[u8], max_decoded_bytes: usize) -> Option<Vec<u8>> {
    // Reject absurdly large inputs before allocating anything.
    if input.len() > max_decoded_bytes.saturating_mul(2).saturating_add(64) {
        return None;
    }
    let est = ((input.len() / 4) * 3).min(max_decoded_bytes);
    let mut out = Vec::with_capacity(est);
    let mut buf = [0u8; 4];
    let mut len = 0usize;
    let mut pad = 0usize;

    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
            buf[len] = 0;
            len += 1;
        } else {
            if pad > 0 {
                return None;
            }
            let val = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                _ => return None,
            };
            buf[len] = val;
            len += 1;
        }
        if len == 4 {
            let add = if pad == 0 {
                3
            } else if pad == 1 {
                2
            } else if pad == 2 {
                1
            } else {
                return None;
            };
            if out.len().saturating_add(add) > max_decoded_bytes {
                return None;
            }
            out.push((buf[0] << 2) | (buf[1] >> 4));
            if pad < 2 {
                out.push((buf[1] << 4) | (buf[2] >> 2));
            }
            if pad < 1 {
                out.push((buf[2] << 6) | buf[3]);
            }
            len = 0;
            pad = 0;
        }
    }
    if len == 2 {
        if out.len().saturating_add(1) > max_decoded_bytes {
            return None;
        }
        out.push((buf[0] << 2) | (buf[1] >> 4));
    } else if len == 3 {
        if out.len().saturating_add(2) > max_decoded_bytes {
            return None;
        }
        out.push((buf[0] << 2) | (buf[1] >> 4));
        out.push((buf[1] << 4) | (buf[2] >> 2));
    } else if len == 1 {
        return None;
    }
    Some(out)
}

/// Decode standard Base64 up to `MAX_SINGLE_IMAGE_BYTES`.
#[cfg(test)]
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    base64_decode_capped(input, MAX_SINGLE_IMAGE_BYTES)
}

/// Encode bytes as standard Base64.
#[allow(dead_code)]
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Stored Kitty image payload waiting for placement or reuse.
#[derive(Clone, Debug)]
pub struct StoredKittyImage {
    pub src_w: u32,
    pub src_h: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// Ongoing chunked Kitty Graphics transfer.
#[derive(Default)]
struct PendingChunk {
    action: char,
    format: u32,
    id: u32,
    width: u32,
    height: u32,
    cols: usize,
    rows: usize,
    quiet: u8,
    payload: Vec<u8>,
}

/// Stateful Kitty Graphics Protocol & image cache manager.
#[derive(Default)]
pub struct GraphicsState {
    pub stored: HashMap<u32, StoredKittyImage>,
    pub placements: Vec<TerminalImage>,
    /// Monotonically incremented whenever visible placements change (for renderer damage tracking).
    pub revision: u64,
    next_internal_id: u64,
    pending_chunk: Option<PendingChunk>,
    total_bytes: usize,
}

impl GraphicsState {
    pub fn next_id(&mut self) -> u64 {
        self.next_internal_id = self.next_internal_id.wrapping_add(1).max(1);
        self.next_internal_id
    }

    pub fn clear_all(&mut self) {
        if !self.placements.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.placements.clear();
        self.stored.clear();
        self.pending_chunk = None;
        self.total_bytes = 0;
    }

    pub fn clear_for_screen(&mut self, alternate: bool) {
        let before = self.placements.len();
        self.placements.retain(|img| img.alternate != alternate);
        if self.placements.len() != before {
            self.revision = self.revision.wrapping_add(1);
            self.recompute_bytes();
        }
    }

    pub fn prune_before(&mut self, oldest_abs_row: usize) {
        let before = self.placements.len();
        self.placements
            .retain(|img| img.alternate || img.abs_row + img.rows > oldest_abs_row);
        if self.placements.len() != before {
            self.revision = self.revision.wrapping_add(1);
            self.recompute_bytes();
        }
    }

    fn recompute_bytes(&mut self) {
        let stored_bytes: usize = self.stored.values().map(|s| s.rgba.len()).sum();
        let placed_bytes: usize = self.placements.iter().map(|p| p.rgba.len()).sum();
        self.total_bytes = stored_bytes + placed_bytes;
    }

    fn enforce_memory_cap(&mut self) {
        let mut evicted_placement = false;
        while (self.total_bytes > MAX_TOTAL_IMAGE_BYTES || self.placements.len() > MAX_PLACEMENTS)
            && !self.placements.is_empty()
        {
            self.placements.remove(0);
            evicted_placement = true;
            self.recompute_bytes();
        }
        while (self.total_bytes > MAX_TOTAL_IMAGE_BYTES || self.stored.len() > MAX_STORED_IMAGES)
            && !self.stored.is_empty()
        {
            if let Some(&key) = self.stored.keys().next() {
                self.stored.remove(&key);
            }
            self.recompute_bytes();
        }
        if evicted_placement {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn add_placement(&mut self, img: TerminalImage) {
        self.total_bytes = self.total_bytes.saturating_add(img.rgba.len());
        self.placements.push(img);
        self.revision = self.revision.wrapping_add(1);
        self.enforce_memory_cap();
    }

    /// Process a single Kitty Graphics APC payload (after `ESC _ G`, before `ST`).
    /// Returns `(response_bytes, cursor_advance_rows)`.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_kitty_apc(
        &mut self,
        body: &[u8],
        cursor_abs_row: usize,
        cursor_col: usize,
        cell_px: (usize, usize),
        grid_cols: usize,
        grid_rows: usize,
        alternate: bool,
    ) -> (Vec<u8>, usize) {
        let (ctrl_bytes, data_bytes) = match body.iter().position(|&b| b == b';') {
            Some(pos) => (&body[..pos], &body[pos + 1..]),
            None => (body, &[][..]),
        };

        let mut action = None::<char>;
        let mut format = None::<u32>;
        let mut medium = 'd';
        let mut id = None::<u32>;
        let mut width = None::<u32>;
        let mut height = None::<u32>;
        let mut cols = None::<usize>;
        let mut rows = None::<usize>;
        let mut more = 0u8;
        let mut quiet = 0u8;
        let mut delete_target = None::<char>;

        for pair in ctrl_bytes.split(|&b| b == b',') {
            if let Some(eq) = pair.iter().position(|&b| b == b'=') {
                let key = &pair[..eq];
                let val = std::str::from_utf8(&pair[eq + 1..]).unwrap_or("");
                match key {
                    b"a" => action = val.chars().next(),
                    b"f" => format = val.parse().ok(),
                    b"t" => medium = val.chars().next().unwrap_or('d'),
                    b"i" => id = val.parse().ok(),
                    b"s" => width = val.parse().ok(),
                    b"v" => height = val.parse().ok(),
                    b"c" => cols = val.parse::<usize>().ok().map(|v| v.min(MAX_PLACEMENT_COLS)),
                    b"r" => rows = val.parse::<usize>().ok().map(|v| v.min(MAX_PLACEMENT_ROWS)),
                    b"m" => more = val.parse().unwrap_or(0),
                    b"q" => quiet = val.parse().unwrap_or(0),
                    b"d" => delete_target = val.chars().next(),
                    _ => {}
                }
            }
        }

        // Security: never allow terminal output to read or delete arbitrary local files or shm.
        if medium != 'd' {
            self.pending_chunk = None;
            return (
                kitty_reply(
                    id.unwrap_or(0),
                    quiet,
                    "ENOTSUP:only direct transfer (t=d) is enabled",
                ),
                0,
            );
        }

        // Continuation of an active chunked transfer.
        if let Some(mut chunk) = self.pending_chunk.take() {
            if chunk.payload.len().saturating_add(data_bytes.len()) > MAX_APC_PAYLOAD_BYTES {
                return (
                    kitty_reply(chunk.id, chunk.quiet, "EFBIG:payload exceeds limit"),
                    0,
                );
            }
            chunk.payload.extend_from_slice(data_bytes);
            if more == 1 {
                self.pending_chunk = Some(chunk);
                return (Vec::new(), 0);
            }
            return self.finalize_kitty_transfer(
                chunk.action,
                chunk.format,
                chunk.id,
                chunk.width,
                chunk.height,
                chunk.cols,
                chunk.rows,
                chunk.quiet,
                &chunk.payload,
                cursor_abs_row,
                cursor_col,
                cell_px,
                grid_cols,
                grid_rows,
                alternate,
            );
        }

        let act = action.unwrap_or('t');
        match act {
            'q' => (kitty_reply(id.unwrap_or(0), quiet, "OK"), 0),
            'd' => {
                let target = delete_target.unwrap_or('a');
                let before = self.placements.len();
                match target {
                    'a' | 'A' => self.clear_for_screen(alternate),
                    'i' | 'I' => {
                        if let Some(kid) = id {
                            self.placements.retain(|p| p.kitty_id != kid);
                            if target == 'I' {
                                self.stored.remove(&kid);
                            }
                            self.recompute_bytes();
                        }
                    }
                    'c' | 'C' => {
                        self.placements.retain(|p| {
                            p.alternate != alternate
                                || !(cursor_abs_row >= p.abs_row
                                    && cursor_abs_row < p.abs_row + p.rows
                                    && cursor_col >= p.col
                                    && cursor_col < p.col + p.cols)
                        });
                        self.recompute_bytes();
                    }
                    _ => self.clear_for_screen(alternate),
                }
                if self.placements.len() != before {
                    self.revision = self.revision.wrapping_add(1);
                }
                (Vec::new(), 0)
            }
            'p' => {
                let kid = id.unwrap_or(0);
                let Some(stored) = self.stored.get(&kid).cloned() else {
                    return (kitty_reply(kid, quiet, "ENOENT:image not found"), 0);
                };
                let (place_cols, place_rows) = compute_cell_span(
                    stored.src_w,
                    stored.src_h,
                    cols.unwrap_or(0),
                    rows.unwrap_or(0),
                    cell_px,
                    grid_cols,
                    grid_rows,
                );
                let internal_id = self.next_id();
                self.add_placement(TerminalImage {
                    id: internal_id,
                    kitty_id: kid,
                    abs_row: cursor_abs_row,
                    col: cursor_col,
                    cols: place_cols,
                    rows: place_rows,
                    src_w: stored.src_w,
                    src_h: stored.src_h,
                    rgba: stored.rgba,
                    alternate,
                });
                (kitty_reply(kid, quiet, "OK"), place_rows)
            }
            't' | 'T' => {
                let fmt = format.unwrap_or(32);
                let kid = id.unwrap_or(0);
                // Validate raw dimensions up front before buffering chunks.
                if matches!(fmt, 24 | 32) {
                    let w = width.unwrap_or(0);
                    let h = height.unwrap_or(0);
                    if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
                        return (kitty_reply(kid, quiet, "EINVAL:invalid dimensions"), 0);
                    }
                }
                if more == 1 {
                    if data_bytes.len() > MAX_APC_PAYLOAD_BYTES {
                        return (kitty_reply(kid, quiet, "EFBIG:payload exceeds limit"), 0);
                    }
                    self.pending_chunk = Some(PendingChunk {
                        action: act,
                        format: fmt,
                        id: kid,
                        width: width.unwrap_or(0),
                        height: height.unwrap_or(0),
                        cols: cols.unwrap_or(0),
                        rows: rows.unwrap_or(0),
                        quiet,
                        payload: data_bytes.to_vec(),
                    });
                    return (Vec::new(), 0);
                }
                self.finalize_kitty_transfer(
                    act,
                    fmt,
                    kid,
                    width.unwrap_or(0),
                    height.unwrap_or(0),
                    cols.unwrap_or(0),
                    rows.unwrap_or(0),
                    quiet,
                    data_bytes,
                    cursor_abs_row,
                    cursor_col,
                    cell_px,
                    grid_cols,
                    grid_rows,
                    alternate,
                )
            }
            _ => (
                kitty_reply(id.unwrap_or(0), quiet, "ENOTSUP:unsupported action"),
                0,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_kitty_transfer(
        &mut self,
        action: char,
        format: u32,
        kitty_id: u32,
        width: u32,
        height: u32,
        cols: usize,
        rows: usize,
        quiet: u8,
        b64_payload: &[u8],
        cursor_abs_row: usize,
        cursor_col: usize,
        cell_px: (usize, usize),
        grid_cols: usize,
        grid_rows: usize,
        alternate: bool,
    ) -> (Vec<u8>, usize) {
        // Pre-validate raw pixel dimensions & expected byte size BEFORE base64 decoding.
        let max_decoded = match format {
            32 => {
                if width == 0 || height == 0 || width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
                    return (kitty_reply(kitty_id, quiet, "EINVAL:invalid dimensions"), 0);
                }
                let Some(bytes) = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|p| p.checked_mul(4))
                else {
                    return (kitty_reply(kitty_id, quiet, "EFBIG:dimension overflow"), 0);
                };
                if bytes > MAX_SINGLE_IMAGE_BYTES {
                    return (kitty_reply(kitty_id, quiet, "EFBIG:image too large"), 0);
                }
                bytes
            }
            24 => {
                if width == 0 || height == 0 || width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
                    return (kitty_reply(kitty_id, quiet, "EINVAL:invalid dimensions"), 0);
                }
                let Some(bytes) = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|p| p.checked_mul(3))
                else {
                    return (kitty_reply(kitty_id, quiet, "EFBIG:dimension overflow"), 0);
                };
                if bytes > MAX_SINGLE_IMAGE_BYTES {
                    return (kitty_reply(kitty_id, quiet, "EFBIG:image too large"), 0);
                }
                bytes
            }
            100 => MAX_SINGLE_IMAGE_BYTES,
            _ => {
                return (
                    kitty_reply(kitty_id, quiet, "ENOTSUP:unsupported pixel format"),
                    0,
                );
            }
        };

        let Some(raw) = base64_decode_capped(b64_payload, max_decoded) else {
            return (
                kitty_reply(kitty_id, quiet, "EINVAL:invalid or oversized base64"),
                0,
            );
        };
        let decoded = match format {
            100 => decode_png_rgba(&raw),
            32 => decode_raw_rgba(&raw, width, height),
            24 => decode_raw_rgb(&raw, width, height),
            _ => Err("ENOTSUP:unsupported pixel format"),
        };
        let (src_w, src_h, rgba) = match decoded {
            Ok(v) => v,
            Err(msg) => return (kitty_reply(kitty_id, quiet, msg), 0),
        };
        let rgba = Arc::new(rgba);
        if kitty_id != 0 {
            self.stored.insert(
                kitty_id,
                StoredKittyImage {
                    src_w,
                    src_h,
                    rgba: Arc::clone(&rgba),
                },
            );
            self.recompute_bytes();
            self.enforce_memory_cap();
        }
        let mut advance_rows = 0;
        if action == 'T' {
            let (place_cols, place_rows) =
                compute_cell_span(src_w, src_h, cols, rows, cell_px, grid_cols, grid_rows);
            let internal_id = self.next_id();
            self.add_placement(TerminalImage {
                id: internal_id,
                kitty_id,
                abs_row: cursor_abs_row,
                col: cursor_col,
                cols: place_cols,
                rows: place_rows,
                src_w,
                src_h,
                rgba,
                alternate,
            });
            advance_rows = place_rows;
        }
        (kitty_reply(kitty_id, quiet, "OK"), advance_rows)
    }
}

fn kitty_reply(id: u32, quiet: u8, status: &str) -> Vec<u8> {
    if quiet >= 2 || (quiet == 1 && status == "OK") {
        return Vec::new();
    }
    if id != 0 {
        format!("\x1b_Gi={id};{status}\x1b\\").into_bytes()
    } else {
        format!("\x1b_G;{status}\x1b\\").into_bytes()
    }
}

/// Decode compressed image (PNG/JPEG/WebP) with strict pre-decode dimension and
/// allocation limits so decompression bombs are rejected before pixel allocation.
fn decode_png_rgba(raw: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIM);
    limits.max_image_height = Some(MAX_IMAGE_DIM);
    limits.max_alloc = Some(MAX_SINGLE_IMAGE_BYTES as u64);

    let mut reader = image::ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .map_err(|_| "EINVAL:unrecognized image format")?;
    reader.limits(limits.clone());

    let (w, h) = reader
        .into_dimensions()
        .map_err(|_| "EINVAL:invalid or oversized image header")?;
    if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        return Err("EFBIG:image dimensions out of bounds");
    }
    let rgba_bytes = (w as usize)
        .checked_mul(h as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or("EFBIG:dimension overflow")?;
    if rgba_bytes > MAX_SINGLE_IMAGE_BYTES {
        return Err("EFBIG:image exceeds memory limit");
    }

    let mut decoder = image::ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .map_err(|_| "EINVAL:unrecognized image format")?;
    decoder.limits(limits);
    let img = decoder.decode().map_err(|_| "EINVAL:invalid image data")?;
    Ok((img.width(), img.height(), img.to_rgba8().into_raw()))
}

fn decode_raw_rgba(raw: &[u8], w: u32, h: u32) -> Result<(u32, u32, Vec<u8>), &'static str> {
    if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        return Err("EINVAL:invalid dimensions");
    }
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|p| p.checked_mul(4));
    if expected != Some(raw.len()) {
        return Err("EINVAL:RGBA byte count mismatch");
    }
    Ok((w, h, raw.to_vec()))
}

fn decode_raw_rgb(raw: &[u8], w: u32, h: u32) -> Result<(u32, u32, Vec<u8>), &'static str> {
    if w == 0 || h == 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        return Err("EINVAL:invalid dimensions");
    }
    let pixels = (w as usize)
        .checked_mul(h as usize)
        .ok_or("EFBIG:overflow")?;
    if pixels > MAX_IMAGE_PIXELS || raw.len() != pixels * 3 {
        return Err("EINVAL:RGB byte count mismatch");
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    for chunk in raw.as_chunks::<3>().0 {
        rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
    }
    Ok((w, h, rgba))
}

pub fn compute_cell_span(
    src_w: u32,
    src_h: u32,
    req_cols: usize,
    req_rows: usize,
    cell_px: (usize, usize),
    grid_cols: usize,
    grid_rows: usize,
) -> (usize, usize) {
    let cw = cell_px.0.max(1);
    let ch = cell_px.1.max(1);
    let max_cols = grid_cols.clamp(1, MAX_PLACEMENT_COLS);
    let max_rows = grid_rows.clamp(1, MAX_PLACEMENT_ROWS);
    let cols = if req_cols > 0 {
        req_cols
    } else {
        (src_w as usize).div_ceil(cw).max(1)
    }
    .clamp(1, max_cols);
    let rows = if req_rows > 0 {
        req_rows
    } else {
        (src_h as usize).div_ceil(ch).max(1)
    }
    .clamp(1, max_rows);
    (cols, rows)
}

/// Decode a VT340 SIXEL payload into `(width, height, rgba)` with strict
/// pre-bounded row-stripe storage (no HashMap OOM) and bounded RLE work.
pub fn decode_sixel(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.is_empty() || data.len() > MAX_SIXEL_PAYLOAD_BYTES {
        return None;
    }
    let mut palette = [[0u8; 3]; 256];
    let defaults: [[u8; 3]; 16] = [
        [0, 0, 0],
        [51, 51, 204],
        [204, 33, 33],
        [51, 204, 51],
        [204, 51, 204],
        [51, 204, 204],
        [204, 204, 51],
        [135, 135, 135],
        [66, 66, 66],
        [84, 84, 255],
        [255, 84, 84],
        [84, 255, 84],
        [255, 84, 255],
        [84, 255, 255],
        [255, 255, 84],
        [255, 255, 255],
    ];
    palette[..16].copy_from_slice(&defaults);

    let max_dim = MAX_IMAGE_DIM as usize;
    let mut cur_color: usize = 0;
    let mut x: usize = 0;
    let mut y: usize = 0;
    let mut max_x: usize = 0;
    let mut max_y: usize = 0;
    let mut ops: usize = 0;

    // Row-oriented RGBA stripes: each entry is a row of `[u8; 4]` grown lazily
    // up to `max_dim`, keeping worst-case memory strictly <= 16 MiB.
    let mut rows: Vec<Vec<[u8; 4]>> = Vec::new();
    let mut set_pixel = |px: usize, py: usize, rgb: [u8; 3]| {
        if px >= max_dim || py >= max_dim {
            return;
        }
        if rows.len() <= py {
            rows.resize_with(py + 1, Vec::new);
        }
        let row = &mut rows[py];
        if row.len() <= px {
            row.resize(px + 1, [0, 0, 0, 0]);
        }
        row[px] = [rgb[0], rgb[1], rgb[2], 255];
        max_x = max_x.max(px + 1);
        max_y = max_y.max(py + 1);
    };

    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        match b {
            b'$' => {
                x = 0;
                i += 1;
            }
            b'-' => {
                x = 0;
                y = y.saturating_add(6);
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
            }
            b'#' => {
                i += 1;
                let mut params = Vec::new();
                let mut num: Option<usize> = None;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    if data[i].is_ascii_digit() {
                        num = Some(
                            num.unwrap_or(0)
                                .saturating_mul(10)
                                .saturating_add((data[i] - b'0') as usize),
                        );
                    } else {
                        if params.len() < 8 {
                            params.push(num.unwrap_or(0));
                        }
                        num = None;
                    }
                    i += 1;
                }
                if let Some(v) = num
                    && params.len() < 8
                {
                    params.push(v);
                }
                if let Some(&reg) = params.first() {
                    cur_color = reg % 256;
                    if params.len() >= 5 && params[1] == 2 {
                        let r = ((params[2].min(100) * 255) / 100) as u8;
                        let g = ((params[3].min(100) * 255) / 100) as u8;
                        let bl = ((params[4].min(100) * 255) / 100) as u8;
                        palette[cur_color] = [r, g, bl];
                    }
                }
            }
            b'!' => {
                i += 1;
                let mut count = 0usize;
                while i < data.len() && data[i].is_ascii_digit() {
                    count = count
                        .saturating_mul(10)
                        .saturating_add((data[i] - b'0') as usize)
                        .min(max_dim);
                    i += 1;
                }
                if i < data.len() && (0x3f..=0x7e).contains(&data[i]) {
                    let bits = data[i] - 0x3f;
                    let rep = count.max(1).min(max_dim.saturating_sub(x));
                    let rgb = palette[cur_color];
                    if bits != 0 && y < max_dim {
                        for _ in 0..rep {
                            ops = ops.saturating_add(1);
                            if ops > MAX_SIXEL_OPS {
                                return None;
                            }
                            for bit in 0..6 {
                                if (bits & (1 << bit)) != 0 {
                                    set_pixel(x, y + bit, rgb);
                                }
                            }
                            x = x.saturating_add(1);
                        }
                    } else {
                        x = x.saturating_add(rep).min(max_dim);
                    }
                    i += 1;
                }
            }
            0x3f..=0x7e => {
                let bits = b - 0x3f;
                let rgb = palette[cur_color];
                ops = ops.saturating_add(1);
                if ops > MAX_SIXEL_OPS {
                    return None;
                }
                if bits != 0 && x < max_dim && y < max_dim {
                    for bit in 0..6 {
                        if (bits & (1 << bit)) != 0 {
                            set_pixel(x, y + bit, rgb);
                        }
                    }
                }
                x = x.saturating_add(1).min(max_dim);
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    if max_x == 0 || max_y == 0 || max_x > max_dim || max_y > max_dim {
        return None;
    }
    let total_bytes = max_x.checked_mul(max_y)?.checked_mul(4)?;
    if total_bytes > MAX_SINGLE_IMAGE_BYTES {
        return None;
    }
    let mut rgba = vec![0u8; total_bytes];
    for (py, row) in rows.into_iter().take(max_y).enumerate() {
        let base = py * max_x * 4;
        for (px, px_rgba) in row.into_iter().take(max_x).enumerate() {
            let off = base + px * 4;
            rgba[off..off + 4].copy_from_slice(&px_rgba);
        }
    }
    Some((max_x as u32, max_y as u32, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_cleanly() {
        let samples: &[&[u8]] = &[
            b"",
            b"a",
            b"ab",
            b"abc",
            b"hello world!",
            b"\x00\xff\x80\x7f",
        ];
        for s in samples {
            let enc = base64_encode(s);
            let dec = base64_decode(enc.as_bytes()).unwrap();
            assert_eq!(&dec, s);
        }
    }

    #[test]
    fn kitty_graphics_handles_chunked_rgba_bounds_and_blocks_file_transport() {
        let mut g = GraphicsState::default();
        // File transport must be refused with ENOTSUP.
        let (reply, adv) = g.handle_kitty_apc(
            b"a=T,t=f,i=7;L2V0Yy9wYXNzd2Q=",
            0,
            0,
            (8, 16),
            80,
            24,
            false,
        );
        assert_eq!(adv, 0);
        assert!(String::from_utf8_lossy(&reply).contains("ENOTSUP"));
        assert!(g.placements.is_empty());

        // Malicious huge r=99999999 must be clamped to grid_rows (24), never hang.
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let b64 = base64_encode(&rgba);
        let (r_clamped, adv_clamped) = g.handle_kitty_apc(
            format!("a=T,f=32,s=2,v=1,r=99999999,c=99999999,i=11;{b64}").as_bytes(),
            0,
            0,
            (8, 16),
            80,
            24,
            false,
        );
        assert_eq!(r_clamped, b"\x1b_Gi=11;OK\x1b\\");
        assert!(adv_clamped <= 24);
        assert!(g.placements[0].cols <= 80);
        assert!(g.placements[0].rows <= 24);
        g.clear_all();

        // Chunked 2x1 RGBA direct transfer.
        let (part1, part2) = b64.split_at(b64.len() / 2);
        let rev_before = g.revision;
        let (r1, a1) = g.handle_kitty_apc(
            format!("a=T,f=32,s=2,v=1,i=42,m=1;{part1}").as_bytes(),
            5,
            2,
            (8, 16),
            80,
            24,
            false,
        );
        assert!(r1.is_empty());
        assert_eq!(a1, 0);
        let (r2, a2) = g.handle_kitty_apc(
            format!("m=0;{part2}").as_bytes(),
            5,
            2,
            (8, 16),
            80,
            24,
            false,
        );
        assert_eq!(r2, b"\x1b_Gi=42;OK\x1b\\");
        assert_eq!(a2, 1);
        assert!(g.revision > rev_before);
        assert_eq!(g.placements.len(), 1);
        assert_eq!(&*g.placements[0].rgba, &rgba);
    }

    #[test]
    fn png_decompression_bomb_header_is_rejected_before_decoding() {
        // Minimal PNG signature + IHDR claiming 30000 x 30000 RGBA (would be 3.6 GB if decoded).
        let mut fake_png = Vec::new();
        fake_png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        fake_png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        fake_png.extend_from_slice(b"IHDR");
        fake_png.extend_from_slice(&30000u32.to_be_bytes()); // width
        fake_png.extend_from_slice(&30000u32.to_be_bytes()); // height
        fake_png.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
        fake_png.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC
        let res = decode_png_rgba(&fake_png);
        assert!(res.is_err());
    }

    #[test]
    fn sixel_decodes_basic_red_band_and_rejects_rle_bomb() {
        let (w, h, rgba) = decode_sixel(b"#1;2;100;0;0#1!4~").unwrap();
        assert_eq!((w, h), (4, 6));
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);

        // RLE bomb repeating huge stripes must terminate promptly without OOM.
        let mut bomb = Vec::new();
        for _ in 0..2000 {
            bomb.extend_from_slice(b"!99999999~-");
        }
        let _ = decode_sixel(&bomb);
    }
}
