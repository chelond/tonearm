//! Colour themes: a few built in, plus `.toml` files the user drops into the themes folder
//! (`~/.config/tonearm/themes`, `%APPDATA%\tonearm\config\themes`). A theme file names only the
//! colours it changes; the rest come from its base ("dark" or "light"). The active custom theme
//! is re-read when its file changes, so editing it shows up right away.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

/// Built-in themes, in the order the settings page shows them. The first one is the default.
const BUILTIN: [(&str, &str); 8] = [
    ("graphite", include_str!("../themes/graphite.toml")),
    ("midnight", include_str!("../themes/midnight.toml")),
    ("oled", include_str!("../themes/oled.toml")),
    ("forest", include_str!("../themes/forest.toml")),
    ("sakura", include_str!("../themes/sakura.toml")),
    ("synthwave", include_str!("../themes/synthwave.toml")),
    ("light", include_str!("../themes/light.toml")),
    ("sand", include_str!("../themes/sand.toml")),
];

#[cfg(test)]
const DEFAULT_ID: &str = "graphite";

/// Put next to the user's themes the first time the folder is opened.
pub const README: &str = "Темы Tonearm\r\n\r\n\
Каждый файл .toml в этой папке — тема. Она появляется в Настройках → Оформление.\r\n\
Проще всего начать с кнопки «Создать из текущей»: она кладёт сюда копию выбранной темы.\r\n\
Откройте файл в Блокноте, поменяйте цвета и сохраните: приложение обновится само.\r\n\r\n\
Цвета пишутся как \"#RRGGBB\" или \"#RRGGBBAA\" (последняя пара — прозрачность).\r\n\
Можно указать только те цвета, которые хотите поменять: остальные возьмутся из base\r\n\
(\"dark\" или \"light\"). Чужой темой можно поделиться, просто передав файл.\r\n";
/// User themes are listed with this prefix in front of the file name.
const USER_PREFIX: &str = "file:";
/// More files than this in the folder are ignored.
const MAX_USER_THEMES: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024;

pub type Rgba = [u8; 4];

