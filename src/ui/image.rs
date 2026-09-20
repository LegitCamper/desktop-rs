use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use image::ImageReader;
use system_tray::item::IconPixmap;

/// Premultiplied BGRA pixels matching Wayland `Argb8888` memory layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    name: String,
    theme_path: Option<String>,
    icon_theme: Option<String>,
    size: u32,
    scale: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PixmapKey {
    pixmap: IconPixmap,
    size: u32,
    scale: u32,
}

/// Named-icon, PNG, SVG, and StatusNotifier pixmap cache.
#[derive(Default)]
pub struct ImageCache {
    images: HashMap<CacheKey, Option<Image>>,
    pixmaps: HashMap<PixmapKey, Option<Image>>,
}

impl ImageCache {
    pub fn named(
        &mut self,
        name: &str,
        theme_path: Option<&str>,
        icon_theme: Option<&str>,
        size: u32,
        scale: u32,
    ) -> Option<&Image> {
        let key = CacheKey {
            name: name.to_owned(),
            theme_path: theme_path.map(str::to_owned),
            icon_theme: icon_theme.map(str::to_owned),
            size,
            scale,
        };
        self.images.entry(key.clone()).or_insert_with(|| {
            resolve_named(name, theme_path, icon_theme, size, scale)
                .and_then(|path| decode_path(&path, size.saturating_mul(scale).max(1)).ok())
        });
        self.images.get(&key).and_then(Option::as_ref)
    }

    pub fn pixmap(&mut self, pixmaps: &[IconPixmap], size: u32, scale: u32) -> Option<&Image> {
        let target = size.saturating_mul(scale).max(1);
        let pixmap = closest_pixmap(pixmaps, target)?.clone();
        let key = PixmapKey {
            pixmap,
            size,
            scale,
        };
        self.pixmaps.entry(key.clone()).or_insert_with(|| {
            decode_pixmap(&key.pixmap)
                .ok()
                .map(|image| fit(image, target))
        });
        self.pixmaps.get(&key).and_then(Option::as_ref)
    }
}

fn resolve_named(
    name: &str,
    theme_path: Option<&str>,
    icon_theme: Option<&str>,
    size: u32,
    scale: u32,
) -> Option<PathBuf> {
    let explicit = Path::new(name);
    if explicit.is_absolute() && explicit.is_file() {
        return Some(explicit.to_owned());
    }
    if let Some(path) = theme_path {
        for extension in ["png", "svg"] {
            let candidate = Path::new(path).join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let lookup = freedesktop_icons::lookup(name)
        .with_size(u16::try_from(size).unwrap_or(u16::MAX))
        .with_scale(u16::try_from(scale).unwrap_or(u16::MAX));
    match icon_theme {
        Some(icon_theme) => lookup.with_theme(icon_theme).with_cache().find(),
        None => lookup.with_cache().find(),
    }
}

fn decode_path(path: &Path, target: u32) -> Result<Image> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => decode_raster(path, target),
        Some("svg") => decode_svg(path, target),
        _ => decode_raster(path, target)
            .or_else(|_| decode_svg(path, target))
            .with_context(|| format!("unsupported icon format `{}`", path.display())),
    }
}

