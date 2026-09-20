use system_tray::item::IconPixmap;

use crate::platform::linux::audio::AudioTarget;
use crate::ui::style::{Color, Style};

/// Length along one axis. `Grow` splits leftover space; `Fit` uses content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Fixed(u32),
    Grow,
    Fit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub value: String,
    pub color: Color,
    pub font_size: u32,
    pub font_family: String,
    pub align: Align,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Box,
    Text(Text),
    Clock {
        text: Text,
        format: String,
    },
    /// Active window title reported by the compositor.
    ActiveWindow {
        text: Text,
        max_chars: Option<u32>,
        empty_value: String,
    },
    /// Workspace numbers/labels reported by the compositor.
    Workspaces {
        text: Text,
        active: Text,
        active_background: Color,
        urgent: Text,
        urgent_background: Color,
        labels: Vec<String>,
        gap: u32,
    },
    /// Image resolved and painted through the shared icon cache.
    Icon {
        name: Option<String>,
        theme_path: Option<String>,
        icon_theme: Option<String>,
        pixmaps: Vec<IconPixmap>,
        size: u32,
        fallback: Text,
    },
    /// Launcher placeholder the runtime expands from its app index.
    AppSearch {
        text: Text,
    },
    /// `text` styles unselected rows, `selected` styles the active row.
    AppList {
        text: Text,
        selected: Text,
        select_background: Color,
        rows: u32,
        icon_size: u32,
        icon_theme: Option<String>,
        /// argv prefix that runs `Terminal=true` entries, e.g. `["kitty", "-e"]`.
        terminal: Vec<String>,
    },
    /// PipeWire audio volume/mute display.
    Audio {
        text: Text,
        target: AudioTarget,
        format: String,
        icon: String,
        muted_icon: String,
        step: u32,
        max_volume: u32,
    },
    Battery {
        text: Text,
        format: String,
        icon: String,
    },
    Backlight {
        text: Text,
        format: String,
        icon: String,
        /// Percentage points one wheel notch moves brightness.
        step: u32,
    },
    Network {
        text: Text,
        format: String,
        disconnected_format: String,
        interval: u32,
    },
    Bluetooth {
        text: Text,
        format: String,
        interval: u32,
    },
    Custom {
        text: Text,
        exec: String,
        interval: u32,
        format: String,
        on_click: Option<String>,
        on_right_click: Option<String>,
        on_middle_click: Option<String>,
        on_scroll_up: Option<String>,
        on_scroll_down: Option<String>,
    },
    /// StatusNotifier items; runtime replaces this node with live children.
    Tray {
        text: Text,
        icon_size: u32,
        gap: u32,
    },
}

/// One painted element. Children stack along `direction` inside the padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub id: Option<String>,
    pub content: Content,
    pub style: Style,
    pub width: Size,
    pub height: Size,
    pub padding: u32,
    pub gap: u32,
    pub direction: Direction,
    pub align: Align,
    /// Main-axis distribution of leftover space. Only bites when no child grows.
    pub justify: Align,
    pub children: Vec<Element>,
}

impl Element {
    /// Leaf box filling whatever space its parent hands it.
    pub fn new(style: Style) -> Self {
        Self {
            id: None,
            content: Content::Box,
            style,
            width: Size::Grow,
            height: Size::Grow,
            padding: 0,
            gap: 0,
            direction: Direction::Row,
            align: Align::Start,
            justify: Align::Start,
            children: Vec::new(),
        }
    }
}

/// Surface-local rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x + self.width)
            && y < f64::from(self.y + self.height)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutItem {
    pub id: Option<String>,
    pub content: Content,
    pub rect: Rect,
    pub style: Style,
}

/// Flattens the tree into paint order: parents before children.
pub fn layout(
    root: &Element,
    width: u32,
    height: u32,
    measure: &mut impl FnMut(&Content) -> (u32, u32),
) -> Vec<LayoutItem> {
    let mut items = Vec::new();
    place(
        root,
        Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        measure,
        &mut items,
    );
    items
}

