use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use nbcl::ast::resolved::ResolvedNode;
use nbcl::{NativeNodeSchema, NbclEngine, PropValidation, Value};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

use crate::platform::linux::audio::AudioTarget;
use crate::platform::linux::window::{Margin, Window};
use crate::ui::animation::{Animation, Slide};
use crate::ui::element::{Align, Content, Direction, Element, Size, Text};
use crate::ui::style::{Color, Style};
use crate::ui::text::validate_clock_format;

pub mod paths;

const COMPONENTS: [&str; 3] = ["Bar", "Notification", "Launcher"];

/// Reads the entry-point config, writing the bundled default on first run.
pub fn load() -> Result<HashMap<String, Window>> {
    let path = paths::entry_point()?;
    paths::write_default_if_missing(&path)?;
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("read config `{}`", path.display()))?;
    let mut engine = engine();
    engine.set_root_file(path.clone());
    let tree = engine
        .evaluate(&source)
        .map_err(|error| anyhow!("{error}"))
        .with_context(|| format!("evaluate config `{}`", path.display()))?;
    windows(&tree.root_nodes)
}

/// Engine with the component and `Box` nodes this config language defines.
fn engine() -> NbclEngine {
    let mut engine = NbclEngine::new();
    for type_name in COMPONENTS.iter().copied().chain([
        "Box",
        "Text",
        "Clock",
        "AppSearch",
        "AppList",
        "ActiveWindow",
        "Workspaces",
        "Audio",
        "Battery",
        "Backlight",
        "Network",
        "Bluetooth",
        "Custom",
        "Icon",
        "Tray",
    ]) {
        engine.register_node(NativeNodeSchema {
            type_name: type_name.to_owned(),
            enforce_id: type_name == "Custom",
            // ponytail: props are checked when read, not declared per node.
            // Swap in PropValidation::Strict once the prop set stops moving.
            validation: PropValidation::Loose,
            child_count: None,
        });
    }
    engine
}

fn windows(nodes: &[ResolvedNode]) -> Result<HashMap<String, Window>> {
    let mut windows = HashMap::new();
    for node in nodes {
        let Some(name) = COMPONENTS.iter().find(|name| **name == node.type_name) else {
            continue;
        };
        let key = name.to_lowercase();
        let base = Window::from_name(&key)?;
        windows.insert(
            key,
            window(node, base).with_context(|| format!("node `{name}`"))?,
        );
    }
    for name in COMPONENTS {
        let key = name.to_lowercase();
        if !windows.contains_key(&key) {
            bail!("config defines no `{name}` node");
        }
    }
    Ok(windows)
}

fn window(node: &ResolvedNode, base: Window) -> Result<Window> {
    let props = &node.props;
    Ok(Window {
        width: opt(props, "width", int_u32)?.unwrap_or(base.width),
        height: opt(props, "height", int_u32)?.unwrap_or(base.height),
        layer: opt(props, "layer", layer)?.unwrap_or(base.layer),
        anchor: opt(props, "anchor", anchor)?.unwrap_or(base.anchor),
        margin: opt(props, "margin", margin)?.unwrap_or(base.margin),
        exclusive_zone: opt(props, "exclusive_zone", int_i32)?.unwrap_or(base.exclusive_zone),
        keyboard_interactivity: opt(props, "keyboard_interactivity", keyboard)?
            .unwrap_or(base.keyboard_interactivity),
        animation: opt(props, "animation", animation)?,
        root: element(node, base.root.style)?,
        namespace: base.namespace,
    })
}