fn decode_raster(path: &Path, target: u32) -> Result<Image> {
    let image = ImageReader::open(path)
        .with_context(|| format!("open icon `{}`", path.display()))?
        .with_guessed_format()
        .context("detect icon format")?
        .decode()
        .with_context(|| format!("decode icon `{}`", path.display()))?
        .resize(target, target, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    rgba_to_bgra(image.width(), image.height(), image.into_raw())
}

fn decode_svg(path: &Path, target: u32) -> Result<Image> {
    let data = std::fs::read(path).with_context(|| format!("read icon `{}`", path.display()))?;
    let options = resvg::usvg::Options {
        resources_dir: path.parent().map(Path::to_owned),
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(&data, &options).context("parse SVG icon")?;
    let source = tree.size();
    let scale = (target as f32 / source.width()).min(target as f32 / source.height());
    let x = (target as f32 - source.width() * scale) / 2.0;
    let y = (target as f32 - source.height() * scale) / 2.0;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(target, target)
        .ok_or_else(|| anyhow!("icon dimensions overflow"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(x, y),
        &mut pixmap.as_mut(),
    );
    rgba_to_bgra(target, target, pixmap.take())
}

fn rgba_to_bgra(width: u32, height: u32, mut pixels: Vec<u8>) -> Result<Image> {
    validate_pixels(width, height, &pixels)?;
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        let red = ((u16::from(pixel[0]) * alpha + 127) / 255) as u8;
        let green = ((u16::from(pixel[1]) * alpha + 127) / 255) as u8;
        let blue = ((u16::from(pixel[2]) * alpha + 127) / 255) as u8;
        pixel.copy_from_slice(&[blue, green, red, pixel[3]]);
    }
    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Decodes network-order ARGB into premultiplied Wayland BGRA bytes.
pub fn decode_pixmap(pixmap: &IconPixmap) -> Result<Image> {
    let width = u32::try_from(pixmap.width).context("negative pixmap width")?;
    let height = u32::try_from(pixmap.height).context("negative pixmap height")?;
    validate_pixels(width, height, &pixmap.pixels)?;
    let mut pixels = Vec::with_capacity(pixmap.pixels.len());
    for pixel in pixmap.pixels.chunks_exact(4) {
        let alpha = u16::from(pixel[0]);
        let red = ((u16::from(pixel[1]) * alpha + 127) / 255) as u8;
        let green = ((u16::from(pixel[2]) * alpha + 127) / 255) as u8;
        let blue = ((u16::from(pixel[3]) * alpha + 127) / 255) as u8;
        pixels.extend_from_slice(&[blue, green, red, pixel[0]]);
    }
    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Scales already-premultiplied BGRA down to `target` when a pixmap
/// arrives larger than the widget box. Tray icons routinely ship 256px.
/// Triangle filtering avoids the ringing overshoot that would push a
/// channel above its own alpha in premultiplied space.
fn fit(image: Image, target: u32) -> Image {
    let longest = image.width.max(image.height);
    if longest <= target || longest == 0 {
        return image;
    }
    let scale = f64::from(target) / f64::from(longest);
    let width = ((f64::from(image.width) * scale).round() as u32).max(1);
    let height = ((f64::from(image.height) * scale).round() as u32).max(1);
    let Some(buffer) = image::RgbaImage::from_raw(image.width, image.height, image.pixels.clone())
    else {
        return image;
    };
    let scaled = image::imageops::resize(
        &buffer,
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    Image {
        width: scaled.width(),
        height: scaled.height(),
        pixels: scaled.into_raw(),
    }
}

fn validate_pixels(width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .context("icon dimensions overflow")?;
    if pixels.len() != expected {
        bail!("expected {expected} icon bytes, found {}", pixels.len());
    }
    Ok(())
}

fn closest_pixmap(pixmaps: &[IconPixmap], target: u32) -> Option<&IconPixmap> {
    pixmaps
        .iter()
        .filter(|pixmap| pixmap.width > 0 && pixmap.height > 0)
        .min_by_key(|pixmap| {
            let size = u32::try_from(pixmap.width.max(pixmap.height)).unwrap_or(u32::MAX);
            (size < target, size.abs_diff(target))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_tray_pixmap_should_shrink_to_icon_box() {
        let pixmap = IconPixmap {
            width: 256,
            height: 256,
            pixels: vec![0xff; 256 * 256 * 4],
        };
        let mut cache = ImageCache::default();

        let size = cache
            .pixmap(std::slice::from_ref(&pixmap), 20, 1)
            .map(|image| (image.width, image.height));

        assert_eq!(size, Some((20, 20)));
    }

    #[test]
    fn small_pixmap_should_not_be_upscaled() {
        let image = fit(
            Image {
                width: 16,
                height: 16,
                pixels: vec![0; 16 * 16 * 4],
            },
            64,
        );

        assert_eq!((image.width, image.height), (16, 16));
    }

    #[test]
    fn fit_should_preserve_aspect_ratio() {
        let image = fit(
            Image {
                width: 128,
                height: 64,
                pixels: vec![0; 128 * 64 * 4],
            },
            32,
        );

        assert_eq!((image.width, image.height), (32, 16));
    }

    #[test]
    fn decode_pixmap_should_premultiply_network_argb_to_wayland_bgra() -> Result<()> {
        let image = decode_pixmap(&IconPixmap {
            width: 1,
            height: 1,
            pixels: vec![0x80, 0xff, 0x80, 0x40],
        })?;

        assert_eq!(image.pixels, [0x20, 0x40, 0x80, 0x80]);
        Ok(())
    }

    #[test]
    fn decode_pixmap_should_reject_bad_byte_count() {
        let result = decode_pixmap(&IconPixmap {
            width: 2,
            height: 1,
            pixels: vec![0; 4],
        });

        assert!(result.is_err());
    }

    #[test]
    fn image_cache_should_reuse_decoded_pixmaps() {
        let pixmap = IconPixmap {
            width: 1,
            height: 1,
            pixels: vec![0xff, 0x10, 0x20, 0x30],
        };
        let mut cache = ImageCache::default();
        assert!(cache.pixmap(std::slice::from_ref(&pixmap), 1, 1).is_some());

        let key = PixmapKey {
            pixmap,
            size: 1,
            scale: 1,
        };
        assert_eq!(cache.pixmaps.len(), 1);
        assert!(cache.pixmaps.contains_key(&key));
    }

    #[test]
    fn decode_path_should_sniff_extensionless_raster() -> Result<()> {
        let target = std::env::temp_dir().join(format!(
            "desktop-rs-extensionless-icon-{}",
            std::process::id()
        ));
        let source = target.with_extension("png");
        image::RgbaImage::from_raw(1, 1, vec![0xff, 0, 0, 0xff])
            .context("create PNG fixture")?
            .save_with_format(&source, image::ImageFormat::Png)?;
        std::fs::rename(source, &target)?;

        let image = decode_path(&target, 1)?;
        std::fs::remove_file(target)?;

        assert_eq!((image.width, image.height), (1, 1));
        Ok(())
    }

    #[test]
    fn closest_pixmap_should_prefer_nearest_non_smaller_size() {
        let pixmaps = [
            IconPixmap {
                width: 16,
                height: 16,
                pixels: vec![0; 16 * 16 * 4],
            },
            IconPixmap {
                width: 32,
                height: 32,
                pixels: vec![0; 32 * 32 * 4],
            },
            IconPixmap {
                width: 64,
                height: 64,
                pixels: vec![0; 64 * 64 * 4],
            },
        ];

        assert_eq!(
            closest_pixmap(&pixmaps, 24).map(|pixmap| pixmap.width),
            Some(32)
        );
    }
}
