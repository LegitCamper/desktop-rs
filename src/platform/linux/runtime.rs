use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Surface},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            channel::{self, Event as ChannelEvent, Sender},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::pointer::AxisScroll,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        xdg::{
            XdgPositioner, XdgShell,
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{Window as XdgWindow, WindowConfigure, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use wayland_protocols::xdg::shell::client::xdg_positioner;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};

use crate::platform::linux::audio::AudioClient;
use crate::platform::linux::status_notifier::{TrayClient, TrayItem, TrayMenuItem};
use crate::platform::linux::taskbar::Taskbar;

use crate::platform::linux::apps::Apps;
use crate::platform::linux::window::{Window, buffer_len};
use crate::ui::animation::Phase;
use crate::ui::element::{self, Align, Content, Direction, Element, LayoutItem, Rect, Size, Text};
use crate::ui::image::ImageCache;
use crate::ui::paint;
use crate::ui::style::{Color, Style};
use crate::ui::text::{TextRenderer, line_height};

/// Launcher result-row ids carry the desktop-entry id after this prefix.
const ROW_PREFIX: &str = "app:";
/// Vertical padding inside one result row, in pixels.
const ROW_PADDING: u32 = 5;
/// Corner radius of the active row.
const ROW_RADIUS: u32 = 6;
/// Rows shown when a launcher configures no `AppList`.
const DEFAULT_ROWS: usize = 8;
/// How often the startup desktop-entry scan is polled before it publishes.
const SCAN_POLL: Duration = Duration::from_millis(100);
/// Linux `BTN_LEFT`; Wayland forwards raw kernel button codes.
const BTN_LEFT: u32 = 0x110;
/// Linux `BTN_RIGHT`; dismisses the launcher.
const BTN_RIGHT: u32 = 0x111;
/// Text caret drawn after the query.
const CARET: &str = "\u{258c}";
/// Marks the active row without relying on colour alone.
const SELECTED: &str = "\u{25b8} ";
/// Marks a row holding a launch failure.
const FAILURE: &str = "\u{26a0}";
const MENU_WIDTH: u32 = 260;
const MENU_ROW_HEIGHT: u32 = 28;
const MENU_PADDING: u32 = 6;
const MENU_BACKGROUND: Color = Color::rgba(0x1b, 0x21, 0x2b, 0xfc);
const MENU_BORDER: Color = Color::rgba(0x5c, 0x6a, 0x82, 0xc0);
const MENU_TEXT: Color = Color::rgba(0xc0, 0xca, 0xf5, 0xff);
const MENU_DISABLED: Color = Color::rgba(0x56, 0x5f, 0x89, 0xff);
const MENU_SELECTED: Color = Color::rgba(0x41, 0x48, 0x68, 0xff);

/// Cross-thread wakeup; backend phases send this after model updates.
#[derive(Debug)]
pub enum BackendEvent {
    Redraw,
}

pub fn run(window: Window) -> Result<()> {
    let connection = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let (globals, event_queue) =
        registry_queue_init(&connection).context("read Wayland globals")?;
    let queue_handle = event_queue.handle();
    let mut event_loop: EventLoop<State> = EventLoop::try_new().context("create event loop")?;
    WaylandSource::new(connection.clone(), event_queue)
        .insert(event_loop.handle())
        .context("insert Wayland event source")?;

    let compositor = CompositorState::bind(&globals, &queue_handle)
        .context("Wayland compositor lacks wl_compositor")?;
    let xdg_shell =
        XdgShell::bind(&globals, &queue_handle).context("Wayland compositor lacks xdg-shell")?;
    let layer_shell = LayerShell::bind(&globals, &queue_handle)
        .context("Wayland compositor lacks wlr-layer-shell")?;
    let shm = Shm::bind(&globals, &queue_handle).context("Wayland compositor lacks wl_shm")?;

    let surface = compositor.create_surface(&queue_handle);
    let layer = layer_shell.create_layer_surface(
        &queue_handle,
        surface,
        window.layer,
        Some(window.namespace),
        None,
    );
    layer.set_anchor(window.anchor);
    layer.set_size(window.width, window.height);
    layer.set_exclusive_zone(window.exclusive_zone);
    let start = window.margin_at(window.animation.map_or(0, |animation| animation.distance));
    layer.set_margin(start.top, start.right, start.bottom, start.left);
    layer.set_keyboard_interactivity(window.keyboard_interactivity);
    layer.commit();

    let taskbar = Taskbar::bind(&globals, &queue_handle);
    let (width, height) = window.initial_size();
    let pool =
        SlotPool::new(buffer_len(width, height)?, &shm).context("create shared-memory pool")?;
    let mut apps: Option<Apps> = None;
    let (backend_sender, backend_channel) = channel::channel();
    let audio = contains_audio(&window.root).then(|| AudioClient::start(backend_sender.clone()));
    let tray = contains_tray(&window.root).then(|| TrayClient::start(backend_sender.clone()));
    backend_sender
        .send(BackendEvent::Redraw)
        .map_err(|error| anyhow::anyhow!("prime backend channel: {error}"))?;
    event_loop
        .handle()
        .insert_source(backend_channel, |event, _, state| {
            if matches!(event, ChannelEvent::Msg(BackendEvent::Redraw)) {
                state.redraw();
            }
        })
        .map_err(|error| anyhow::anyhow!("insert backend channel: {error}"))?;

    if contains_clock(&window.root) {
        event_loop
            .handle()
            .insert_source(Timer::from_duration(next_second()), |_, _, state| {
                state.redraw();
                TimeoutAction::ToDuration(next_second())
            })
            .map_err(|error| anyhow::anyhow!("insert clock timer: {error}"))?;
    }

    if let Some((rows, terminal)) = launcher_config(&window.root) {
        let mut launcher_apps = Apps::scan();
        launcher_apps.set_visible(rows);
        launcher_apps.set_terminal(terminal);
        apps = Some(launcher_apps);
        event_loop
            .handle()
            .insert_source(Timer::from_duration(SCAN_POLL), |_, _, state| {
                state.poll_apps();
                if state.scanning() {
                    return TimeoutAction::ToDuration(SCAN_POLL);
                }
                state.redraw();
                TimeoutAction::Drop
            })
            .map_err(|error| anyhow::anyhow!("insert desktop-index timer: {error}"))?;
    }

    if let Some(delay) = animation_wakeup(&window) {
        event_loop
            .handle()
            .insert_source(Timer::from_duration(delay), |_, _, state| {
                state.redraw();
                state.animation_wakeup().map_or_else(
                    || {
                        state.redraw();
                        TimeoutAction::Drop
                    },
                    TimeoutAction::ToDuration,
                )
            })
            .map_err(|error| anyhow::anyhow!("insert animation timer: {error}"))?;
    }

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        compositor,
        xdg_shell,
        seat_state: SeatState::new(&globals, &queue_handle),
        output_state: OutputState::new(&globals, &queue_handle),
        shm,
        layer,
        pool,
        window,
        queue_handle,
        width,
        height,
        drawn: false,
        mapped_at: Instant::now(),
        exit: false,
        error: None,
        text: TextRenderer::new(),
        images: ImageCache::default(),
        layout: Vec::new(),
        keyboard: None,
        pointer: None,
        seat: None,
        last_serial: None,
        modifiers: Modifiers::default(),
        pointer_position: None,
        hovered: None,
        apps,
        taskbar,
        audio,
        tray,
        menu: None,
        status: None,
        _backend_sender: backend_sender,
    };

    while !state.exit && state.error.is_none() {
        event_loop
            .dispatch(None, &mut state)
            .context("dispatch desktop event")?;
    }
    if let Some(error) = state.error {
        bail!(error);
    }
    Ok(())
}