fn element(node: &ResolvedNode, fallback: Style) -> Result<Element> {
    let props = &node.props;
    let text_style = Text {
        value: opt(props, "value", text)?.unwrap_or_default(),
        color: opt(props, "color", color)?.unwrap_or(Color::rgba(0xff, 0xff, 0xff, 0xff)),
        font_size: opt(props, "font_size", int_u32)?.unwrap_or(14),
        font_family: opt(props, "font_family", text)?.unwrap_or_default(),
        align: opt(props, "text_align", align)?.unwrap_or(Align::Start),
    };
    let content = match node.type_name.as_str() {
        "Text" => Content::Text(text_style),
        "AppSearch" => Content::AppSearch {
            text: text_style.clone(),
        },
        "AppList" => Content::AppList {
            selected: Text {
                color: opt(props, "select_color", color)?.unwrap_or(text_style.color),
                ..text_style.clone()
            },
            select_background: opt(props, "select_background", color)?
                .unwrap_or(Color::rgba(0xff, 0xff, 0xff, 0x26)),
            text: text_style.clone(),
            rows: opt(props, "rows", int_u32)?.unwrap_or(8),
            icon_size: opt(props, "icon_size", int_u32)?.unwrap_or(24),
            icon_theme: opt(props, "icon_theme", text)?,
            terminal: opt(props, "terminal", string_list)?.unwrap_or_default(),
        },
        "ActiveWindow" => Content::ActiveWindow {
            text: text_style.clone(),
            max_chars: opt(props, "max_chars", int_u32)?,
            empty_value: opt(props, "empty_value", text)?.unwrap_or_default(),
        },
        "Workspaces" => Content::Workspaces {
            active: Text {
                color: opt(props, "active_color", color)?.unwrap_or(text_style.color),
                ..text_style.clone()
            },
            active_background: opt(props, "active_background", color)?
                .unwrap_or(Color::rgba(0xff, 0xff, 0xff, 0x26)),
            urgent: Text {
                color: opt(props, "urgent_color", color)?.unwrap_or(text_style.color),
                ..text_style.clone()
            },
            urgent_background: opt(props, "urgent_background", color)?
                .unwrap_or(Color::rgba(0xff, 0x55, 0x55, 0x33)),
            labels: opt(props, "labels", string_list)?.unwrap_or_default(),
            gap: opt(props, "gap", int_u32)?.unwrap_or(4),
            text: text_style,
        },
        "Audio" => Content::Audio {
            text: text_style,
            target: opt(props, "target", audio_target)?.unwrap_or(AudioTarget::Sink),
            format: opt(props, "format", text)?.unwrap_or_else(|| "{icon} {volume}%".to_owned()),
            icon: opt(props, "icon", text)?.unwrap_or_else(|| "VOL".to_owned()),
            muted_icon: opt(props, "muted_icon", text)?.unwrap_or_else(|| "MUTED".to_owned()),
            step: opt(props, "step", int_u32)?.unwrap_or(5),
            max_volume: opt(props, "max_volume", int_u32)?.unwrap_or(100),
        },
        "Battery" => Content::Battery {
            text: text_style,
            format: opt(props, "format", text)?.unwrap_or_else(|| "{icon} {capacity}%".to_owned()),
            icon: opt(props, "icon", text)?.unwrap_or_else(|| "BAT".to_owned()),
        },
        "Backlight" => Content::Backlight {
            text: text_style,
            format: opt(props, "format", text)?.unwrap_or_else(|| "{icon} {percent}%".to_owned()),
            icon: opt(props, "icon", text)?.unwrap_or_else(|| "BRT".to_owned()),
            step: opt(props, "step", int_u32)?.unwrap_or(5),
        },
        "Network" => Content::Network {
            text: text_style,
            format: opt(props, "format", text)?
                .unwrap_or_else(|| "{iface} {ssid} {signal}% {ipv4}/{prefix}".to_owned()),
            disconnected_format: opt(props, "disconnected_format", text)?
                .unwrap_or_else(|| "disconnected".to_owned()),
            interval: opt(props, "interval", int_u32)?.unwrap_or(5),
        },
        "Bluetooth" => Content::Bluetooth {
            text: text_style,
            format: opt(props, "format", text)?.unwrap_or_else(|| "BT {connected}".to_owned()),
            interval: opt(props, "interval", int_u32)?.unwrap_or(5),
        },
        "Custom" => Content::Custom {
            text: text_style,
            exec: opt(props, "exec", text)?.context("prop `exec` is required")?,
            interval: opt(props, "interval", int_u32)?.unwrap_or(5),
            format: opt(props, "format", text)?.unwrap_or_else(|| "{output}".to_owned()),
            on_click: opt(props, "on_click", text)?,
            on_right_click: opt(props, "on_right_click", text)?,
            on_middle_click: opt(props, "on_middle_click", text)?,
            on_scroll_up: opt(props, "on_scroll_up", text)?,
            on_scroll_down: opt(props, "on_scroll_down", text)?,
        },
        "Icon" => Content::Icon {
            name: opt(props, "name", text)?,
            theme_path: None,
            icon_theme: opt(props, "icon_theme", text)?,
            pixmaps: Vec::new(),
            size: opt(props, "icon_size", int_u32)?.unwrap_or(24),
            fallback: text_style,
        },
        "Tray" => Content::Tray {
            text: text_style,
            icon_size: opt(props, "icon_size", int_u32)?.unwrap_or(20),
            gap: opt(props, "gap", int_u32)?.unwrap_or(4),
        },
        "Clock" => {
            let format = opt(props, "format", text)?.unwrap_or_else(|| "%H:%M".to_owned());
            validate_clock_format(&format)?;
            Content::Clock {
                text: text_style,
                format,
            }
        }
        _ => Content::Box,
    };
    Ok(Element {
        id: node.id.clone(),
        content,
        style: Style {
            background: opt(props, "background", color)?.unwrap_or(fallback.background),
            border: opt(props, "border", color)?.unwrap_or(fallback.border),
            border_width: opt(props, "border_width", int_u32)?.unwrap_or(fallback.border_width),
            corner_radius: opt(props, "corner_radius", int_u32)?.unwrap_or(fallback.corner_radius),
        },
        width: opt(props, "width", size)?.unwrap_or(Size::Grow),
        height: opt(props, "height", size)?.unwrap_or(Size::Grow),
        padding: opt(props, "padding", int_u32)?.unwrap_or(0),
        gap: opt(props, "gap", int_u32)?.unwrap_or(0),
        direction: opt(props, "direction", direction)?.unwrap_or(Direction::Row),
        align: opt(props, "align", align)?.unwrap_or(Align::Start),
        justify: opt(props, "justify", align)?.unwrap_or(Align::Start),
        children: node
            .children
            .iter()
            .map(|child| element(child, Style::panel(Color::TRANSPARENT)))
            .collect::<Result<_>>()?,
    })
}

