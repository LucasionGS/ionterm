//! The background picture (`[window] background_image`): decoded once, kept
//! no bigger than a 4K screen, and handed to the renderer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ionterm_render::Background;

use crate::config::WindowConfig;

/// The image id the renderer caches the picture under; terminal images
/// count up from 1, so they never meet it.
const IMAGE_ID: u32 = u32::MAX;
/// Bigger pictures are scaled down first: no screen shows more.
const MAX_SIZE: (u32, u32) = (3840, 2160);

/// Each load gets a new generation so the renderer uploads it again.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// The configured picture, or why it couldn't be shown.
pub fn load(config: &WindowConfig) -> Result<Option<Background>, String> {
    let path = config.background_image.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let path = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
        None => PathBuf::from(path),
    };
    let picture = image::open(&path).map_err(|error| {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        format!("Background image {name}: {error}")
    })?;
    let picture = if picture.width() > MAX_SIZE.0 || picture.height() > MAX_SIZE.1 {
        picture.thumbnail(MAX_SIZE.0, MAX_SIZE.1)
    } else {
        picture
    };
    let rgba = picture.into_rgba8();
    Ok(Some(Background {
        image: ionterm_vt::Image {
            id: IMAGE_ID,
            number: 0,
            width: rgba.width(),
            height: rgba.height(),
            rgba: Arc::new(rgba.into_raw()),
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
        },
        opacity: config.background_image_opacity,
    }))
}
