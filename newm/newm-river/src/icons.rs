//! App icons for the switcher: the desktop entry an app ID names, its `Icon=`, and a PNG of that
//! icon from the icon theme (KDE's in `kdeglobals`, then breeze and hicolor) or `pixmaps`.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::draw::Pixmap;

/// Sizes looked for, the most fitting first.
const SIZES: [&str; 7] = [
    "64x64", "48x48", "96x96", "128x128", "256x256", "32x32", "512x512",
];

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`.
pub fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
        .into_iter()
        .collect();
    let sys =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(sys.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs
}

/// The value of `key` in a desktop entry's `[Desktop Entry]` group.
pub fn entry_value(text: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// The desktop file names an app ID may have: itself, lowercased, and its last dotted part.
pub fn entry_names(app_id: &str) -> Vec<String> {
    let mut names = vec![app_id.to_owned(), app_id.to_lowercase()];
    if let Some(last) = app_id.rsplit('.').next()
        && last != app_id
    {
        names.push(last.to_owned());
        names.push(last.to_lowercase());
    }
    names.dedup();
    names
}

/// The `Icon=` of the desktop entry for an app ID: by file name, else by `StartupWMClass`.
pub fn icon_name(app_id: &str, dirs: &[PathBuf]) -> Option<String> {
    let apps: Vec<PathBuf> = dirs.iter().map(|d| d.join("applications")).collect();
    for name in entry_names(app_id) {
        for dir in &apps {
            if let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.desktop"))) {
                return Some(entry_value(&text, "Icon").unwrap_or(name));
            }
        }
    }
    for dir in &apps {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if entry_value(&text, "StartupWMClass").is_some_and(|c| c.eq_ignore_ascii_case(app_id))
            {
                return entry_value(&text, "Icon");
            }
        }
    }
    None
}

/// The icon theme Plasma uses, from `kdeglobals`.
fn kde_theme() -> Option<String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let text = std::fs::read_to_string(base.join("kdeglobals")).ok()?;
    let mut in_icons = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_icons = line == "[Icons]";
        } else if in_icons && let Some(v) = line.strip_prefix("Theme=") {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// A PNG file for an icon name: a path as is, else the themes' `apps` folders, else `pixmaps`.
pub fn icon_file(icon: &str, dirs: &[PathBuf], theme: Option<&str>) -> Option<PathBuf> {
    let path = Path::new(icon);
    if path.is_absolute() {
        return (path.extension().is_some_and(|x| x == "png") && path.is_file())
            .then(|| path.to_owned());
    }
    let mut themes: Vec<&str> = theme.into_iter().collect();
    themes.extend(["breeze", "hicolor"]);
    for theme in themes {
        for size in SIZES {
            for dir in dirs {
                let f = dir
                    .join("icons")
                    .join(theme)
                    .join(size)
                    .join("apps")
                    .join(format!("{icon}.png"));
                if f.is_file() {
                    return Some(f);
                }
            }
        }
    }
    dirs.iter()
        .map(|d| d.join("pixmaps").join(format!("{icon}.png")))
        .find(|f| f.is_file())
}

/// Decodes a PNG into a premultiplied pixmap.
pub fn load_png(path: &Path) -> Result<Pixmap> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let len = reader.output_buffer_size().context("image too large")?;
    let mut buf = vec![0; len];
    let info = reader.next_frame(&mut buf)?;
    let pixels = (info.width * info.height) as usize;
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..pixels * 4].to_vec(),
        png::ColorType::Rgb => buf[..pixels * 3]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..pixels * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf[..pixels].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("indexed image was not expanded"),
    };
    Ok(Pixmap::from_straight_rgba(info.width, info.height, &rgba))
}

/// The app's icon, scaled to fit `side`, if one is found.
pub fn app_icon(app_id: &str, side: i32) -> Option<Pixmap> {
    let dirs = data_dirs();
    let name = icon_name(app_id, &dirs)?;
    let theme = kde_theme();
    let file = icon_file(&name, &dirs, theme.as_deref())?;
    load_png(&file)
        .inspect_err(|e| tracing::debug!("icon {}: {e:#}", file.display()))
        .ok()
        .map(|p| p.scaled_to_fit((side, side)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("newm-river-icons-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn png(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = File::create(path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[255, 0, 0, 255, 0, 0, 255, 128])
            .unwrap();
    }

    #[test]
    fn desktop_entries_give_their_icon() {
        let text = "[Desktop Entry]\nName=Foot\nIcon = foot\n[Desktop Action new]\nIcon=x\n";
        assert_eq!(entry_value(text, "Icon").as_deref(), Some("foot"));
        assert_eq!(entry_value("[Other]\nIcon=y\n", "Icon"), None);
        assert_eq!(
            entry_names("org.kde.Dolphin"),
            vec!["org.kde.Dolphin", "org.kde.dolphin", "Dolphin", "dolphin"]
        );
    }

    #[test]
    fn an_app_id_finds_its_png_by_name_or_wm_class() {
        let d = tmp("lookup");
        let apps = d.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("foot.desktop"), "[Desktop Entry]\nIcon=foot\n").unwrap();
        std::fs::write(
            apps.join("other.desktop"),
            "[Desktop Entry]\nStartupWMClass=Chromium-Browser\nIcon=chromium\n",
        )
        .unwrap();
        png(&d.join("icons/hicolor/48x48/apps/foot.png"));
        png(&d.join("pixmaps/chromium.png"));
        let dirs = vec![d.clone()];
        assert_eq!(icon_name("foot", &dirs).as_deref(), Some("foot"));
        assert_eq!(
            icon_name("chromium-browser", &dirs).as_deref(),
            Some("chromium")
        );
        assert_eq!(icon_name("nothing", &dirs), None);
        let f = icon_file("foot", &dirs, None).unwrap();
        assert!(f.ends_with("icons/hicolor/48x48/apps/foot.png"));
        assert!(icon_file("chromium", &dirs, Some("breeze")).is_some());
        let p = load_png(&f).unwrap();
        assert_eq!((p.width, p.height), (2, 1));
        assert_eq!(p.px[0], 0xffff0000);
        assert_eq!(p.px[1], 0x80000080);
        let _ = std::fs::remove_dir_all(&d);
    }
}