/// Reads one optional prop, tagging any conversion error with the key.
fn opt<T>(
    props: &HashMap<String, Value>,
    key: &str,
    convert: fn(&Value) -> Result<T>,
) -> Result<Option<T>> {
    props
        .get(key)
        .filter(|value| **value != Value::Null)
        .map(|value| convert(value).with_context(|| format!("prop `{key}`")))
        .transpose()
}

fn int_u32(value: &Value) -> Result<u32> {
    let int = value.get_int().context("expected an integer")?;
    u32::try_from(int).context("expected a non-negative integer")
}

fn int_i32(value: &Value) -> Result<i32> {
    let int = value.get_int().context("expected an integer")?;
    i32::try_from(int).context("integer out of range")
}

fn text(value: &Value) -> Result<String> {
    value.get_string().context("expected a string")
}

fn string_list(value: &Value) -> Result<Vec<String>> {
    let Value::List(items) = value else {
        return Err(anyhow!("expected a list of strings"));
    };
    items.iter().map(text).collect()
}

fn size(value: &Value) -> Result<Size> {
    match value {
        Value::Str(word) if word == "grow" => Ok(Size::Grow),
        Value::Str(word) if word == "fit" => Ok(Size::Fit),
        Value::Str(other) => Err(anyhow!(
            "expected an integer, \"grow\", or \"fit\", found `{other}`"
        )),
        other => int_u32(other).map(Size::Fixed),
    }
}

fn align(value: &Value) -> Result<Align> {
    match text(value)?.as_str() {
        "start" => Ok(Align::Start),
        "center" => Ok(Align::Center),
        "end" => Ok(Align::End),
        other => Err(anyhow!(
            "expected \"start\", \"center\", or \"end\", found `{other}`"
        )),
    }
}

fn direction(value: &Value) -> Result<Direction> {
    match text(value)?.as_str() {
        "row" => Ok(Direction::Row),
        "column" => Ok(Direction::Column),
        other => Err(anyhow!("expected \"row\" or \"column\", found `{other}`")),
    }
}

fn audio_target(value: &Value) -> Result<AudioTarget> {
    match text(value)?.as_str() {
        "sink" => Ok(AudioTarget::Sink),
        "source" => Ok(AudioTarget::Source),
        other => Err(anyhow!("expected \"sink\" or \"source\", found `{other}`")),
    }
}

fn layer(value: &Value) -> Result<Layer> {
    match text(value)?.as_str() {
        "background" => Ok(Layer::Background),
        "bottom" => Ok(Layer::Bottom),
        "top" => Ok(Layer::Top),
        "overlay" => Ok(Layer::Overlay),
        other => Err(anyhow!(
            "expected background, bottom, top, or overlay, found `{other}`"
        )),
    }
}

fn keyboard(value: &Value) -> Result<KeyboardInteractivity> {
    match text(value)?.as_str() {
        "none" => Ok(KeyboardInteractivity::None),
        "exclusive" => Ok(KeyboardInteractivity::Exclusive),
        "on_demand" => Ok(KeyboardInteractivity::OnDemand),
        other => Err(anyhow!(
            "expected none, exclusive, or on_demand, found `{other}`"
        )),
    }
}

