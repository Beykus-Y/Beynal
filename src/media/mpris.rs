// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linux MPRIS2 D-Bus media session controller via `zbus::blocking`.

use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::OwnedValue;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MprisCapabilities {
    pub can_control: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MprisSession {
    pub bus_name: String,
    pub identity: String,
    pub track_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    pub cover_art: Option<(u32, u32, Arc<Vec<u8>>)>,
    pub playing: bool,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: Option<f64>,
    pub capabilities: MprisCapabilities,
}

pub type CoverArt = (u32, u32, Arc<Vec<u8>>);

/// Bounded in-memory cache for decoded MPRIS album art so images are not
/// re-read or re-decoded on every poll tick.
#[derive(Default)]
pub struct ArtCache {
    entries: HashMap<String, Option<CoverArt>>,
}

impl ArtCache {
    pub fn get_or_load(&mut self, url: &str) -> Option<CoverArt> {
        if let Some(cached) = self.entries.get(url) {
            return cached.clone();
        }
        if self.entries.len() >= 32 {
            self.entries.clear();
        }
        let loaded = load_local_art(url);
        self.entries.insert(url.to_string(), loaded.clone());
        loaded
    }
}

pub fn list_mpris_players(conn: &zbus::blocking::Connection) -> Vec<String> {
    let Ok(proxy) = zbus::blocking::fdo::DBusProxy::new(conn) else {
        return Vec::new();
    };
    let Ok(names) = proxy.list_names() else {
        return Vec::new();
    };
    names
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .collect()
}

pub fn read_mpris_session(
    conn: &zbus::blocking::Connection,
    bus_name: &str,
    art_cache: &mut ArtCache,
) -> Option<MprisSession> {
    let root_proxy = zbus::blocking::Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2",
    )
    .ok()?;
    let player_proxy = zbus::blocking::Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .ok()?;

    let identity = root_proxy
        .get_property::<String>("Identity")
        .unwrap_or_else(|_| {
            bus_name
                .trim_start_matches("org.mpris.MediaPlayer2.")
                .to_string()
        });
    let playback_status = player_proxy
        .get_property::<String>("PlaybackStatus")
        .unwrap_or_default();
    let playing = playback_status == "Playing";

    let position_us = player_proxy.get_property::<i64>("Position").unwrap_or(0);
    let volume = player_proxy.get_property::<f64>("Volume").ok();
    let capabilities = MprisCapabilities {
        can_control: player_proxy.get_property("CanControl").unwrap_or(false),
        can_play: player_proxy.get_property("CanPlay").unwrap_or(false),
        can_pause: player_proxy.get_property("CanPause").unwrap_or(false),
        can_go_next: player_proxy.get_property("CanGoNext").unwrap_or(false),
        can_go_previous: player_proxy.get_property("CanGoPrevious").unwrap_or(false),
        can_seek: player_proxy.get_property("CanSeek").unwrap_or(false),
    };

    let mut track_id = None;
    let mut title = String::new();
    let mut artist = String::new();
    let mut album = String::new();
    let mut art_url = None;
    let mut duration_secs = 0.0;

    if let Ok(meta) = player_proxy.get_property::<HashMap<String, OwnedValue>>("Metadata") {
        if let Some(v) = meta.get("mpris:trackid") {
            if let Ok(p) = <zbus::zvariant::OwnedObjectPath>::try_from(v.clone()) {
                track_id = Some(p.as_str().to_string());
            } else if let Ok(s) = <&str>::try_from(v) {
                track_id = Some(s.to_string());
            }
        }
        if let Some(v) = meta.get("xesam:title")
            && let Ok(s) = <&str>::try_from(v)
        {
            title = s.to_string();
        }
        if let Some(v) = meta.get("xesam:album")
            && let Ok(s) = <&str>::try_from(v)
        {
            album = s.to_string();
        }
        if let Some(v) = meta.get("xesam:artist")
            && let Ok(arr) = <Vec<String>>::try_from(v.clone())
        {
            artist = arr.join(", ");
        }
        if let Some(v) = meta.get("mpris:artUrl")
            && let Ok(s) = <&str>::try_from(v)
        {
            art_url = Some(s.to_string());
        }
        if let Some(v) = meta.get("mpris:length")
            && let Ok(us) = i64::try_from(v)
        {
            duration_secs = (us.max(0) as f64) / 1_000_000.0;
        }
    }

    let cover_art = art_url.as_deref().and_then(|u| art_cache.get_or_load(u));

    Some(MprisSession {
        bus_name: bus_name.to_string(),
        identity,
        track_id,
        title,
        artist,
        album,
        art_url,
        cover_art,
        playing,
        position_secs: (position_us.max(0) as f64) / 1_000_000.0,
        duration_secs,
        volume,
        capabilities,
    })
}

fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn load_local_art(url: &str) -> Option<(u32, u32, Arc<Vec<u8>>)> {
    let raw_path = url.strip_prefix("file://")?;
    let decoded = percent_decode_path(raw_path);
    let bytes = std::fs::read(decoded).ok()?;
    if bytes.len() > 8 * 1024 * 1024 {
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.thumbnail(128, 128).to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, Arc::new(rgba.into_raw())))
}

pub fn send_mpris_command(
    conn: &zbus::blocking::Connection,
    bus_name: &str,
    method: &str,
) -> Result<(), String> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .map_err(|e| format!("mpris proxy: {e}"))?;
    proxy
        .call_method(method, &())
        .map_err(|e| format!("mpris {method}: {e}"))?;
    Ok(())
}

pub fn send_mpris_seek(
    conn: &zbus::blocking::Connection,
    bus_name: &str,
    position_secs: f64,
    track_id: Option<&str>,
) -> Result<(), String> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .map_err(|e| format!("mpris proxy: {e}"))?;
    let target_us = (position_secs.max(0.0) * 1_000_000.0).round() as i64;
    // Prefer MPRIS SetPosition(track_id, position_us) for exact absolute seeking
    // when the player provides an mpris:trackid ObjectPath.
    if let Some(tid) = track_id
        && let Ok(obj_path) = zbus::zvariant::ObjectPath::try_from(tid)
        && proxy
            .call_method("SetPosition", &(obj_path, target_us))
            .is_ok()
    {
        return Ok(());
    }
    let current_us = proxy.get_property::<i64>("Position").unwrap_or(0);
    let offset_us = target_us - current_us;
    proxy
        .call_method("Seek", &(offset_us,))
        .map_err(|e| format!("mpris Seek: {e}"))?;
    Ok(())
}

pub fn send_mpris_volume(
    conn: &zbus::blocking::Connection,
    bus_name: &str,
    volume: f64,
) -> Result<(), String> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .map_err(|e| format!("mpris proxy: {e}"))?;
    proxy
        .set_property("Volume", volume.clamp(0.0, 1.5))
        .map_err(|e| format!("mpris Volume: {e}"))?;
    Ok(())
}