fn place(
    element: &Element,
    rect: Rect,
    measure: &mut impl FnMut(&Content) -> (u32, u32),
    items: &mut Vec<LayoutItem>,
) {
    items.push(LayoutItem {
        id: element.id.clone(),
        content: element.content.clone(),
        rect,
        style: element.style,
    });

    let Some(last) = element.children.len().checked_sub(1) else {
        return;
    };
    let pad = element.padding as f32;
    let inner = Rect {
        x: rect.x + pad,
        y: rect.y + pad,
        width: (rect.width - pad * 2.0).max(0.0),
        height: (rect.height - pad * 2.0).max(0.0),
    };
    let row = element.direction == Direction::Row;
    let (main, cross) = if row {
        (inner.width, inner.height)
    } else {
        (inner.height, inner.width)
    };

    let measured: Vec<(u32, u32)> = element
        .children
        .iter()
        .map(|child| natural_size(child, measure))
        .collect();
    let main_len = |index: usize, child: &Element| {
        let size = if row { child.width } else { child.height };
        match size {
            Size::Fixed(value) => Some(value as f32),
            Size::Fit => Some(if row {
                measured[index].0
            } else {
                measured[index].1
            } as f32),
            Size::Grow => None,
        }
    };
    let gaps = element.gap as f32 * last as f32;
    let taken: f32 = element
        .children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| main_len(index, child))
        .sum();
    let growers = element
        .children
        .iter()
        .filter(|child| {
            if row {
                child.width == Size::Grow
            } else {
                child.height == Size::Grow
            }
        })
        .count();
    let share = if growers == 0 {
        0.0
    } else {
        ((main - gaps - taken) / growers as f32).max(0.0)
    };

    let mut offset = if growers == 0 {
        match element.justify {
            Align::Start => 0.0,
            Align::Center => ((main - gaps - taken) / 2.0).max(0.0),
            Align::End => (main - gaps - taken).max(0.0),
        }
    } else {
        0.0
    };
    for (index, child) in element.children.iter().enumerate() {
        let child_main = main_len(index, child).unwrap_or(share).min(main.max(0.0));
        let cross_size = if row { child.height } else { child.width };
        let natural_cross = if row {
            measured[index].1
        } else {
            measured[index].0
        } as f32;
        let child_cross = match cross_size {
            Size::Fixed(value) => (value as f32).min(cross),
            Size::Fit => natural_cross.min(cross),
            Size::Grow => cross,
        };
        let cross_offset = match element.align {
            Align::Start => 0.0,
            Align::Center => (cross - child_cross) / 2.0,
            Align::End => cross - child_cross,
        }
        .max(0.0);
        let child_rect = if row {
            Rect {
                x: inner.x + offset,
                y: inner.y + cross_offset,
                width: child_main,
                height: child_cross,
            }
        } else {
            Rect {
                x: inner.x + cross_offset,
                y: inner.y + offset,
                width: child_cross,
                height: child_main,
            }
        };
        place(child, child_rect, measure, items);
        offset += child_main + element.gap as f32;
    }
}

fn natural_size(element: &Element, measure: &mut impl FnMut(&Content) -> (u32, u32)) -> (u32, u32) {
    let own = measure(&element.content);
    if element.children.is_empty() {
        return own;
    }

    let children = element
        .children
        .iter()
        .map(|child| natural_size(child, measure))
        .collect::<Vec<_>>();
    let gaps = element
        .gap
        .saturating_mul(u32::try_from(children.len().saturating_sub(1)).unwrap_or(u32::MAX));
    let padding = element.padding.saturating_mul(2);
    let (content_width, content_height) = match element.direction {
        Direction::Row => (
            children
                .iter()
                .map(|(width, _)| *width)
                .fold(gaps, u32::saturating_add),
            children
                .iter()
                .map(|(_, height)| *height)
                .max()
                .unwrap_or_default(),
        ),
        Direction::Column => (
            children
                .iter()
                .map(|(width, _)| *width)
                .max()
                .unwrap_or_default(),
            children
                .iter()
                .map(|(_, height)| *height)
                .fold(gaps, u32::saturating_add),
        ),
    };
    (
        own.0.max(content_width.saturating_add(padding)),
        own.1.max(content_height.saturating_add(padding)),
    )
}

