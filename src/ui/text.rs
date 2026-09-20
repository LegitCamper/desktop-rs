use anyhow::{Context, Result};
use cosmic_text::{
    Align as TextAlign, Attrs, Buffer, Color as CosmicColor, Ellipsize, EllipsizeHeightLimit,
    Family, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};

use crate::ui::element::{Align, Content, Rect, Text};
use crate::ui::style::Color;

/// Shared system-font database and glyph cache for one UI process.
pub struct TextRenderer {
    fonts: FontSystem,
    glyphs: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        let mut fonts = FontSystem::new();
        if let Ok(directory) = crate::config::paths::font_dir()
            && directory.is_dir()
        {
            fonts.db_mut().load_fonts_dir(directory);
        }
        Self {
            fonts,
            glyphs: SwashCache::new(),
        }
    }

    /// Natural single-line size used by `Size::Fit`.
    pub fn measure(&mut self, content: &Content) -> (u32, u32) {
        let Some(text) = resolved_text(content).ok().flatten() else {
            return (0, 0);
        };
        let mut buffer = self.buffer(&text, None, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max)
            .ceil() as u32;
        (width, line_height(text.font_size).ceil() as u32)
    }

    pub fn draw(
        &mut self,
        canvas: &mut [u8],
        canvas_width: u32,
        canvas_height: u32,
        rect: Rect,
        content: &Content,
    ) -> Result<()> {
        let Some(text) = resolved_text(content)? else {
            return Ok(());
        };
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return Ok(());
        }
        let mut buffer = self.buffer(&text, Some(rect.width), Some(rect.height));
        let x_origin = rect.x.floor() as i32;
        let y_origin = rect.y.floor() as i32;
        buffer.draw(
            &mut self.fonts,
            &mut self.glyphs,
            CosmicColor::rgba(
                text.color.red,
                text.color.green,
                text.color.blue,
                text.color.alpha,
            ),
            |x, y, width, height, color| {
                for local_y in 0..height {
                    for local_x in 0..width {
                        composite(
                            canvas,
                            canvas_width,
                            canvas_height,
                            x_origin + x + local_x as i32,
                            y_origin + y + local_y as i32,
                            color,
                        );
                    }
                }
            },
        );
        Ok(())
    }

    fn buffer(&mut self, text: &Text, width: Option<f32>, height: Option<f32>) -> Buffer {
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(text.font_size as f32, line_height(text.font_size)),
        );
        buffer.set_size(width, height);
        buffer.set_wrap(Wrap::None);
        buffer.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
        let family = if text.font_family.is_empty() {
            Family::SansSerif
        } else {
            Family::Name(&text.font_family)
        };
        buffer.set_text(
            &text.value,
            &Attrs::new().family(family),
            Shaping::Advanced,
            Some(match text.align {
                Align::Start => TextAlign::Left,
                Align::Center => TextAlign::Center,
                Align::End => TextAlign::Right,
            }),
        );
        buffer
    }
}

fn resolved_text(content: &Content) -> Result<Option<Text>> {
    match content {
        Content::Icon { fallback, .. } => Ok(Some(fallback.clone())),
        Content::Box
        | Content::AppSearch { .. }
        | Content::AppList { .. }
        | Content::ActiveWindow { .. }
        | Content::Workspaces { .. }
        | Content::Audio { .. }
        | Content::Battery { .. }
        | Content::Backlight { .. }
        | Content::Network { .. }
        | Content::Bluetooth { .. }
        | Content::Custom { .. }
        | Content::Tray { .. } => Ok(None),
        Content::Text(text) => Ok(Some(text.clone())),
        Content::Clock { text, format } => {
            let mut current = text.clone();
            current.value = jiff::fmt::strtime::format(format, &jiff::Zoned::now())
                .with_context(|| format!("format clock with `{format}`"))?;
            Ok(Some(current))
        }
    }
}

pub fn validate_clock_format(format: &str) -> Result<()> {
    jiff::fmt::strtime::format(format, &jiff::Zoned::now())
        .map(drop)
        .with_context(|| format!("invalid clock format `{format}`"))
}

/// Text row height for a font size; matches the renderer's line metrics.
pub const fn line_height(font_size: u32) -> f32 {
    font_size as f32 * 1.25
}

fn composite(canvas: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: CosmicColor) {
    let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
        return;
    };
    if x >= width || y >= height {
        return;
    }
    let start = ((y * width + x) * 4) as usize;
    let Some(destination) = canvas.get_mut(start..start + 4) else {
        return;
    };
    let (red, green, blue, alpha) = color.as_rgba_tuple();
    let source = premultiply(Color::rgba(red, green, blue, alpha));
    let inverse = u16::from(255 - source[3]);
    for index in 0..3 {
        destination[index] = (u16::from(source[index])
            + (u16::from(destination[index]) * inverse + 127) / 255)
            .min(255) as u8;
    }
    destination[3] =
        (u16::from(source[3]) + (u16::from(destination[3]) * inverse + 127) / 255).min(255) as u8;
}

fn premultiply(color: Color) -> [u8; 4] {
    let alpha = u16::from(color.alpha);
    let channel = |value: u8| ((u16::from(value) * alpha + 127) / 255) as u8;
    [
        channel(color.blue),
        channel(color.green),
        channel(color.red),
        color.alpha,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_clock_format_should_reject_incomplete_directive() {
        assert!(validate_clock_format("%Y %").is_err());
    }

    #[test]
    fn premultiply_should_emit_wayland_bgra_bytes() {
        assert_eq!(
            premultiply(Color::rgba(0xff, 0x80, 0x40, 0x80)),
            [0x20, 0x40, 0x80, 0x80]
        );
    }

    #[test]
    fn renderer_should_measure_and_draw_utf8() -> Result<()> {
        let text = Text {
            value: "Clock 世界".to_owned(),
            color: Color::rgba(0xff, 0xff, 0xff, 0xff),
            font_size: 16,
            font_family: String::new(),
            align: Align::Start,
        };
        let mut renderer = TextRenderer::new();
        let size = renderer.measure(&Content::Text(text.clone()));
        let mut canvas = vec![0_u8; 240 * 40 * 4];

        renderer.draw(
            &mut canvas,
            240,
            40,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 240.0,
                height: 40.0,
            },
            &Content::Text(text),
        )?;

        assert!(size.0 > 0);
        assert!(canvas.iter().any(|byte| *byte != 0));
        Ok(())
    }
}