fn next_second() -> Duration {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    Duration::from_nanos(1_000_000_000 - u64::from(elapsed.subsec_nanos()))
}

/// Rows and terminal argv for a launcher surface, or `None` for other trees.
fn launcher_config(root: &Element) -> Option<(usize, Vec<String>)> {
    if !is_launcher(root) {
        return None;
    }
    Some(match find_app_list(root) {
        Some(Content::AppList { rows, terminal, .. }) => (*rows as usize, terminal.clone()),
        _ => (DEFAULT_ROWS, Vec::new()),
    })
}

fn is_launcher(element: &Element) -> bool {
    matches!(
        element.content,
        Content::AppSearch { .. } | Content::AppList { .. }
    ) || element.children.iter().any(is_launcher)
}

fn find_app_list(element: &Element) -> Option<&Content> {
    matches!(element.content, Content::AppList { .. })
        .then_some(&element.content)
        .or_else(|| element.children.iter().find_map(find_app_list))
}

/// Replaces dynamic shell widgets (launcher, title, workspaces) with live state.
fn expand(
    root: &Element,
    apps: Option<&Apps>,
    taskbar: &Taskbar,
    audio: Option<&AudioClient>,
    tray: Option<&TrayClient>,
    status: Option<&str>,
) -> Element {
    match &root.content {
        Content::AppSearch { text } => {
            let line = match apps {
                Some(apps) => search_line(text, apps),
                None => text.clone(),
            };
            Element {
                content: Content::Box,
                children: vec![Element {
                    content: Content::Text(line),
                    ..Element::new(Style::panel(Color::TRANSPARENT))
                }],
                ..root.clone()
            }
        }
        Content::AppList {
            text,
            selected,
            select_background,
            ..
        } => {
            let children = match apps {
                Some(apps) => result_rows(apps, status, text, selected, *select_background),
                None => Vec::new(),
            };
            Element {
                children,
                ..root.clone()
            }
        }
        Content::ActiveWindow {
            text,
            max_chars,
            empty_value,
        } => {
            let mut line = text.clone();
            line.value = match taskbar.toplevels.active_title() {
                Some(title) => match max_chars {
                    Some(max) if title.chars().count() > *max as usize => {
                        format!("{}…", title.chars().take(*max as usize).collect::<String>())
                    }
                    _ => title.to_owned(),
                },
                None => empty_value.clone(),
            };
            Element {
                content: Content::Text(line),
                ..root.clone()
            }
        }
        Content::Workspaces {
            text,
            active,
            active_background,
            urgent,
            urgent_background,
            gap,
        } => {
            let list = taskbar.workspaces.workspaces();
            let children = list
                .iter()
                .map(|ws| {
                    let (ws_text, bg) = if ws.active {
                        (active, *active_background)
                    } else if ws.urgent {
                        (urgent, *urgent_background)
                    } else {
                        (text, Color::TRANSPARENT)
                    };
                    Element {
                        id: Some(format!("ws:{}", ws.id)),
                        content: Content::Text(Text {
                            value: if ws.name.is_empty() {
                                ws.id.clone()
                            } else {
                                ws.name.clone()
                            },
                            ..ws_text.clone()
                        }),
                        style: Style {
                            corner_radius: 4,
                            ..Style::panel(bg)
                        },
                        width: Size::Fit,
                        height: Size::Fit,
                        padding: 3,
                        align: Align::Center,
                        direction: Direction::Row,
                        ..Element::new(Style::panel(Color::TRANSPARENT))
                    }
                })
                .collect();
            Element {
                content: Content::Box,
                direction: Direction::Row,
                gap: *gap,
                align: Align::Center,
                children,
                ..root.clone()
            }
        }
        Content::Audio { text, .. } => {
            let mut line = text.clone();
            line.value = match audio.map(AudioClient::snapshot) {
                Some(snapshot) if snapshot.available && snapshot.muted => {
                    format!("MUTED {}%", snapshot.volume)
                }
                Some(snapshot) if snapshot.available => format!("VOL {}%", snapshot.volume),
                _ => "VOL --".to_owned(),
            };
            Element {
                id: Some("audio".to_owned()),
                content: Content::Text(line),
                ..root.clone()
            }
        }
        Content::Tray {
            text,
            icon_size,
            gap,
        } => {
            let children = tray
                .map(TrayClient::snapshot)
                .into_iter()
                .flat_map(|snapshot| snapshot.items)
                .filter(|item| item.status != system_tray::item::Status::Passive)
                .map(|item| tray_item(item, text, *icon_size))
                .collect();
            Element {
                content: Content::Box,
                width: Size::Fit,
                direction: Direction::Row,
                gap: *gap,
                align: Align::Center,
                children,
                ..root.clone()
            }
        }
        _ => Element {
            children: root
                .children
                .iter()
                .map(|child| expand(child, apps, taskbar, audio, tray, status))
                .collect(),
            ..root.clone()
        },
    }
}

fn tray_item(item: TrayItem, text: &Text, icon_size: u32) -> Element {
    let mut fallback = text.clone();
    fallback.value = item.title.chars().next().unwrap_or('?').to_string();
    Element {
        id: Some(format!("tray:{}", item.address)),
        content: Content::Icon {
            name: item.icon_name,
            theme_path: item.icon_theme_path,
            pixmaps: item.icon_pixmaps,
            size: icon_size,
            fallback,
        },
        width: Size::Fixed(icon_size),
        height: Size::Fixed(icon_size),
        padding: 3,
        align: Align::Center,
        direction: Direction::Row,
        ..Element::new(Style::panel(Color::TRANSPARENT))
    }
}