fn anchor(value: &Value) -> Result<Anchor> {
    let Value::List(edges) = value else {
        return Err(anyhow!("expected a list of edges"));
    };
    let mut anchor = Anchor::empty();
    for edge in edges {
        anchor |= match text(edge)?.as_str() {
            "top" => Anchor::TOP,
            "bottom" => Anchor::BOTTOM,
            "left" => Anchor::LEFT,
            "right" => Anchor::RIGHT,
            other => bail!("expected top, bottom, left, or right, found `{other}`"),
        };
    }
    Ok(anchor)
}

fn margin(value: &Value) -> Result<Margin> {
    let entries = map(value)?;
    Ok(Margin {
        top: opt(&entries, "top", int_i32)?.unwrap_or(0),
        right: opt(&entries, "right", int_i32)?.unwrap_or(0),
        bottom: opt(&entries, "bottom", int_i32)?.unwrap_or(0),
        left: opt(&entries, "left", int_i32)?.unwrap_or(0),
    })
}

fn animation(value: &Value) -> Result<Animation> {
    let entries = map(value)?;
    Ok(Animation {
        slide: opt(&entries, "slide", slide)?.context("`slide` is required")?,
        distance: opt(&entries, "distance", int_i32)?.context("`distance` is required")?,
        duration_ms: opt(&entries, "duration_ms", int_u32)?.unwrap_or(200),
        hold_ms: opt(&entries, "hold_ms", int_u32)?,
    })
}

fn slide(value: &Value) -> Result<Slide> {
    match text(value)?.as_str() {
        "top" => Ok(Slide::Top),
        "right" => Ok(Slide::Right),
        "bottom" => Ok(Slide::Bottom),
        "left" => Ok(Slide::Left),
        other => Err(anyhow!(
            "expected top, right, bottom, or left, found `{other}`"
        )),
    }
}

fn map(value: &Value) -> Result<HashMap<String, Value>> {
    let Value::Map(entries) = value else {
        return Err(anyhow!("expected a map"));
    };
    Ok(entries.iter().cloned().collect())
}

/// `#rrggbb` or `#rrggbbaa`.
fn color(value: &Value) -> Result<Color> {
    let text = text(value)?;
    let digits = text
        .strip_prefix('#')
        .context("expected a color like \"#1b212bf7\"")?;
    if digits.len() != 6 && digits.len() != 8 {
        bail!("expected 6 or 8 hex digits, found {}", digits.len());
    }
    let channel = |start: usize| -> Result<u8> {
        let slice = digits.get(start..start + 2).context("color is too short")?;
        u8::from_str_radix(slice, 16).context("color holds a non-hex digit")
    };
    Ok(Color::rgba(
        channel(0)?,
        channel(2)?,
        channel(4)?,
        if digits.len() == 8 { channel(6)? } else { 0xff },
    ))
}

#[cfg(test)]
mod tests {
    // Tests report failure by panicking; the crate-wide bans target runtime code.
    #![expect(clippy::panic, reason = "assertion failure in tests")]

    use super::*;

    fn evaluate(source: &str) -> Result<HashMap<String, Window>> {
        evaluate_at(
            source,
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets")
                .join("config.nbcl"),
        )
    }

    fn evaluate_at(source: &str, path: &std::path::Path) -> Result<HashMap<String, Window>> {
        let mut engine = engine();
        engine.set_root_file(path.to_path_buf());
        let tree = engine
            .evaluate(source)
            .map_err(|error| anyhow!("{error}"))?;
        windows(&tree.root_nodes)
    }

    #[test]
    fn default_config_should_define_all_three_components() -> Result<()> {
        let windows = evaluate(paths::DEFAULT_CONFIG)?;

        assert_eq!(windows.len(), 3);
        Ok(())
    }

    #[test]
    fn default_config_should_give_the_notification_a_slide_out() -> Result<()> {
        let windows = evaluate(paths::DEFAULT_CONFIG)?;
        let animation = windows
            .get("notification")
            .and_then(|window| window.animation)
            .context("notification has no animation")?;

        assert_eq!(animation.slide, Slide::Right);
        assert_eq!(animation.hold_ms, Some(4000));
        Ok(())
    }

    #[test]
    fn default_config_should_build_the_bar_children() -> Result<()> {
        let windows = evaluate(paths::DEFAULT_CONFIG)?;
        let bar = windows.get("bar").context("config defines no bar")?;

        assert_eq!(bar.root.children.len(), 9);
        assert_eq!(bar.root.children[0].width, Size::Fit);
        assert_eq!(bar.root.children[1].width, Size::Grow);
        assert_eq!(bar.exclusive_zone, 40);
        Ok(())
    }

