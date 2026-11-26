use std::{collections::HashMap, fs, path::PathBuf};

use smithay::input::pointer::CursorIcon;
use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

// Fallback cursor bitmap from Smithay's `anvil` (`anvil/resources/cursor.rgba`, MIT).
static FALLBACK_CURSOR: &[u8] = include_bytes!("../resources/cursor.rgba");

pub struct Cursor {
    theme: CursorTheme,
    size: u32,
    cache: HashMap<CursorIcon, Vec<Image>>,
}

impl Cursor {
    pub fn load(theme: &str, size: u32) -> Self {
        let theme = if theme.is_empty() {
            std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into())
        } else {
            theme.to_string()
        };

        tracing::info!(theme, size, "cursor theme loaded");

        Self {
            theme: CursorTheme::load(&theme),
            size,
            cache: HashMap::new(),
        }
    }

    pub fn image(&mut self, scale: u32) -> &Image {
        self.image_for(CursorIcon::Default, scale)
    }

    pub fn image_for(&mut self, icon: CursorIcon, scale: u32) -> &Image {
        let images = self.cache.entry(icon).or_insert_with(|| {
            let key = icon.name();
            let path = std::iter::once(key)
                .chain(icon.alt_names().iter().copied())
                .find_map(|candidate| self.theme.load_icon(candidate));
            let mut loaded = match path {
                Some(path) => match read_icon(path) {
                    Ok(images) => images,
                    Err(err) => {
                        warn!("failed to parse cursor {key:?}: {err}");
                        Vec::new()
                    }
                },
                None => {
                    warn!("cursor theme has no {key:?} icon, using fallback");
                    Vec::new()
                }
            };
            if loaded.is_empty() {
                loaded.push(fallback());
            }
            loaded
        });

        let wanted = self.size.saturating_mul(scale.max(1));
        images
            .iter()
            .min_by_key(|image| (image.size as i32 - wanted as i32).abs())
            .expect("cursor cache entry is never empty")
    }
}

fn read_icon(path: PathBuf) -> Result<Vec<Image>, std::io::Error> {
    let data = fs::read(path)?;
    parse_xcursor(&data).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "failed to parse xcursor")
    })
}

fn fallback() -> Image {
    Image {
        size: 32,
        width: 64,
        height: 64,
        xhot: 1,
        yhot: 1,
        delay: 1,
        pixels_rgba: FALLBACK_CURSOR.to_vec(),
        pixels_argb: Vec::new(),
    }
}