fn search_line(placeholder: &Text, apps: &Apps) -> Text {
    let mut line = placeholder.clone();
    line.value = if !apps.query().is_empty() {
        format!("{}{}", apps.query(), CARET)
    } else if apps.is_scanning() {
        "indexing applications\u{2026}".to_owned()
    } else if apps.matches().is_empty() {
        "no applications found".to_owned()
    } else {
        placeholder.value.clone()
    };
    line
}

/// One row per visible match, plus a leading error row when a launch failed.
fn result_rows(
    apps: &Apps,
    status: Option<&str>,
    text: &Text,
    selected: &Text,
    select_background: Color,
) -> Vec<Element> {
    let height = line_height(text.font_size).ceil() as u32 + ROW_PADDING * 2;
    let mut rows = status
        .into_iter()
        .map(|message| Element {
            content: Content::Text(Text {
                value: format!("{FAILURE} {message}"),
                ..selected.clone()
            }),
            height: Size::Fixed(height),
            padding: ROW_PADDING,
            ..Element::new(Style::panel(Color::TRANSPARENT))
        })
        .collect::<Vec<_>>();
    rows.extend(apps.page().into_iter().map(|(app, active)| {
        let marker = if active { SELECTED } else { "  " };
        let label = if app.comment.is_empty() {
            format!("{marker}{}", app.name)
        } else {
            format!("{marker}{} \u{2014} {}", app.name, app.comment)
        };
        Element {
            id: Some(format!("{ROW_PREFIX}{}", app.id)),
            content: Content::Text(Text {
                value: label,
                ..(if active { selected } else { text }).clone()
            }),
            style: if active {
                Style {
                    corner_radius: ROW_RADIUS,
                    ..Style::panel(select_background)
                }
            } else {
                Style::panel(Color::TRANSPARENT)
            },
            height: Size::Fixed(height),
            padding: ROW_PADDING,
            align: Align::Center,
            direction: Direction::Row,
            ..Element::new(Style::panel(Color::TRANSPARENT))
        }
    }));
    rows
}

fn find_audio_config(element: &Element) -> Option<(u32, u32)> {
    match &element.content {
        Content::Audio {
            step, max_volume, ..
        } => Some((*step, *max_volume)),
        _ => element.children.iter().find_map(find_audio_config),
    }
}

fn contains_clock(element: &element::Element) -> bool {
    matches!(element.content, Content::Clock { .. }) || element.children.iter().any(contains_clock)
}

fn contains_audio(element: &Element) -> bool {
    matches!(element.content, Content::Audio { .. }) || element.children.iter().any(contains_audio)
}

fn contains_tray(element: &Element) -> bool {
    matches!(element.content, Content::Tray { .. }) || element.children.iter().any(contains_tray)
}

fn animation_wakeup(window: &Window) -> Option<Duration> {
    window.animation.and_then(|animation| {
        animation.hold_ms.map(|hold_ms| {
            Duration::from_millis(
                u64::from(animation.duration_ms)
                    .saturating_add(u64::from(hold_ms))
                    .max(1),
            )
        })
    })
}

/// Menu navigation state; kept free of Wayland objects so it can be tested.
#[derive(Debug)]
struct MenuState {
    levels: Vec<Vec<TrayMenuItem>>,
    selected: usize,
}

impl MenuState {
    fn new(items: Vec<TrayMenuItem>) -> Self {
        Self {
            selected: first_selectable(&items).unwrap_or(0),
            levels: vec![items],
        }
    }

    fn items(&self) -> &[TrayMenuItem] {
        self.levels.last().map_or(&[], Vec::as_slice)
    }

    fn move_selection(&mut self, delta: isize) {
        let selectable = self
            .items()
            .iter()
            .enumerate()
            .filter(|(_, item)| item.enabled && !item.separator)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            return;
        }
        let current = selectable
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(selectable.len() as isize) as usize;
        self.selected = selectable[next];
    }

    fn enter_submenu(&mut self) -> bool {
        let Some(submenu) = self
            .items()
            .get(self.selected)
            .map(|item| item.submenu.clone())
        else {
            return false;
        };
        if submenu.is_empty() {
            return false;
        }
        self.selected = first_selectable(&submenu).unwrap_or(0);
        self.levels.push(submenu);
        true
    }

    fn leave_submenu(&mut self) -> bool {
        if self.levels.len() <= 1 {
            return false;
        }
        self.levels.pop();
        self.selected = first_selectable(self.items()).unwrap_or(0);
        true
    }
}

struct MenuPopup {
    popup: Popup,
    address: String,
    menu_path: String,
    state: MenuState,
    width: u32,
    height: u32,
    layout: Vec<LayoutItem>,
}

impl MenuPopup {
    fn new(popup: Popup, item: &TrayItem) -> Option<Self> {
        let menu_path = item.menu_path.clone()?;
        if item.menu.is_empty() {
            return None;
        }
        let height = menu_height(&item.menu);
        Some(Self {
            popup,
            address: item.address.clone(),
            menu_path,
            state: MenuState::new(item.menu.clone()),
            width: MENU_WIDTH,
            height,
            layout: Vec::new(),
        })
    }

    fn items(&self) -> &[TrayMenuItem] {
        self.state.items()
    }

    fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
    }

    fn enter_submenu(&mut self) -> bool {
        let entered = self.state.enter_submenu();
        if entered {
            self.height = menu_height(self.items());
        }
        entered
    }

    fn leave_submenu(&mut self) -> bool {
        let left = self.state.leave_submenu();
        if left {
            self.height = menu_height(self.items());
        }
        left
    }

    /// Row index under the pointer, if any.
    fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        element::hit_test(&self.layout, x, y)
            .and_then(|id| id.strip_prefix("menu:"))
            .and_then(|index| index.parse().ok())
    }
}

fn first_selectable(items: &[TrayMenuItem]) -> Option<usize> {
    items
        .iter()
        .position(|item| item.enabled && !item.separator)
}

fn menu_height(items: &[TrayMenuItem]) -> u32 {
    MENU_PADDING
        .saturating_mul(2)
        .saturating_add(MENU_ROW_HEIGHT.saturating_mul(items.len() as u32))
}

