use anyhow::Result;

use crate::ui::element::{LayoutItem, Rect};
use crate::ui::image::{Image, ImageCache};
use crate::ui::style::{Color, Style};
use crate::ui::text::TextRenderer;

/// Clears the ARGB8888 canvas and composites `items` in paint order.
pub fn render(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    items: &[LayoutItem],
    text: &mut TextRenderer,
    images: &mut ImageCache,
) -> Result<()> {
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, 0]);
    }
    for item in items {
        fill_rect(canvas, width, height, item.rect, &item.style);
        if let crate::ui::element::Content::Icon {
            name,
            theme_path,
            icon_theme,
            pixmaps,
            size,
            ..
        } = &item.content
        {
            let image = name
                .as_deref()
                .and_then(|name| {
                    images
                        .named(name, theme_path.as_deref(), icon_theme.as_deref(), *size, 1)
                        .cloned()
                })
                .or_else(|| images.pixmap(pixmaps, *size, 1).cloned());
            if let Some(image) = image {
                draw_image(canvas, width, height, item.rect, &image);
                continue;
            }
        }
        text.draw(canvas, width, height, item.rect, &item.content)?;
    }
    Ok(())
}

fn draw_image(canvas: &mut [u8], canvas_width: u32, canvas_height: u32, rect: Rect, image: &Image) {
    let x0 = (rect.x + (rect.width - image.width as f32) / 2.0).round();
    let y0 = (rect.y + (rect.height - image.height as f32) / 2.0).round();
    for y in 0..image.height {
        for x in 0..image.width {
            let (Ok(target_x), Ok(target_y)) = (
                u32::try_from(x0 as i64 + i64::from(x)),
                u32::try_from(y0 as i64 + i64::from(y)),
            ) else {
                continue;
            };
            if target_x >= canvas_width || target_y >= canvas_height {
                continue;
            }
            let source = ((y * image.width + x) * 4) as usize;
            let destination = ((target_y * canvas_width + target_x) * 4) as usize;
            let (Some(source), Some(destination)) = (
                image.pixels.get(source..source + 4),
                canvas.get_mut(destination..destination + 4),
            ) else {
                continue;
            };
            let color = [source[0], source[1], source[2], source[3]];
            destination.copy_from_slice(&over(color, destination));
        }
    }
}

/// Composites `style` inside `rect` over whatever the canvas already holds.
pub fn fill_rect(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    rect: Rect,
    style: &Style,
) {
    let border = style.border_width as f32;
    let radius = style.corner_radius as f32;
    let x0 = rect.x.floor().max(0.0) as u32;
    let y0 = rect.y.floor().max(0.0) as u32;
    let x1 = (rect.x + rect.width).ceil().max(0.0) as u32;
    let y1 = (rect.y + rect.height).ceil().max(0.0) as u32;

    for row in y0..y1.min(canvas_height) {
        for column in x0..x1.min(canvas_width) {
            let x = column as f32 + 0.5 - rect.x;
            let y = row as f32 + 0.5 - rect.y;
            let outer = coverage(rounded_rect_distance(x, y, rect.width, rect.height, radius));
            if outer <= 0.0 {
                continue;
            }
            let inner = coverage(rounded_rect_distance(
                x - border,
                y - border,
                rect.width - border * 2.0,
                rect.height - border * 2.0,
                (radius - border).max(0.0),
            ));
            let start = ((row * canvas_width + column) * 4) as usize;
            let Some(pixel) = canvas.get_mut(start..start + 4) else {
                continue;
            };
            let source = blend(style.background, inner, style.border, outer - inner);
            pixel.copy_from_slice(&over(source, pixel));
        }
    }
}

fn rounded_rect_distance(x: f32, y: f32, w: f32, h: f32, radius: f32) -> f32 {
    let half_w = w / 2.0;
    let half_h = h / 2.0;
    let radius = radius.min(half_w).min(half_h).max(0.0);
    let dx = (x - half_w).abs() - (half_w - radius);
    let dy = (y - half_h).abs() - (half_h - radius);
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - radius
}

fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn blend(
    background: Color,
    background_coverage: f32,
    border: Color,
    border_coverage: f32,
) -> [u8; 4] {
    let bg_alpha = background.alpha as f32 / 255.0 * background_coverage;
    let border_alpha = border.alpha as f32 / 255.0 * border_coverage;
    let channel = |bg: u8, bd: u8| to_u8(bg as f32 * bg_alpha + bd as f32 * border_alpha);

    [
        channel(background.blue, border.blue),
        channel(background.green, border.green),
        channel(background.red, border.red),
        to_u8((bg_alpha + border_alpha) * 255.0),
    ]
}

fn over(source: [u8; 4], destination: &[u8]) -> [u8; 4] {
    let inverse = 1.0 - source[3] as f32 / 255.0;
    let mut out = [0_u8; 4];
    for (index, channel) in out.iter_mut().enumerate() {
        let under = destination.get(index).copied().unwrap_or(0) as f32;
        *channel = to_u8(source[index] as f32 + under * inverse);
    }
    out
}

fn to_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::element::Content;

    const OPAQUE_BLUE: Color = Color::rgba(0x20, 0x60, 0xc0, 0xff);

    fn item(size: u32, style: Style) -> LayoutItem {
        LayoutItem {
            id: None,
            content: Content::Box,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: size as f32,
                height: size as f32,
            },
            style,
        }
    }

    fn pixel_at(canvas: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let start = ((y * width + x) * 4) as usize;
        [
            canvas[start],
            canvas[start + 1],
            canvas[start + 2],
            canvas[start + 3],
        ]
    }

    #[test]
    fn render_should_write_opaque_background_for_square_panel() -> Result<()> {
        let mut canvas = [0_u8; 4 * 4 * 4];

        render(
            &mut canvas,
            4,
            4,
            &[item(4, Style::panel(OPAQUE_BLUE))],
            &mut TextRenderer::new(),
            &mut ImageCache::default(),
        )?;

        assert_eq!(pixel_at(&canvas, 4, 0, 0), [0xc0, 0x60, 0x20, 0xff]);
        Ok(())
    }

    #[test]
    fn render_should_clear_stale_pixels_before_painting() -> Result<()> {
        let mut canvas = [0xff_u8; 32 * 32 * 4];

        render(
            &mut canvas,
            32,
            32,
            &[item(
                32,
                Style::card(OPAQUE_BLUE, Color::rgba(0xff, 0xff, 0xff, 0xff)),
            )],
            &mut TextRenderer::new(),
            &mut ImageCache::default(),
        )?;

        assert_eq!(pixel_at(&canvas, 32, 0, 0), [0, 0, 0, 0]);
        Ok(())
    }

    #[test]
    fn fill_rect_should_leave_pixels_outside_the_rect_untouched() {
        let mut canvas = [0_u8; 8 * 8 * 4];
        let rect = Rect {
            x: 2.0,
            y: 2.0,
            width: 4.0,
            height: 4.0,
        };

        fill_rect(&mut canvas, 8, 8, rect, &Style::panel(OPAQUE_BLUE));

        assert_eq!(pixel_at(&canvas, 8, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&canvas, 8, 3, 3), [0xc0, 0x60, 0x20, 0xff]);
    }
}
