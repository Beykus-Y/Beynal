use serde::Deserialize;
use std::{io, path::PathBuf};

pub const ANSI: [[u8; 3]; 16] = [
    [24, 30, 39],
    [232, 127, 133],
    [157, 197, 156],
    [222, 191, 130],
    [137, 174, 224],
    [188, 154, 211],
    [142, 191, 194],
    [213, 220, 228],
    [133, 148, 166],
    [255, 165, 169],
    [189, 224, 181],
    [245, 217, 164],
    [174, 204, 247],
    [220, 182, 241],
    [178, 224, 226],
    [244, 246, 249],
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub cursor: [u8; 3],
    pub ansi: [[u8; 3]; 16],
    pub padding: usize,
    pub font_size: f32,
    pub font_family: String,
    pub name: String,
    pub selection: [u8; 3],
    pub selection_foreground: [u8; 3],
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub scrollback_limit: usize,
    pub scroll_speed: usize,
    pub signature_compact: bool,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: [18, 24, 32],
            foreground: [232, 232, 232],
            cursor: [155, 174, 191],
            ansi: ANSI,
            padding: 12,
            font_size: 16.0,
            font_family: "monospace".into(),
            name: "minimal".into(),
            selection: [192, 192, 192],
            selection_foreground: [0, 0, 0],
            cursor_shape: CursorShape::Underline,
            cursor_blink: false,
            scrollback_limit: 10000,
            scroll_speed: 3,
            signature_compact: false,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    theme: Option<String>,
    selection: Option<String>,
    selection_foreground: Option<String>,
    cursor_shape: Option<String>,
    cursor_blink: Option<bool>,
    scrollback_limit: Option<usize>,
    scroll_speed: Option<usize>,
    signature_compact: bool,
    background: Option<String>,
    foreground: Option<String>,
    cursor: Option<String>,
    ansi: Option<Vec<String>>,
    padding: Option<usize>,
    font_size: Option<f32>,
    font_family: Option<String>,
}
impl Theme {
    pub fn builtin(name: &str) -> Result<Self, String> {
        let mut theme = Self::default();
        match name {
            "minimal" => {}
            "developer" => {
                theme.background = [22, 27, 34];
                theme.foreground = [226, 232, 240];
                theme.cursor = [120, 220, 180];
                theme.padding = 14;
                theme.cursor_shape = CursorShape::Beam;
                theme.selection = [46, 77, 100];
                theme.selection_foreground = [247, 250, 255];
                theme.ansi[2] = [80, 200, 140];
                theme.ansi[4] = [100, 160, 240];
            }
            "signature" => {
                theme.background = [20, 14, 36];
                theme.foreground = [231, 230, 250];
                theme.cursor = [93, 235, 247];
                theme.selection = [88, 64, 153];
                theme.selection_foreground = [250, 246, 255];
                theme.cursor_shape = CursorShape::Block;
                theme.padding = 16;
                theme.ansi[4] = [100, 130, 255];
                theme.ansi[5] = [190, 120, 255];
                theme.ansi[6] = [85, 220, 235];
            }
            _ => {
                return Err(format!(
                    "unknown theme {name:?}; expected minimal, developer, or signature"
                ));
            }
        }
        theme.name = name.to_owned();
        Ok(theme)
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
        let name = config.theme.as_deref().unwrap_or("minimal");
        let mut t = Self::builtin(name)?;
        t.signature_compact = config.signature_compact;
        fn color(s: &str) -> Result<[u8; 3], String> {
            let s = s.strip_prefix('#').ok_or("color must be #RRGGBB")?;
            if s.len() != 6 || !s.is_ascii() {
                return Err("color must be #RRGGBB".into());
            }
            let n = u32::from_str_radix(s, 16).map_err(|_| "color must be #RRGGBB")?;
            Ok([(n >> 16) as u8, (n >> 8) as u8, n as u8])
        }
        if let Some(v) = config.scrollback_limit {
            if v > 100_000 {
                return Err("scrollback_limit must be 0..=100000".into());
            }
            t.scrollback_limit = v;
        }
        if let Some(v) = config.scroll_speed {
            if !(1..=100).contains(&v) {
                return Err("scroll_speed must be 1..=100".into());
            }
            t.scroll_speed = v;
        }
        if let Some(v) = config.cursor_shape {
            t.cursor_shape = match v.as_str() {
                "block" => CursorShape::Block,
                "beam" => CursorShape::Beam,
                "underline" => CursorShape::Underline,
                _ => return Err("cursor_shape must be block, beam, or underline".into()),
            };
        }
        if let Some(v) = config.cursor_blink {
            t.cursor_blink = v;
        }
        if let Some(v) = config.selection_foreground {
            t.selection_foreground = color(&v).map_err(|e| format!("selection_foreground: {e}"))?;
        }
        if let Some(v) = config.selection {
            t.selection = color(&v).map_err(|e| format!("selection: {e}"))?;
        }
        if let Some(v) = config.background {
            t.background = color(&v).map_err(|e| format!("background: {e}"))?;
        }
        if let Some(v) = config.foreground {
            t.foreground = color(&v).map_err(|e| format!("foreground: {e}"))?;
        }
        if let Some(v) = config.cursor {
            t.cursor = color(&v).map_err(|e| format!("cursor: {e}"))?;
        }
        if let Some(v) = config.ansi {
            if v.len() != 16 {
                return Err("ansi must contain exactly 16 colors".into());
            }
            for (index, (dst, src)) in t.ansi.iter_mut().zip(v).enumerate() {
                *dst = color(&src).map_err(|e| format!("ansi[{index}]: {e}"))?;
            }
        }
        if let Some(v) = config.padding {
            if v > 256 {
                return Err("padding must be between 0 and 256".into());
            }
            t.padding = v;
        }
        if let Some(v) = config.font_size {
            if !v.is_finite() || !(6.0..=96.0).contains(&v) {
                return Err("font_size must be between 6 and 96".into());
            }
            t.font_size = v;
        }
        if let Some(v) = config.font_family {
            if v.trim().is_empty() || v.contains(['\n', '\r', '\0']) {
                return Err("invalid font_family".into());
            }
            t.font_family = v;
        }
        Ok(t)
    }
    fn config_path() -> Result<PathBuf, String> {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty() && std::path::Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
        root.map(|root| root.join("beynal/config.toml"))
            .ok_or_else(|| "XDG_CONFIG_HOME and HOME are unavailable".to_string())
    }
    pub fn save_choice_at(path: &std::path::Path, name: &str) -> Result<(), String> {
        Self::builtin(name)?;
        let existing = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut doc: toml_edit::DocumentMut = existing
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(item) = doc.get_mut("theme") {
            let value = item.as_value_mut().ok_or("theme must be a string")?;
            let decor = value.decor().clone();
            *value = toml_edit::Value::from(name);
            *value.decor_mut() = decor;
        } else {
            doc.insert("theme", toml_edit::value(name));
        }
        let content = doc.to_string();
        Self::parse(&content)?;
        let parent = path.parent().ok_or("invalid config path")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        use std::io::Write;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        temp.write_all(content.as_bytes())
            .map_err(|e| e.to_string())?;
        temp.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn save_choice(name: &str) -> Result<(), String> {
        Self::save_choice_at(&Self::config_path()?, name)
    }
    pub fn load_for(name: &str) -> Result<Self, String> {
        Self::builtin(name)?;
        let path = Self::config_path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.to_string()),
        };
        let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
        doc.insert("theme", toml_edit::value(name));
        Self::parse(&doc.to_string())
    }
    fn load() -> Result<Self, String> {
        let path = Self::config_path()?;
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::parse(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}
/// Application configuration root. Future keyboard bindings and actions belong here,
/// independently of visual settings and keyboard event handling.
pub struct AppConfig {
    pub theme: Theme,
}
impl AppConfig {
    pub fn load() -> Result<Self, String> {
        Ok(Self {
            theme: Theme::load()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_comments_and_unknown_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# user comment\ntheme = 'minimal' # retain this\npadding = 3\ncustom = 'kept'\n[future]\noption = true\n";
        std::fs::write(&path, original).unwrap();
        Theme::save_choice_at(&path, "developer").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# user comment"));
        assert!(text.contains("# retain this"));
        assert!(text.contains("custom = 'kept'"));
        assert!(text.contains("[future]\noption = true"));
        assert_eq!(Theme::parse(&text).unwrap().name, "developer");
    }
    #[test]
    fn builtin_override_and_persistence() {
        for name in ["minimal", "developer", "signature"] {
            assert_eq!(Theme::builtin(name).unwrap().name, name);
        }
        assert!(Theme::builtin("unknown").is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "padding=3\nforeground='#123456'\n").unwrap();
        Theme::save_choice_at(&path, "signature").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let t = Theme::parse(&contents).unwrap();
        assert_eq!(t.name, "signature");
        assert_eq!(t.padding, 3);
        assert_eq!(t.foreground, [18, 52, 86]);
        assert_ne!(t.background, Theme::builtin("minimal").unwrap().background);
        assert_eq!(Theme::parse("padding=2").unwrap().name, "minimal");
    }
    #[test]
    fn parses_and_rejects_invalid() {
        let t = Theme::parse(
            "padding=5\nfont_size=20.0\nforeground='#123456'\nfont_family='DejaVu Sans Mono'",
        )
        .unwrap();
        assert_eq!(t.foreground, [18, 52, 86]);
        assert_eq!(t.padding, 5);
        assert_eq!(t.font_size, 20.0);
        assert!(
            Theme::parse("foreground='oops'")
                .unwrap_err()
                .contains("foreground")
        );
        let colors = (0..16)
            .map(|i| format!("'#0000{i:02x}'"))
            .collect::<Vec<_>>()
            .join(",");
        let palette = Theme::parse(&format!("ansi=[{colors}]")).unwrap();
        assert_eq!(palette.ansi[15], [0, 0, 15]);
        for v in [
            "font_size=0",
            "padding=300",
            "cursor='bad'",
            "ansi=['#000000']",
            "font_family=''",
            "cursor_shape='triangle'",
            "scrollback_limit=100001",
            "scroll_speed=0",
        ] {
            assert!(Theme::parse(v).is_err(), "{v}");
        }
    }
}