fn menu_element(items: &[TrayMenuItem], selected: usize) -> Element {
    let children = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let marker = match item.toggle {
                Some(true) => "✓ ",
                Some(false) => "  ",
                None => "",
            };
            let submenu = if item.submenu.is_empty() { "" } else { "  ›" };
            let value = if item.separator {
                "─".repeat(24)
            } else {
                format!("{marker}{}{submenu}", item.label)
            };
            Element {
                id: Some(format!("menu:{index}")),
                content: Content::Text(Text {
                    value,
                    color: if item.enabled {
                        MENU_TEXT
                    } else {
                        MENU_DISABLED
                    },
                    font_size: 14,
                    font_family: String::new(),
                    align: Align::Start,
                }),
                style: Style {
                    corner_radius: 4,
                    ..Style::panel(if index == selected && !item.separator {
                        MENU_SELECTED
                    } else {
                        Color::TRANSPARENT
                    })
                },
                width: Size::Grow,
                height: Size::Fixed(MENU_ROW_HEIGHT),
                padding: 5,
                ..Element::new(Style::panel(Color::TRANSPARENT))
            }
        })
        .collect();
    Element {
        style: Style::card(MENU_BACKGROUND, MENU_BORDER),
        width: Size::Grow,
        height: Size::Grow,
        padding: MENU_PADDING,
        gap: 0,
        direction: Direction::Column,
        children,
        ..Element::new(Style::panel(MENU_BACKGROUND))
    }
}

struct State {
    registry_state: RegistryState,
    compositor: CompositorState,
    xdg_shell: XdgShell,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    window: Window,
    queue_handle: QueueHandle<Self>,
    width: u32,
    height: u32,
    drawn: bool,
    mapped_at: Instant,
    exit: bool,
    error: Option<anyhow::Error>,
    text: TextRenderer,
    images: ImageCache,
    layout: Vec<LayoutItem>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    seat: Option<wl_seat::WlSeat>,
    last_serial: Option<u32>,
    modifiers: Modifiers,
    pointer_position: Option<(f64, f64)>,
    hovered: Option<String>,
    apps: Option<Apps>,
    taskbar: Taskbar,
    audio: Option<AudioClient>,
    tray: Option<TrayClient>,
    menu: Option<MenuPopup>,
    /// Last launcher error to show until the next key or click.
    status: Option<String>,
    _backend_sender: Sender<BackendEvent>,
}

impl State {
    fn animation_wakeup(&self) -> Option<Duration> {
        if !self.drawn {
            return Some(Duration::from_millis(16));
        }
        let animation = self.window.animation?;
        let hold_ms = animation.hold_ms?;
        let elapsed = self
            .mapped_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let out_start = u64::from(animation.duration_ms).saturating_add(u64::from(hold_ms));
        (elapsed < out_start).then(|| Duration::from_millis((out_start - elapsed).max(1)))
    }

    fn redraw(&mut self) {
        if !self.drawn {
            return;
        }
        let queue_handle = self.queue_handle.clone();
        if let Err(error) = self.draw(&queue_handle) {
            self.error = Some(error);
        }
    }

    fn draw(&mut self, queue_handle: &QueueHandle<Self>) -> Result<()> {
        let elapsed = self
            .mapped_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let phase = match self.window.animation {
            Some(animation) => {
                let (phase, offset) = animation.offset(elapsed);
                let margin = self.window.margin_at(offset);
                self.layer
                    .set_margin(margin.top, margin.right, margin.bottom, margin.left);
                phase
            }
            None => Phase::Resting,
        };
        if phase == Phase::Done {
            self.exit = true;
            return Ok(());
        }

        let width = i32::try_from(self.width).context("surface width exceeds Wayland limit")?;
        let height = i32::try_from(self.height).context("surface height exceeds Wayland limit")?;
        let stride = width.checked_mul(4).context("surface stride overflow")?;
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .context("create shared-memory buffer")?;

        let root = expand(
            &self.window.root,
            self.apps.as_ref(),
            &self.taskbar,
            self.audio.as_ref(),
            self.tray.as_ref(),
            self.status.as_deref(),
        );
        self.layout = element::layout(&root, self.width, self.height, &mut |content| {
            self.text.measure(content)
        });
        paint::render(
            canvas,
            self.width,
            self.height,
            &self.layout,
            &mut self.text,
            &mut self.images,
        )?;

        let surface = self.layer.wl_surface();
        if phase == Phase::Moving {
            surface.frame(queue_handle, FrameCallbackData(surface.clone()));
        }
        surface.damage_buffer(0, 0, width, height);
        buffer
            .attach_to(surface)
            .context("attach shared-memory buffer")?;
        self.layer.commit();
        Ok(())
    }

    fn update_hover(&mut self) {
        let hovered = self
            .pointer_position
            .and_then(|(x, y)| element::hit_test(&self.layout, x, y))
            .map(str::to_owned);
        if hovered != self.hovered {
            self.hovered = hovered;
            self.redraw();
        }
    }

    /// True while the desktop-entry index has not published yet.
    fn scanning(&self) -> bool {
        self.apps.as_ref().is_some_and(Apps::is_scanning)
    }

    fn poll_apps(&mut self) {
        let outcome = self.apps.as_mut().map(Apps::poll);
        match outcome {
            Some(Ok(true)) => self.redraw(),
            Some(Ok(false)) => {}
            Some(Err(error)) => self.error = Some(error),
            None => {}
        }
    }

