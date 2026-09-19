use anyhow::{Context, Result};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

use crate::ui::animation::Animation;
use crate::ui::element::Element;
use crate::ui::style::{Color, Style};

/// Edge offsets in surface-local pixels, in the Wayland argument order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl Margin {
    pub const fn uniform(value: i32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// Everything needed to map one layer surface, independent of Wayland state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub namespace: &'static str,
    pub layer: Layer,
    pub anchor: Anchor,
    pub width: u32,
    pub height: u32,
    pub margin: Margin,
    /// Screen space reserved from other windows. `-1` opts out of reservation.
    pub exclusive_zone: i32,
    pub keyboard_interactivity: KeyboardInteractivity,
    /// Widget tree painted into the surface; its own style is the background.
    pub root: Element,
    pub animation: Option<Animation>,
}

const INK: Color = Color::rgba(0x12, 0x16, 0x1c, 0xf2);
const SURFACE: Color = Color::rgba(0x1b, 0x21, 0x2b, 0xf7);
const EDGE: Color = Color::rgba(0x5c, 0x6a, 0x82, 0x9c);

impl Window {
    /// Top bar spanning the output, reserving its own height.
    pub fn bar() -> Self {
        let height = 36;
        Self {
            namespace: "desktop-rs-bar",
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            width: 0,
            height,
            margin: Margin::default(),
            exclusive_zone: height as i32,
            keyboard_interactivity: KeyboardInteractivity::None,
            root: Element::new(Style::panel(INK)),
            animation: None,
        }
    }

    /// Notification popup in the top-right corner, over other windows.
    pub fn notification() -> Self {
        Self {
            namespace: "desktop-rs-notification",
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::RIGHT,
            width: 380,
            height: 96,
            margin: Margin {
                top: 48,
                right: 12,
                bottom: 0,
                left: 0,
            },
            exclusive_zone: -1,
            keyboard_interactivity: KeyboardInteractivity::None,
            root: Element::new(Style::card(SURFACE, EDGE)),
            animation: None,
        }
    }

    /// Centered run launcher that takes keyboard focus.
    pub fn launcher() -> Self {
        Self {
            namespace: "desktop-rs-launcher",
            layer: Layer::Overlay,
            anchor: Anchor::empty(),
            width: 640,
            height: 320,
            margin: Margin::uniform(0),
            exclusive_zone: -1,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            root: Element::new(Style::card(SURFACE, EDGE)),
            animation: None,
        }
    }

    /// Defaults for one component, before the config file overrides them.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "bar" => Ok(Self::bar()),
            "notification" => Ok(Self::notification()),
            "launcher" => Ok(Self::launcher()),
            other => Err(anyhow::anyhow!(
                "unknown window `{other}`; expected bar, notification, or launcher"
            )),
        }
    }

    /// Size used before the compositor sends its first configure.
    pub fn initial_size(&self) -> (u32, u32) {
        (self.width.max(1), self.height.max(1))
    }

    /// Margin shifted by an in-flight slide, pushing the surface off its edge.
    pub fn margin_at(&self, offset: i32) -> Margin {
        use crate::ui::animation::Slide;

        let Some(animation) = self.animation else {
            return self.margin;
        };
        let mut margin = self.margin;
        match animation.slide {
            Slide::Top => margin.top -= offset,
            Slide::Right => margin.right -= offset,
            Slide::Bottom => margin.bottom -= offset,
            Slide::Left => margin.left -= offset,
        }
        margin
    }
}

/// Byte length of a tightly packed ARGB8888 buffer.
pub fn buffer_len(width: u32, height: u32) -> Result<usize> {
    let pixels = width
        .checked_mul(height)
        .context("surface dimensions overflow")?;
    let bytes = pixels.checked_mul(4).context("surface buffer overflow")?;
    usize::try_from(bytes).context("surface buffer does not fit memory address space")
}

#[cfg(test)]
mod tests {
    // Tests report failure by panicking; the crate-wide bans target runtime code.
    #![expect(clippy::panic, reason = "assertion failure in tests")]

    use super::*;
    use crate::ui::animation::Slide;

    #[test]
    fn buffer_len_should_count_four_bytes_per_pixel() -> Result<()> {
        assert_eq!(buffer_len(360, 160)?, 230_400);
        Ok(())
    }

    #[test]
    fn buffer_len_should_reject_overflow() {
        assert!(buffer_len(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn bar_should_reserve_its_own_height() {
        let bar = Window::bar();

        assert_eq!(bar.exclusive_zone, bar.height as i32);
    }

    #[test]
    fn launcher_should_take_exclusive_keyboard_focus() {
        assert_eq!(
            Window::launcher().keyboard_interactivity,
            KeyboardInteractivity::Exclusive
        );
    }

    #[test]
    fn notification_should_stay_out_of_other_windows_layout() {
        assert_eq!(Window::notification().exclusive_zone, -1);
    }

    #[test]
    fn from_name_should_reject_unknown_window() {
        let Err(error) = Window::from_name("dock") else {
            panic!("`dock` must not resolve to a window");
        };

        assert!(
            error.to_string().contains("unknown window `dock`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn initial_size_should_replace_compositor_chosen_width() {
        assert_eq!(Window::bar().initial_size(), (1, 36));
    }

    #[test]
    fn margin_at_should_push_the_surface_off_its_slide_edge() {
        let window = Window {
            animation: Some(Animation {
                slide: Slide::Right,
                distance: 400,
                duration_ms: 200,
                hold_ms: None,
            }),
            ..Window::notification()
        };

        assert_eq!(window.margin_at(400).right, 12 - 400);
        assert_eq!(window.margin_at(0), window.margin);
    }

    #[test]
    fn margin_at_should_ignore_offsets_without_an_animation() {
        let window = Window::notification();

        assert_eq!(window.margin_at(400), window.margin);
    }
}