    #[test]
    fn every_bundled_example_should_evaluate() -> Result<()> {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        for name in ["minimal.nbcl", "full.nbcl", "launcher.nbcl"] {
            let path = assets.join("examples").join(name);
            let source = std::fs::read_to_string(&path)?;
            let windows = evaluate_at(&source, &path)
                .with_context(|| format!("evaluate bundled example `{name}`"))?;
            assert_eq!(windows.len(), 3);
        }
        Ok(())
    }

    #[test]
    fn every_bundled_theme_should_import() -> Result<()> {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        for name in [
            "tokyo-night.nbcl",
            "gruvbox.nbcl",
            "dracula.nbcl",
            "catppuccin-latte.nbcl",
            "catppuccin-frappe.nbcl",
            "catppuccin-macchiato.nbcl",
            "catppuccin-mocha.nbcl",
        ] {
            let source = paths::DEFAULT_CONFIG.replace("tokyo-night.nbcl", name);
            let path = assets.join("config.nbcl");
            let windows = evaluate_at(&source, &path)
                .with_context(|| format!("evaluate bundled theme `{name}`"))?;
            assert_eq!(windows.len(), 3);
        }
        Ok(())
    }

    #[test]
    fn custom_should_require_a_unique_node_id() {
        let source = paths::DEFAULT_CONFIG.replace(
            "    Audio {",
            "    Custom { exec = \"printf ok\" }\n    Audio {",
        );

        let Err(error) = evaluate(&source) else {
            panic!("Custom without ID must fail");
        };

        assert!(error.to_string().contains("requires an #id"));
    }

    #[test]
    fn current_dynamic_widget_subset_should_parse() -> Result<()> {
        let source = paths::DEFAULT_CONFIG.replace(
            "    Audio {",
            "    Battery { width = \"fit\" format = \"{capacity}% {status}\" }\n    Backlight { width = \"fit\" }\n    Network { width = \"fit\" interval = 3 }\n    Bluetooth { width = \"fit\" }\n    Custom \"kernel\" { width = \"fit\" exec = \"uname -r\" interval = 60 on_click = \"true\" }\n    Audio { target = \"source\" format = \"{icon} {volume}%\" icon = \"MIC\" muted_icon = \"MIC OFF\"",
        );

        let windows = evaluate(&source)?;

        assert_eq!(windows.get("bar").context("bar")?.root.children.len(), 14);
        Ok(())
    }

    #[test]
    fn app_list_should_default_icon_size_and_accept_icon_theme() -> Result<()> {
        let source = paths::DEFAULT_CONFIG.replace(
            "        icon_size = 24\n        icon_theme = \"hicolor\"\n",
            "",
        );
        let windows = evaluate(&source)?;
        let launcher = windows.get("launcher").context("launcher")?;
        let list = launcher
            .root
            .children
            .iter()
            .find(|element| matches!(element.content, Content::AppList { .. }))
            .context("AppList")?;

        let Content::AppList {
            icon_size,
            icon_theme,
            ..
        } = &list.content
        else {
            bail!("expected AppList");
        };
        assert_eq!((*icon_size, icon_theme.as_deref()), (24, None));
        Ok(())
    }

    #[test]
    fn icon_should_keep_theme_path_and_icon_theme_separate() -> Result<()> {
        let source = paths::DEFAULT_CONFIG.replace(
            "    Audio {",
            "    Icon { width = \"fit\" name = \"utilities-terminal\" icon_size = 32 icon_theme = \"Papirus\" }\n    Audio {",
        );
        let windows = evaluate(&source)?;
        let icon = windows
            .get("bar")
            .context("bar")?
            .root
            .children
            .iter()
            .find(|element| matches!(element.content, Content::Icon { .. }))
            .context("Icon")?;

        let Content::Icon {
            theme_path,
            icon_theme,
            size,
            ..
        } = &icon.content
        else {
            bail!("expected Icon");
        };
        assert_eq!(
            (theme_path.as_deref(), icon_theme.as_deref(), *size),
            (None, Some("Papirus"), 32)
        );
        Ok(())
    }

    #[test]
    fn color_should_default_alpha_to_opaque() -> Result<()> {
        assert_eq!(
            color(&Value::Str("#7aa2f7".to_owned()))?,
            Color::rgba(0x7a, 0xa2, 0xf7, 0xff)
        );
        Ok(())
    }

    #[test]
    fn color_should_reject_a_bad_length() {
        assert!(color(&Value::Str("#abc".to_owned())).is_err());
    }

    #[test]
    fn windows_should_reject_a_config_missing_a_component() {
        let Err(error) = evaluate("Bar { height = 30 }") else {
            panic!("a config without Notification must not load");
        };

        assert!(
            error.to_string().contains("no `Notification` node"),
            "unexpected error: {error}"
        );
    }
}