    fn activate_selection(&mut self) {
        let outcome = self.apps.as_mut().map(Apps::activate);
        match outcome {
            Some(Ok(true)) => self.exit = true,
            Some(Ok(false)) => {
                self.status = Some("no application matches this query".to_owned());
                self.redraw();
            }
            Some(Err(error)) => {
                self.status = Some(error.to_string());
                self.redraw();
            }
            None => {}
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let (raw, ctrl, shift) = (
            event.keysym.raw(),
            self.modifiers.ctrl,
            self.modifiers.shift,
        );
        if self.handle_menu_key(raw) {
            return;
        }
        if raw == xkeysym::key::Escape {
            self.exit = true;
            return;
        }
        if matches!(raw, xkeysym::key::Return | xkeysym::key::KP_Enter) {
            self.activate_selection();
            return;
        }
        let Some(apps) = self.apps.as_mut() else {
            return;
        };
        // Any further key means the previous launch error no longer describes state.
        self.status = None;
        match raw {
            xkeysym::key::BackSpace => apps.backspace(ctrl),
            xkeysym::key::Delete => apps.clear_query(),
            xkeysym::key::u if ctrl => apps.clear_query(),
            xkeysym::key::Up => apps.select(-1),
            xkeysym::key::Down => apps.select(1),
            xkeysym::key::Page_Up => apps.page_by(-1),
            xkeysym::key::Page_Down => apps.page_by(1),
            xkeysym::key::Home => apps.select_index(0),
            xkeysym::key::End => apps.select_last(),
            xkeysym::key::Tab => apps.select(if shift { -1 } else { 1 }),
            xkeysym::key::ISO_Left_Tab => apps.select(-1),
            _ => {
                let Some(typed) = event
                    .utf8
                    .as_deref()
                    .filter(|text| !text.chars().any(char::is_control))
                else {
                    return;
                };
                apps.type_text(typed);
            }
        }
        self.redraw();
    }

    fn handle_click(&mut self, position: (f64, f64), button: u32) {
        let hit = element::hit_test(&self.layout, position.0, position.1).map(str::to_owned);
        if button == BTN_RIGHT {
            if let Some(address) = hit.as_deref().and_then(|id| id.strip_prefix("tray:")) {
                let address = address.to_owned();
                self.open_menu(&address);
                if self.menu.is_none() {
                    if let Some(tray) = &self.tray {
                        tray.context_menu(address, position.0 as i32, position.1 as i32);
                    }
                } else {
                    self.redraw_menu();
                }
                return;
            }
            if self.apps.is_some() {
                self.exit = true;
            }
            return;
        }
        if button != BTN_LEFT {
            return;
        }
        if let Some(ws_id) = hit.as_deref().and_then(|id| id.strip_prefix("ws:")) {
            self.taskbar.activate_workspace(ws_id);
            return;
        }
        if let Some(address) = hit.as_deref().and_then(|id| id.strip_prefix("tray:")) {
            if let Some(tray) = &self.tray {
                tray.activate(address.to_owned(), position.0 as i32, position.1 as i32);
            }
            return;
        }
        if hit.as_deref() == Some("audio") {
            if let Some(audio) = &self.audio {
                audio.toggle_mute();
            }
            return;
        }
        let app_row = hit.as_deref().and_then(|id| id.strip_prefix(ROW_PREFIX));
        let moved = match (app_row, &mut self.apps) {
            (Some(id), Some(apps)) => apps.select_id(id),
            _ => false,
        };
        if moved {
            self.redraw();
        }
        if app_row.is_some() {
            self.activate_selection();
        }
    }

    fn handle_scroll(&mut self, vertical: &AxisScroll) {
        let steps = vertical
            .value120
            .ne(&0)
            .then_some(vertical.value120)
            .or_else(|| (vertical.discrete != 0).then_some(vertical.discrete))
            .or_else(|| (vertical.absolute != 0.0).then_some(vertical.absolute.signum() as i32))
            .unwrap_or(0);
        let notches = if steps.abs() >= 120 {
            steps / 120
        } else {
            steps.signum()
        };
        if self.hovered.as_deref() == Some("audio") {
            if let (Some(audio), Some((step, max_volume))) =
                (&self.audio, find_audio_config(&self.window.root))
            {
                // Positive Wayland axis values scroll down, which lowers volume.
                audio.step_volume(-notches * step as i32, max_volume);
            }
            return;
        }
        if let Some(address) = self
            .hovered
            .as_deref()
            .and_then(|id| id.strip_prefix("tray:"))
        {
            if let Some(tray) = &self.tray {
                tray.scroll(address.to_owned(), -notches);
            }
            return;
        }
        let delta = notches as isize;
        let scrolled = self.apps.as_mut().is_some_and(|apps| {
            apps.select(delta);
            true
        });
        if scrolled {
            self.redraw();
        }
    }

    /// Opens the DBusMenu popup anchored under the clicked tray icon.
    fn open_menu(&mut self, address: &str) {
        self.close_menu();
        let Some(tray) = self.tray.clone() else {
            return;
        };
        let Some(item) = tray
            .snapshot()
            .items
            .into_iter()
            .find(|item| item.address == address)
        else {
            return;
        };
        let Some(anchor) = self
            .layout
            .iter()
            .find(|layout| layout.id.as_deref() == Some(&format!("tray:{address}")))
            .map(|layout| layout.rect)
        else {
            return;
        };
        match self.create_menu(&item, anchor) {
            Ok(Some(menu)) => self.menu = Some(menu),
            Ok(None) => {}
            Err(error) => eprintln!("open tray menu: {error:#}"),
        }
        if let Some(menu) = &self.menu {
            tray.show_menu(menu.address.clone(), menu.menu_path.clone());
        }
    }

    fn create_menu(&mut self, item: &TrayItem, anchor: Rect) -> Result<Option<MenuPopup>> {
        let height = menu_height(&item.menu);
        let positioner = XdgPositioner::new(&self.xdg_shell).context("create popup positioner")?;
        positioner.set_size(
            i32::try_from(MENU_WIDTH).context("menu width overflow")?,
            i32::try_from(height).context("menu height overflow")?,
        );
        positioner.set_anchor_rect(
            anchor.x as i32,
            anchor.y as i32,
            (anchor.width as i32).max(1),
            (anchor.height as i32).max(1),
        );
        positioner.set_anchor(xdg_positioner::Anchor::BottomLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(
            xdg_positioner::ConstraintAdjustment::FlipY
                | xdg_positioner::ConstraintAdjustment::SlideX,
        );

        let surface =
            Surface::new(&self.compositor, &self.queue_handle).context("create popup surface")?;
        let popup = Popup::from_surface(
            None,
            &positioner,
            &self.queue_handle,
            surface,
            &self.xdg_shell,
        )
        .context("create popup")?;
        let Some(menu) = MenuPopup::new(popup, item) else {
            return Ok(None);
        };
        self.layer.get_popup(menu.popup.xdg_popup());
        if let (Some(seat), Some(serial)) = (&self.seat, self.last_serial) {
            menu.popup.xdg_popup().grab(seat, serial);
        }
        menu.popup.wl_surface().commit();
        Ok(Some(menu))
    }

    fn close_menu(&mut self) {
        self.menu = None;
    }

    fn draw_menu(&mut self) -> Result<()> {
        let Some(menu) = &self.menu else {
            return Ok(());
        };
        let (width, height) = (menu.width, menu.height);
        let root = menu_element(menu.items(), menu.state.selected);
        let buffer_width = i32::try_from(width).context("menu width overflow")?;
        let buffer_height = i32::try_from(height).context("menu height overflow")?;
        let stride = buffer_width
            .checked_mul(4)
            .context("menu stride overflow")?;
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                buffer_width,
                buffer_height,
                stride,
                wl_shm::Format::Argb8888,
            )
            .context("create menu buffer")?;
        let layout = element::layout(&root, width, height, &mut |content| {
            self.text.measure(content)
        });
        paint::render(
            canvas,
            width,
            height,
            &layout,
            &mut self.text,
            &mut self.images,
        )?;
        let Some(menu) = &mut self.menu else {
            return Ok(());
        };
        menu.layout = layout;
        let surface = menu.popup.wl_surface();
        surface.damage_buffer(0, 0, buffer_width, buffer_height);
        buffer.attach_to(surface).context("attach menu buffer")?;
        surface.commit();
        Ok(())
    }

    fn redraw_menu(&mut self) {
        if let Err(error) = self.draw_menu() {
            self.error = Some(error);
        }
    }

    /// Returns true when the click was consumed by the open menu.
    fn handle_menu_click(&mut self, position: (f64, f64), button: u32) {
        if button != BTN_LEFT {
            self.close_menu();
            return;
        }
        let Some(menu) = &mut self.menu else {
            return;
        };
        let Some(row) = menu.row_at(position.0, position.1) else {
            self.close_menu();
            return;
        };
        menu.state.selected = row;
        self.activate_menu_row();
    }

    fn handle_menu_motion(&mut self, position: (f64, f64)) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        let Some(row) = menu.row_at(position.0, position.1) else {
            return;
        };
        let selectable = menu
            .items()
            .get(row)
            .is_some_and(|item| item.enabled && !item.separator);
        if selectable && menu.state.selected != row {
            menu.state.selected = row;
            self.redraw_menu();
        }
    }