/// Colour names, as written in theme files and in the same order as [`Palette::get`].
pub const KEYS: [&str; 15] = [
    "bg",
    "sidebar",
    "surface",
    "surface-2",
    "hover",
    "pressed",
    "selected",
    "line",
    "line-2",
    "text-1",
    "text-2",
    "text-3",
    "danger",
    "accent",
    "on-accent",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette(pub [Rgba; 15]);

impl Palette {
    #[must_use]
    pub fn get(&self, key: &str) -> Rgba {
        KEYS.iter()
            .position(|k| *k == key)
            .map_or([0, 0, 0, 255], |i| self.0[i])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDef {
    /// `graphite`… for built-in ones, `file:<name>.toml` for the user's.
    pub id: String,
    pub name: String,
    pub light: bool,
    pub accent_from_cover: bool,
    pub colors: Palette,
}

impl ThemeDef {
    #[must_use]
    pub fn is_custom(&self) -> bool {
        self.id.starts_with(USER_PREFIX)
    }
}

#[derive(Deserialize)]
struct ThemeFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    base: String,
    accent_from_cover: Option<bool>,
    #[serde(default)]
    colors: std::collections::HashMap<String, String>,
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA`.
#[must_use]
pub fn parse_color(s: &str) -> Option<Rgba> {
    let hex = s.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| u8::from_str_radix(&hex[i..=i], 16).ok().map(|v| v * 17);
            Some([d(0)?, d(1)?, d(2)?, 255])
        }
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

#[must_use]
pub fn format_color(c: Rgba) -> String {
    if c[3] == 255 {
        format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", c[0], c[1], c[2], c[3])
    }
}

/// Parses a theme file on top of `base` (the built-in dark or light palette).
fn parse(id: &str, text: &str, dark: &ThemeDef, light: &ThemeDef) -> Result<ThemeDef, String> {
    let file: ThemeFile = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    let is_light = file.base.eq_ignore_ascii_case("light");
    let base = if is_light { light } else { dark };
    let mut colors = base.colors;
    for (key, value) in &file.colors {
        let Some(i) = KEYS.iter().position(|k| k == key) else {
            continue; // unknown names are ignored, so files from newer versions still load
        };
        colors.0[i] = parse_color(value).ok_or_else(|| format!("{key}: «{value}» не цвет"))?;
    }
    Ok(ThemeDef {
        id: id.to_owned(),
        name: if file.name.trim().is_empty() {
            id.trim_start_matches(USER_PREFIX)
                .trim_end_matches(".toml")
                .to_owned()
        } else {
            file.name.trim().chars().take(40).collect()
        },
        light: is_light,
        accent_from_cover: file.accent_from_cover.unwrap_or(base.accent_from_cover),
        colors,
    })
}

/// The two palettes every theme builds on: "Графит" (dark) and "Светлая" (light).
fn bases() -> (ThemeDef, ThemeDef) {
    let raw = |text: &str, light: bool| {
        let file: ThemeFile = toml::from_str(text).expect("built-in theme");
        let mut colors = Palette([[0, 0, 0, 255]; 15]);
        for (i, key) in KEYS.iter().enumerate() {
            if let Some(c) = file.colors.get(*key).and_then(|v| parse_color(v)) {
                colors.0[i] = c;
            }
        }
        ThemeDef {
            id: String::new(),
            name: file.name,
            light,
            accent_from_cover: file.accent_from_cover.unwrap_or(true),
            colors,
        }
    };
    (
        raw(BUILTIN[0].1, false),
        raw(include_str!("../themes/light.toml"), true),
    )
}

#[must_use]
pub fn dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm").map(|d| d.config_dir().join("themes"))
}

/// What the settings page lists.
pub struct Catalog {
    pub themes: Vec<ThemeDef>,
    /// Files that did not load, with the reason (shown on the settings page).
    pub errors: Vec<String>,
}

impl Catalog {
    /// Built-in themes and the ones in `dir`.
    #[must_use]
    pub fn load(dir: Option<&Path>) -> Self {
        let (dark, light) = bases();
        let mut themes: Vec<ThemeDef> = BUILTIN
            .iter()
            .map(|(id, text)| parse(id, text, &dark, &light).expect("built-in theme"))
            .collect();
        let mut errors = Vec::new();
        let mut files: Vec<PathBuf> = dir
            .and_then(|d| std::fs::read_dir(d).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
            })
            .collect();
        files.sort();
        for path in files.into_iter().take(MAX_USER_THEMES) {
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let id = format!("{USER_PREFIX}{file_name}");
            match read_small(&path).and_then(|t| parse(&id, &t, &dark, &light)) {
                Ok(t) => themes.push(t),
                Err(e) => errors.push(format!("{file_name}: {e}")),
            }
        }
        Self { themes, errors }
    }

    /// The theme with this id, or the default one.
    #[must_use]
    pub fn get(&self, id: &str) -> &ThemeDef {
        self.themes
            .iter()
            .find(|t| t.id == id)
            .unwrap_or(&self.themes[0])
    }
}

fn read_small(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("файл слишком большой".into());
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

/// Path of a user theme by its id.
#[must_use]
pub fn file_of(id: &str) -> Option<PathBuf> {
    let name = id.strip_prefix(USER_PREFIX)?;
    // Only plain file names: an id from a hand-edited config must not point elsewhere.
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    Some(dir()?.join(name))
}

/// Last change of the active user theme file, to reload it when it is edited.
#[must_use]
pub fn modified(id: &str) -> Option<SystemTime> {
    std::fs::metadata(file_of(id)?).ok()?.modified().ok()
}

/// The theme as a file the user can edit: every colour written out, with comments.
#[must_use]
pub fn to_file(theme: &ThemeDef, name: &str) -> String {
    let mut out = String::from(
        "# Tonearm theme: change the colours, save, and the app updates right away.\n\
         # Colours are \"#RRGGBB\" or \"#RRGGBBAA\" (the last pair is transparency).\n",
    );
    out.push_str(&format!("name = {}\n", toml_string(name)));
    out.push_str(&format!(
        "base = \"{}\"\n",
        if theme.light { "light" } else { "dark" }
    ));
    out.push_str("# true: the accent follows the playing cover; false: always `accent`.\n");
    out.push_str(&format!(
        "accent_from_cover = {}\n\n[colors]\n",
        theme.accent_from_cover
    ));
    for (i, key) in KEYS.iter().enumerate() {
        out.push_str(&format!(
            "{key} = \"{}\"\n",
            format_color(theme.colors.0[i])
        ));
    }
    out
}

fn toml_string(s: &str) -> String {
    let escaped: String = s
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Writes a copy of `theme` into the themes folder under a free name; returns its id.
pub fn save_copy(theme: &ThemeDef) -> Result<String, String> {
    let dir = dir().ok_or("нет папки настроек")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = if theme.is_custom() {
        theme.name.clone()
    } else {
        format!("{} (моя)", theme.name)
    };
    for n in 1..100 {
        let file_name = if n == 1 {
            "my-theme.toml".to_owned()
        } else {
            format!("my-theme-{n}.toml")
        };
        let path = dir.join(&file_name);
        if path.exists() {
            continue;
        }
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base} {n}")
        };
        std::fs::write(&path, to_file(theme, &name)).map_err(|e| e.to_string())?;
        return Ok(format!("{USER_PREFIX}{file_name}"));
    }
    Err("слишком много своих тем".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some([255, 255, 255, 255]));
        assert_eq!(parse_color("#0E0E11"), Some([14, 14, 17, 255]));
        assert_eq!(parse_color("#FFFFFF0D"), Some([255, 255, 255, 13]));
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(format_color([14, 14, 17, 255]), "#0E0E11");
        assert_eq!(format_color([255, 255, 255, 13]), "#FFFFFF0D");
    }

    #[test]
    fn builtins_load_and_default_comes_first() {
        let c = Catalog::load(None);
        assert_eq!(c.themes.len(), BUILTIN.len());
        assert!(c.errors.is_empty());
        assert_eq!(c.get("nope").id, DEFAULT_ID);
        assert!(c.get("light").light);
        assert_eq!(c.get("oled").colors.get("bg"), [0, 0, 0, 255]);
        // OLED names only a few colours; the rest come from the dark base.
        assert_eq!(
            c.get("oled").colors.get("text-2"),
            c.get("graphite").colors.get("text-2")
        );
    }

    #[test]
    fn user_files_fill_from_base_and_report_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            "name = \"Мой\"\nbase = \"light\"\n[colors]\naccent = \"#123456\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.toml"), "[colors]\nbg = \"blue\"\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "x").unwrap();
        let c = Catalog::load(Some(dir.path()));
        let mine = c.get("file:a.toml");
        assert_eq!(mine.name, "Мой");
        assert!(mine.light);
        assert_eq!(mine.colors.get("accent"), [0x12, 0x34, 0x56, 255]);
        assert_eq!(mine.colors.get("bg"), c.get("light").colors.get("bg"));
        assert_eq!(c.errors.len(), 1);
        assert!(c.errors[0].starts_with("b.toml"));
    }

    #[test]
    fn written_file_reads_back_the_same() {
        let c = Catalog::load(None);
        let t = c.get("synthwave");
        let text = to_file(t, "Копия \"1\"");
        let (dark, light) = bases();
        let back = parse("file:x.toml", &text, &dark, &light).unwrap();
        assert_eq!(back.colors, t.colors);
        assert_eq!(back.name, "Копия \"1\"");
        assert_eq!(back.accent_from_cover, t.accent_from_cover);
    }

    #[test]
    fn ids_cannot_escape_the_folder() {
        assert!(file_of("file:../x.toml").is_none());
        assert!(file_of("file:a/b.toml").is_none());
        assert!(file_of("graphite").is_none());
    }
}