/// Last named element under this point wins because children paint over parents.
pub fn hit_test(items: &[LayoutItem], x: f64, y: f64) -> Option<&str> {
    items
        .iter()
        .rev()
        .find(|item| item.id.is_some() && item.rect.contains(x, y))
        .and_then(|item| item.id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: Color = Color::rgba(0x10, 0x10, 0x10, 0xff);

    fn leaf(width: Size) -> Element {
        Element {
            width,
            ..Element::new(Style::panel(BG))
        }
    }

    fn row(children: Vec<Element>, padding: u32, gap: u32) -> Element {
        Element {
            padding,
            gap,
            children,
            ..Element::new(Style::panel(BG))
        }
    }

    fn zero(_: &Content) -> (u32, u32) {
        (0, 0)
    }

    #[test]
    fn layout_should_emit_parent_before_children() {
        let items = layout(&row(vec![leaf(Size::Grow)], 0, 0), 100, 20, &mut zero);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].rect.width, 100.0);
    }

    #[test]
    fn layout_should_give_leftover_space_to_growers() {
        let tree = row(vec![leaf(Size::Fixed(30)), leaf(Size::Grow)], 0, 0);

        let items = layout(&tree, 100, 20, &mut zero);

        assert_eq!(items[1].rect.width, 30.0);
        assert_eq!(items[2].rect.x, 30.0);
        assert_eq!(items[2].rect.width, 70.0);
    }

    #[test]
    fn layout_should_use_measured_content_for_fit() {
        let tree = row(vec![leaf(Size::Fit), leaf(Size::Grow)], 0, 0);

        let items = layout(&tree, 100, 20, &mut |_| (28, 12));

        assert_eq!(items[1].rect.width, 28.0);
        assert_eq!(items[2].rect.width, 72.0);
    }

    #[test]
    fn layout_should_measure_fit_containers_from_descendants() {
        let tree = row(
            vec![Element {
                width: Size::Fit,
                padding: 5,
                children: vec![leaf(Size::Fit)],
                ..Element::new(Style::panel(BG))
            }],
            0,
            0,
        );

        let items = layout(&tree, 100, 30, &mut |_| (28, 12));

        assert_eq!(items[1].rect.width, 38.0);
        assert_eq!(items[2].rect.width, 28.0);
    }

    #[test]
    fn justify_should_center_fixed_children_along_the_main_axis() {
        let tree = Element {
            justify: Align::Center,
            children: vec![Element {
                width: Size::Fixed(20),
                ..Element::new(Style::panel(BG))
            }],
            ..Element::new(Style::panel(BG))
        };

        let items = layout(&tree, 100, 30, &mut zero);

        assert_eq!(items[1].rect.x, 40.0);
    }

    #[test]
    fn justify_should_push_fixed_children_to_the_end() {
        let tree = Element {
            justify: Align::End,
            children: vec![Element {
                width: Size::Fixed(20),
                ..Element::new(Style::panel(BG))
            }],
            ..Element::new(Style::panel(BG))
        };

        let items = layout(&tree, 100, 30, &mut zero);

        assert_eq!(items[1].rect.x, 80.0);
    }

    #[test]
    fn justify_should_defer_to_growing_children() {
        let tree = Element {
            justify: Align::Center,
            children: vec![leaf(Size::Grow)],
            ..Element::new(Style::panel(BG))
        };

        let items = layout(&tree, 100, 30, &mut zero);

        assert_eq!(items[1].rect.x, 0.0);
        assert_eq!(items[1].rect.width, 100.0);
    }

    #[test]
    fn layout_should_center_children_across_the_row() {
        let tree = Element {
            align: Align::Center,
            children: vec![Element {
                height: Size::Fixed(10),
                ..leaf(Size::Grow)
            }],
            ..Element::new(Style::panel(BG))
        };

        let items = layout(&tree, 100, 30, &mut zero);

        assert_eq!(items[1].rect.y, 10.0);
    }

    #[test]
    fn layout_should_stack_columns_downwards() {
        let tree = Element {
            direction: Direction::Column,
            children: vec![
                Element {
                    height: Size::Fixed(12),
                    ..Element::new(Style::panel(BG))
                },
                Element::new(Style::panel(BG)),
            ],
            ..Element::new(Style::panel(BG))
        };

        let items = layout(&tree, 100, 40, &mut zero);

        assert_eq!(items[1].rect.height, 12.0);
        assert_eq!(items[2].rect.y, 12.0);
        assert_eq!(items[2].rect.height, 28.0);
    }

    #[test]
    fn hit_test_should_choose_the_topmost_named_item() {
        let items = vec![
            LayoutItem {
                id: Some("parent".to_owned()),
                content: Content::Box,
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 30.0,
                    height: 30.0,
                },
                style: Style::panel(BG),
            },
            LayoutItem {
                id: Some("child".to_owned()),
                content: Content::Box,
                rect: Rect {
                    x: 5.0,
                    y: 5.0,
                    width: 10.0,
                    height: 10.0,
                },
                style: Style::panel(BG),
            },
        ];

        assert_eq!(hit_test(&items, 6.0, 6.0), Some("child"));
        assert_eq!(hit_test(&items, 29.0, 29.0), Some("parent"));
        assert_eq!(hit_test(&items, 31.0, 31.0), None);
    }
}