    /// Activates the selected row: descends into submenus, else sends the D-Bus event.
    fn activate_menu_row(&mut self) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        let Some(item) = menu.items().get(menu.state.selected).cloned() else {
            return;
        };
        if !item.enabled || item.separator {
            return;
        }
        if !item.submenu.is_empty() {
            if menu.enter_submenu() {
                self.redraw_menu();
            }
            return;
        }
        let (address, menu_path) = (menu.address.clone(), menu.menu_path.clone());
        if let Some(tray) = &self.tray {
            tray.activate_menu_item(address, menu_path, item.id);
        }
        self.close_menu();
    }

    /// Returns true when the key was consumed by the open menu.
    fn handle_menu_key(&mut self, raw: u32) -> bool {
        if self.menu.is_none() {
            return false;
        }
        match raw {
            xkeysym::key::Escape => {
                let popped = self.menu.as_mut().is_some_and(MenuPopup::leave_submenu);
                if popped {
                    self.redraw_menu();
                } else {
                    self.close_menu();
                }
            }
            xkeysym::key::Up => {
                if let Some(menu) = &mut self.menu {
                    menu.move_selection(-1);
                }
                self.redraw_menu();
            }
            xkeysym::key::Down => {
                if let Some(menu) = &mut self.menu {
                    menu.move_selection(1);
                }
                self.redraw_menu();
            }
            xkeysym::key::Left => {
                if self.menu.as_mut().is_some_and(MenuPopup::leave_submenu) {
                    self.redraw_menu();
                }
            }
            xkeysym::key::Right => {
                if self.menu.as_mut().is_some_and(MenuPopup::enter_submenu) {
                    self.redraw_menu();
                }
            }
            xkeysym::key::Return | xkeysym::key::KP_Enter | xkeysym::key::space => {
                self.activate_menu_row();
            }
            _ => {}
        }
        true
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (fallback_width, fallback_height) = self.window.initial_size();
        let width = NonZeroU32::new(configure.new_size.0).map_or(fallback_width, NonZeroU32::get);
        let height = NonZeroU32::new(configure.new_size.1).map_or(fallback_height, NonZeroU32::get);
        let resized = (width, height) != (self.width, self.height);
        self.width = width;
        self.height = height;
        if !self.drawn || resized {
            if !self.drawn {
                self.mapped_at = Instant::now();
            }
            self.drawn = true;
            if let Err(error) = self.draw(queue_handle) {
                self.error = Some(error);
            }
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
        self.redraw();
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Err(error) = self.draw(queue_handle) {
            self.error = Some(error);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(&event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(&event);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let on_menu = self
                .menu
                .as_ref()
                .is_some_and(|menu| &event.surface == menu.popup.wl_surface());
            if on_menu {
                match event.kind {
                    PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                        self.handle_menu_motion(event.position);
                    }
                    PointerEventKind::Press { button, serial, .. } => {
                        self.last_serial = Some(serial);
                        self.handle_menu_click(event.position, button);
                    }
                    PointerEventKind::Leave { .. }
                    | PointerEventKind::Release { .. }
                    | PointerEventKind::Axis { .. } => {}
                }
                continue;
            }
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Leave { .. } => self.pointer_position = None,
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_position = Some(event.position);
                }
                PointerEventKind::Press { button, serial, .. } => {
                    self.last_serial = Some(serial);
                    self.handle_click(event.position, button);
                }
                PointerEventKind::Release { .. } => {}
                PointerEventKind::Axis { vertical, .. } => self.handle_scroll(&vertical),
            }
        }
        self.update_hover();
    }
}

impl PopupHandler for State {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        if &menu.popup != popup {
            return;
        }
        menu.width = u32::try_from(configure.width).unwrap_or(menu.width).max(1);
        menu.height = u32::try_from(configure.height)
            .unwrap_or(menu.height)
            .max(1);
        self.redraw_menu();
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self.menu.as_ref().is_some_and(|menu| &menu.popup == popup) {
            self.close_menu();
        }
    }
}

/// xdg-shell's `XdgShell::bind` requires this even though no toplevel is created.
impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &XdgWindow) {}

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &XdgWindow,
        _: WindowConfigure,
        _: u32,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(queue_handle, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(error) => self.error = Some(error.into()),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(queue_handle, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(error) => self.error = Some(error.into()),
            }
        }
        self.seat = Some(seat);
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            self.pointer_position = None;
            self.hovered = None;
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(State);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(State);

