/// Straight-alpha color, converted to premultiplied ARGB when painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

/// Visual treatment of a single surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub background: Color,
    pub border: Color,
    pub border_width: u32,
    pub corner_radius: u32,
}

impl Style {
    /// Flat panel: opaque, square, no border.
    pub const fn panel(background: Color) -> Self {
        Self {
            background,
            border: Color::TRANSPARENT,
            border_width: 0,
            corner_radius: 0,
        }
    }

    /// Floating card: rounded, hairline border.
    pub const fn card(background: Color, border: Color) -> Self {
        Self {
            background,
            border,
            border_width: 1,
            corner_radius: 12,
        }
    }
}