impl Dispatch<ext_workspace_manager_v1::ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { .. } => {}
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.taskbar.on_workspace_created(workspace);
            }
            ext_workspace_manager_v1::Event::Done => {
                if state.taskbar.on_workspace_done() {
                    state.redraw();
                }
            }
            ext_workspace_manager_v1::Event::Finished => {}
            _ => {}
        }
    }

    event_created_child!(State, ext_workspace_manager_v1::ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
        _: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ext_workspace_handle_v1::ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let object_id = proxy.id();
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                state.taskbar.on_workspace_name(&object_id, name);
            }
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                let coords: Vec<u32> = coordinates
                    .chunks_exact(4)
                    .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap_or([0; 4])))
                    .collect();
                state.taskbar.on_workspace_coordinates(&object_id, coords);
            }
            ext_workspace_handle_v1::Event::State { state: ws_state } => {
                let (active, urgent, hidden) = match ws_state {
                    WEnum::Value(flags) => (
                        flags.contains(ext_workspace_handle_v1::State::Active),
                        flags.contains(ext_workspace_handle_v1::State::Urgent),
                        flags.contains(ext_workspace_handle_v1::State::Hidden),
                    ),
                    WEnum::Unknown(_) => (false, false, false),
                };
                state
                    .taskbar
                    .on_workspace_state(&object_id, active, urgent, hidden);
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.taskbar.on_workspace_removed(&object_id);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                state.taskbar.on_toplevel_created(toplevel);
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {}
            _ => {}
        }
    }

    event_created_child!(State, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let object_id = proxy.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.taskbar.on_toplevel_title(&object_id, title);
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.taskbar.on_toplevel_app_id(&object_id, app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: top_state } => {
                let mut activated = false;
                let mut maximized = false;
                let mut minimized = false;
                let mut fullscreen = false;
                for chunk in top_state.chunks_exact(4) {
                    let raw = u32::from_ne_bytes(chunk.try_into().unwrap_or([0; 4]));
                    match WEnum::from(raw) {
                        WEnum::Value(zwlr_foreign_toplevel_handle_v1::State::Activated) => {
                            activated = true
                        }
                        WEnum::Value(zwlr_foreign_toplevel_handle_v1::State::Maximized) => {
                            maximized = true
                        }
                        WEnum::Value(zwlr_foreign_toplevel_handle_v1::State::Minimized) => {
                            minimized = true
                        }
                        WEnum::Value(zwlr_foreign_toplevel_handle_v1::State::Fullscreen) => {
                            fullscreen = true
                        }
                        _ => {}
                    }
                }
                state
                    .taskbar
                    .on_toplevel_state(&object_id, activated, maximized, minimized, fullscreen);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if state.taskbar.on_toplevel_done(&object_id) {
                    state.redraw();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed
                if state.taskbar.on_toplevel_closed(&object_id) =>
            {
                state.redraw();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::panic, reason = "assertion failure in tests")]

    use super::*;
    use crate::ui::element::Element;

    #[test]
    fn next_second_should_be_positive_and_at_most_one_second() {
        let delay = next_second();
        assert!(delay > Duration::ZERO);
        assert!(delay <= Duration::from_secs(1));
    }

    #[test]
    fn contains_clock_should_search_descendants() {
        let mut root = Element::new(Style::panel(Color::TRANSPARENT));
        root.children.push(Element {
            content: Content::Clock {
                text: crate::ui::element::Text {
                    value: String::new(),
                    color: Color::TRANSPARENT,
                    font_size: 14,
                    font_family: String::new(),
                    align: crate::ui::element::Align::Start,
                },
                format: "%H:%M".to_owned(),
            },
            ..Element::new(Style::panel(Color::TRANSPARENT))
        });

        assert!(contains_clock(&root));
    }

    fn styled(value: &str) -> Text {
        Text {
            value: value.to_owned(),
            color: Color::rgba(0xff, 0xff, 0xff, 0xff),
            font_size: 14,
            font_family: String::new(),
            align: Align::Start,
        }
    }

    fn launcher_tree(rows: u32, terminal: Vec<String>) -> Element {
        Element {
            direction: Direction::Column,
            children: vec![
                Element {
                    content: Content::AppSearch {
                        text: styled("Search applications\u{2026}"),
                    },
                    ..Element::new(Style::panel(Color::TRANSPARENT))
                },
                Element {
                    content: Content::AppList {
                        text: styled("row"),
                        selected: styled("active"),
                        select_background: Color::rgba(0x11, 0x22, 0x33, 0x44),
                        rows,
                        terminal,
                    },
                    ..Element::new(Style::panel(Color::TRANSPARENT))
                },
            ],
            ..Element::new(Style::panel(Color::TRANSPARENT))
        }
    }

    fn entry(name: &str) -> crate::platform::linux::apps::App {
        crate::platform::linux::apps::App {
            id: name.to_owned(),
            name: name.to_owned(),
            generic_name: String::new(),
            comment: String::new(),
            keywords: String::new(),
            executable: vec!["/bin/true".to_owned()],
            terminal: false,
            icon: String::new(),
            path: std::path::PathBuf::from(format!("/tmp/{name}.desktop")),
        }
    }

    fn indexed(names: &[&str]) -> Apps {
        let mut apps = Apps::from_index(names.iter().map(|name| entry(name)).collect());
        apps.set_visible(2);
        apps
    }

    #[test]
    fn launcher_config_should_read_the_app_list() {
        let tree = launcher_tree(3, vec!["kitty".to_owned(), "-e".to_owned()]);

        assert_eq!(
            launcher_config(&tree),
            Some((3, vec!["kitty".to_owned(), "-e".to_owned()],))
        );
    }

    #[test]
    fn launcher_config_should_ignore_other_trees() {
        let tree = Element {
            children: vec![Element {
                content: Content::Clock {
                    text: styled(""),
                    format: "%H:%M".to_owned(),
                },
                ..Element::new(Style::panel(Color::TRANSPARENT))
            }],
            ..Element::new(Style::panel(Color::TRANSPARENT))
        };

        assert_eq!(launcher_config(&tree), None);
    }

    #[test]
    fn expand_should_draw_visible_rows_and_mark_selection() {
        // Empty-query order is alphabetical, so rows 0 and 1 hold alpha and beta.
        let apps = indexed(&["alpha", "beta", "gamma"]);
        let tree = launcher_tree(2, Vec::new());
        let taskbar = Taskbar::default();

        let list = &expand(&tree, Some(&apps), &taskbar, None, None, None).children[1];
        assert_eq!(list.children.len(), 2);
        assert_eq!(list.children[0].id.as_deref(), Some("app:alpha"));
        assert_eq!(list.children[1].id.as_deref(), Some("app:beta"));

        let Content::Text(active) = &list.children[0].content else {
            panic!("rows should carry text");
        };
        assert!(
            active.value.starts_with('▸'),
            "the selected row needs a non-colour marker, found `{}`",
            active.value
        );
        assert_eq!(
            list.children[0].style.background,
            Color::rgba(0x11, 0x22, 0x33, 0x44)
        );

        let Content::Text(plain) = &list.children[1].content else {
            panic!("rows should carry text");
        };
        assert!(
            plain.value.starts_with("  "),
            "unselected rows must not carry the marker"
        );
        assert_eq!(list.children[1].style.background, Color::TRANSPARENT);
    }

    #[test]
    fn expand_should_show_query_caret_and_launch_failures() {
        let mut apps = indexed(&["one"]);
        let tree = launcher_tree(2, Vec::new());
        let taskbar = Taskbar::default();

        let Content::Text(placeholder) =
            &expand(&tree, Some(&apps), &taskbar, None, None, None).children[0].children[0].content
        else {
            panic!("the search box should hold one text child");
        };
        assert_eq!(placeholder.value, "Search applications\u{2026}");

        apps.type_text("on");
        let Content::Text(query) =
            &expand(&tree, Some(&apps), &taskbar, None, None, None).children[0].children[0].content
        else {
            panic!("the search box should hold one text child");
        };
        assert_eq!(query.value, format!("on{CARET}"));

        let list = expand(
            &tree,
            Some(&apps),
            &taskbar,
            None,
            None,
            Some("launch failed"),
        )
        .children[1]
            .clone();
        let Content::Text(status) = &list.children[0].content else {
            panic!("a failure should render as text");
        };
        assert_eq!(status.value, format!("{FAILURE} launch failed"));
        assert_eq!(list.children.len(), 2, "rows follow the status line");
    }

    #[test]
    fn expand_should_render_active_window_title_and_workspaces() {
        let mut taskbar = Taskbar::default();
        taskbar.toplevels.toplevel_created("top_1".into());
        taskbar
            .toplevels
            .set_title("top_1", "Firefox Nightly - Rust".into());
        taskbar
            .toplevels
            .set_state("top_1", true, false, false, false);
        taskbar.toplevels.commit_toplevel("top_1");

        taskbar.workspaces.workspace_created("ws_1".into());
        taskbar.workspaces.set_name("ws_1", "1".into());
        taskbar.workspaces.set_state("ws_1", true, false, false);
        taskbar.workspaces.commit_done();

        let title_elem = Element {
            content: Content::ActiveWindow {
                text: styled("title"),
                max_chars: Some(10),
                empty_value: "Desktop".into(),
            },
            ..Element::new(Style::panel(Color::TRANSPARENT))
        };
        let ws_elem = Element {
            content: Content::Workspaces {
                text: styled("ws"),
                active: styled("ws_act"),
                active_background: Color::rgba(0xff, 0xff, 0xff, 0x44),
                urgent: styled("ws_urg"),
                urgent_background: Color::rgba(0xff, 0x00, 0x00, 0x44),
                gap: 6,
            },
            ..Element::new(Style::panel(Color::TRANSPARENT))
        };

        let expanded_title = expand(&title_elem, None, &taskbar, None, None, None);
        let Content::Text(text) = &expanded_title.content else {
            panic!("expanded ActiveWindow should have Text content");
        };
        assert_eq!(text.value, "Firefox Ni…");

        let expanded_ws = expand(&ws_elem, None, &taskbar, None, None, None);
        assert_eq!(expanded_ws.children.len(), 1);
        assert_eq!(expanded_ws.children[0].id.as_deref(), Some("ws:ws_1"));
        assert_eq!(
            expanded_ws.children[0].style.background,
            Color::rgba(0xff, 0xff, 0xff, 0x44)
        );
    }

    #[test]
    fn search_line_should_report_an_empty_index() {
        let apps = indexed(&[] as &[&str]);

        assert_eq!(
            search_line(&styled("placeholder"), &apps).value,
            "no applications found"
        );
    }

    #[test]
    fn animation_with_hold_should_wake_at_slide_out() {
        let mut window = crate::platform::linux::window::Window::bar();
        window.animation = Some(crate::ui::animation::Animation {
            slide: crate::ui::animation::Slide::Right,
            distance: 400,
            duration_ms: 220,
            hold_ms: Some(4_000),
        });

        assert_eq!(
            animation_wakeup(&window),
            Some(Duration::from_millis(4_220))
        );
    }

    fn menu_item(id: i32, label: &str) -> TrayMenuItem {
        TrayMenuItem {
            id,
            label: label.to_owned(),
            enabled: true,
            separator: false,
            toggle: None,
            submenu: Vec::new(),
        }
    }

    fn separator() -> TrayMenuItem {
        TrayMenuItem {
            separator: true,
            enabled: false,
            ..menu_item(-1, "")
        }
    }

    #[test]
    fn menu_state_should_start_on_the_first_selectable_row() {
        let state = MenuState::new(vec![separator(), menu_item(1, "Open")]);

        assert_eq!(state.selected, 1);
    }

    #[test]
    fn menu_state_should_skip_separators_and_wrap() {
        let mut state = MenuState::new(vec![menu_item(1, "One"), separator(), menu_item(2, "Two")]);

        state.move_selection(1);
        assert_eq!(state.selected, 2);

        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn menu_state_should_skip_disabled_rows() {
        let disabled = TrayMenuItem {
            enabled: false,
            ..menu_item(2, "Disabled")
        };
        let mut state = MenuState::new(vec![menu_item(1, "One"), disabled, menu_item(3, "Three")]);

        state.move_selection(1);

        assert_eq!(state.selected, 2);
    }

    #[test]
    fn menu_state_should_push_and_pop_submenu_levels() {
        let parent = TrayMenuItem {
            submenu: vec![menu_item(10, "Child")],
            ..menu_item(1, "Parent")
        };
        let mut state = MenuState::new(vec![parent]);

        assert!(state.enter_submenu());
        assert_eq!(state.items(), [menu_item(10, "Child")]);

        assert!(state.leave_submenu());
        assert_eq!(state.selected, 0);
        assert!(!state.leave_submenu());
    }

    #[test]
    fn menu_state_should_refuse_to_enter_a_leaf_row() {
        let mut state = MenuState::new(vec![menu_item(1, "Leaf")]);

        assert!(!state.enter_submenu());
    }

    #[test]
    fn menu_element_should_mark_toggles_submenus_and_selection() {
        let items = vec![
            TrayMenuItem {
                toggle: Some(true),
                ..menu_item(1, "Checked")
            },
            TrayMenuItem {
                submenu: vec![menu_item(10, "Child")],
                ..menu_item(2, "Parent")
            },
        ];

        let element = menu_element(&items, 1);

        let labels = element
            .children
            .iter()
            .map(|child| match &child.content {
                Content::Text(text) => text.value.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, ["\u{2713} Checked", "Parent  \u{203a}"]);
        assert_eq!(element.children[1].style.background, MENU_SELECTED);
    }

    #[test]
    fn menu_height_should_cover_padding_and_rows() {
        assert_eq!(
            menu_height(&[menu_item(1, "One"), menu_item(2, "Two")]),
            MENU_PADDING * 2 + MENU_ROW_HEIGHT * 2
        );
    }
}
