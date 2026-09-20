//! StatusNotifier backend running independently from Wayland dispatch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;
use system_tray::client::{ActivateRequest, Client, Event};
use system_tray::item::{IconPixmap, Status, StatusNotifierItem};
use system_tray::menu::{MenuItem, MenuType, ToggleState, ToggleType, TrayMenu};
use tokio::sync::{broadcast, mpsc};
use zbus::proxy;

use crate::platform::linux::runtime::BackendEvent;

#[proxy(interface = "org.kde.StatusNotifierItem", assume_defaults = true)]
trait NotifierItem {
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;
}

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// One display-ready DBusMenu item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayMenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub toggle: Option<bool>,
    pub submenu: Vec<TrayMenuItem>,
}

impl TrayMenuItem {
    fn from_menu(item: &MenuItem) -> Option<Self> {
        item.visible.then(|| Self {
            id: item.id,
            label: menu_label(item.label.as_deref().unwrap_or_default()),
            enabled: item.enabled,
            separator: item.menu_type == MenuType::Separator,
            toggle: match item.toggle_type {
                ToggleType::Checkmark | ToggleType::Radio => {
                    Some(item.toggle_state == ToggleState::On)
                }
                ToggleType::CannotBeToggled => None,
            },
            submenu: item.submenu.iter().filter_map(Self::from_menu).collect(),
        })
    }
}

fn menu_label(label: &str) -> String {
    let mut chars = label.chars().peekable();
    let mut result = String::new();
    while let Some(character) = chars.next() {
        if character == '_' {
            if chars.peek() == Some(&'_') {
                result.push('_');
                chars.next();
            }
        } else {
            result.push(character);
        }
    }
    result
}

fn menu_items(menu: Option<&TrayMenu>) -> Vec<TrayMenuItem> {
    menu.into_iter()
        .flat_map(|menu| &menu.submenus)
        .filter_map(TrayMenuItem::from_menu)
        .collect()
}

/// UI-owned representation of one StatusNotifier item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    pub address: String,
    pub id: String,
    pub title: String,
    pub status: Status,
    pub icon_theme_path: Option<String>,
    pub icon_name: Option<String>,
    pub icon_pixmaps: Vec<IconPixmap>,
    pub item_is_menu: bool,
    pub menu_path: Option<String>,
    pub menu: Vec<TrayMenuItem>,
}

impl TrayItem {
    fn from_notifier(address: String, item: &StatusNotifierItem, menu: Option<&TrayMenu>) -> Self {
        let attention = item.status == Status::NeedsAttention;
        Self {
            address,
            id: item.id.clone(),
            title: item.title.clone().unwrap_or_else(|| item.id.clone()),
            status: item.status,
            icon_theme_path: item.icon_theme_path.clone(),
            icon_name: if attention {
                item.attention_icon_name
                    .clone()
                    .or_else(|| item.icon_name.clone())
            } else {
                item.icon_name.clone()
            },
            icon_pixmaps: if attention {
                item.attention_icon_pixmap
                    .clone()
                    .or_else(|| item.icon_pixmap.clone())
                    .unwrap_or_default()
            } else {
                item.icon_pixmap.clone().unwrap_or_default()
            },
            item_is_menu: item.item_is_menu,
            menu_path: item.menu.clone(),
            menu: menu_items(menu),
        }
    }
}

/// Stable tray view published to the Wayland thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraySnapshot {
    pub items: Vec<TrayItem>,
}

#[derive(Debug)]
enum TrayCommand {
    Activate {
        address: String,
        x: i32,
        y: i32,
    },
    ContextMenu {
        address: String,
        x: i32,
        y: i32,
    },
    SecondaryActivate {
        address: String,
        x: i32,
        y: i32,
    },
    Scroll {
        address: String,
        delta: i32,
    },
    ShowMenu {
        address: String,
        menu_path: String,
    },
    ActivateMenuItem {
        address: String,
        menu_path: String,
        id: i32,
    },
}

#[derive(Debug)]
enum TrayEvent {
    Upsert(TrayItem),
    Remove(String),
}

#[derive(Default)]
struct TrayReducer {
    items: HashMap<String, TrayItem>,
}

impl TrayReducer {
    fn replace(&mut self, items: impl IntoIterator<Item = TrayItem>) {
        self.items = items
            .into_iter()
            .map(|item| (item.address.clone(), item))
            .collect();
    }

    fn apply(&mut self, event: TrayEvent) {
        match event {
            TrayEvent::Upsert(item) => {
                self.items.insert(item.address.clone(), item);
            }
            TrayEvent::Remove(address) => {
                self.items.remove(&address);
            }
        }
    }

    fn snapshot(&self) -> TraySnapshot {
        let mut items = self.items.values().cloned().collect::<Vec<_>>();
        items.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.address.cmp(&right.address))
        });
        TraySnapshot { items }
    }
}

/// Handle used by UI thread to read tray state and send activation requests.
#[derive(Clone)]
pub struct TrayClient {
    command_tx: mpsc::UnboundedSender<TrayCommand>,
    snapshot: Arc<Mutex<TraySnapshot>>,
}

impl TrayClient {
    pub fn start(backend_sender: CalloopSender<BackendEvent>) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(TraySnapshot::default()));
        let worker_snapshot = Arc::clone(&snapshot);

        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-tray".into())
            .spawn(move || run_worker(command_rx, worker_snapshot, backend_sender))
        {
            eprintln!("start status notifier worker: {error}");
        }

        Self {
            command_tx,
            snapshot,
        }
    }

    pub fn snapshot(&self) -> TraySnapshot {
        self.snapshot
            .lock()
            .map_or_else(|_| TraySnapshot::default(), |guard| guard.clone())
    }

    pub fn activate(&self, address: String, x: i32, y: i32) {
        let _ = self
            .command_tx
            .send(TrayCommand::Activate { address, x, y });
    }

    pub fn context_menu(&self, address: String, x: i32, y: i32) {
        let _ = self
            .command_tx
            .send(TrayCommand::ContextMenu { address, x, y });
    }

    /// Middle click; waybar and friends map this to `SecondaryActivate`.
    pub fn secondary_activate(&self, address: String, x: i32, y: i32) {
        let _ = self
            .command_tx
            .send(TrayCommand::SecondaryActivate { address, x, y });
    }

    pub fn scroll(&self, address: String, delta: i32) {
        let _ = self.command_tx.send(TrayCommand::Scroll { address, delta });
    }

    pub fn show_menu(&self, address: String, menu_path: String) {
        let _ = self
            .command_tx
            .send(TrayCommand::ShowMenu { address, menu_path });
    }

    pub fn activate_menu_item(&self, address: String, menu_path: String, id: i32) {
        let _ = self.command_tx.send(TrayCommand::ActivateMenuItem {
            address,
            menu_path,
            id,
        });
    }
}

fn run_worker(
    command_rx: mpsc::UnboundedReceiver<TrayCommand>,
    snapshot: Arc<Mutex<TraySnapshot>>,
    backend_sender: CalloopSender<BackendEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("create status notifier runtime: {error}");
            return;
        }
    };
    runtime.block_on(connection_loop(command_rx, snapshot, backend_sender));
}

async fn connection_loop(
    mut command_rx: mpsc::UnboundedReceiver<TrayCommand>,
    snapshot: Arc<Mutex<TraySnapshot>>,
    backend_sender: CalloopSender<BackendEvent>,
) {
    let mut reported_unavailable = false;
    loop {
        match run_connection(&mut command_rx, &snapshot, &backend_sender).await {
            Ok(()) => return,
            Err(error) => {
                if !reported_unavailable {
                    eprintln!("status notifier unavailable: {error:#}");
                    reported_unavailable = true;
                }
                publish(&snapshot, &backend_sender, TraySnapshot::default());
                tokio::select! {
                    () = tokio::time::sleep(RECONNECT_DELAY) => {}
                    command = command_rx.recv() => {
                        if command.is_none() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

async fn run_connection(
    command_rx: &mut mpsc::UnboundedReceiver<TrayCommand>,
    snapshot: &Arc<Mutex<TraySnapshot>>,
    backend_sender: &CalloopSender<BackendEvent>,
) -> Result<()> {
    let client = Client::new().await.context("connect to session D-Bus")?;
    let connection = zbus::Connection::session()
        .await
        .context("open status notifier command connection")?;
    let mut events = client.subscribe();
    let mut reducer = TrayReducer::default();
    reducer.replace(read_items(&client)?);
    publish(snapshot, backend_sender, reducer.snapshot());

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Add(address, item)) => {
                    reducer.apply(TrayEvent::Upsert(TrayItem::from_notifier(address, &item, None)));
                    publish(snapshot, backend_sender, reducer.snapshot());
                }
                Ok(Event::Update(address, _)) => {
                    if let Some(item) = read_item(&client, &address)? {
                        reducer.apply(TrayEvent::Upsert(item));
                        publish(snapshot, backend_sender, reducer.snapshot());
                    }
                }
                Ok(Event::Remove(address)) => {
                    reducer.apply(TrayEvent::Remove(address));
                    publish(snapshot, backend_sender, reducer.snapshot());
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    reducer.replace(read_items(&client)?);
                    publish(snapshot, backend_sender, reducer.snapshot());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!("status notifier event stream closed"));
                }
            },
            command = command_rx.recv() => {
                let Some(command) = command else {
                    return Ok(());
                };
                if let Err(error) = activate(&client, &connection, command).await {
                    eprintln!("activate status notifier item: {error:#}");
                }
            }
        }
    }
}

fn read_items(client: &Client) -> Result<Vec<TrayItem>> {
    let items = client.items();
    let guard = items
        .lock()
        .map_err(|_| anyhow!("status notifier item cache lock poisoned"))?;
    Ok(guard
        .iter()
        .map(|(address, (item, menu))| {
            TrayItem::from_notifier(address.clone(), item, menu.as_ref())
        })
        .collect())
}

fn read_item(client: &Client, address: &str) -> Result<Option<TrayItem>> {
    let items = client.items();
    let guard = items
        .lock()
        .map_err(|_| anyhow!("status notifier item cache lock poisoned"))?;
    Ok(guard
        .get(address)
        .map(|(item, menu)| TrayItem::from_notifier(address.to_owned(), item, menu.as_ref())))
}

async fn activate(
    client: &Client,
    connection: &zbus::Connection,
    command: TrayCommand,
) -> Result<()> {
    match command {
        TrayCommand::Activate { address, x, y } => client
            .activate(ActivateRequest::Default { address, x, y })
            .await
            .context("send D-Bus activation"),
        TrayCommand::ContextMenu { address, x, y } => notifier_proxy(connection, address)
            .await?
            .context_menu(x, y)
            .await
            .context("open StatusNotifier context menu"),
        TrayCommand::SecondaryActivate { address, x, y } => notifier_proxy(connection, address)
            .await?
            .secondary_activate(x, y)
            .await
            .context("secondary activate StatusNotifier item"),
        TrayCommand::Scroll { address, delta } => notifier_proxy(connection, address)
            .await?
            .scroll(delta, "vertical")
            .await
            .context("scroll StatusNotifier item"),
        TrayCommand::ShowMenu { address, menu_path } => client
            .about_to_show_menuitem(address, menu_path, 0)
            .await
            .map(|_| ())
            .context("refresh DBusMenu root"),
        TrayCommand::ActivateMenuItem {
            address,
            menu_path,
            id,
        } => client
            .activate(ActivateRequest::MenuItem {
                address,
                menu_path,
                submenu_id: id,
            })
            .await
            .context("activate DBusMenu item"),
    }
}

async fn notifier_proxy(
    connection: &zbus::Connection,
    address: String,
) -> Result<NotifierItemProxy<'_>> {
    let (destination, path) = address.split_once('/').map_or(
        (address.as_str(), "/StatusNotifierItem"),
        |(destination, path)| (destination, path),
    );
    NotifierItemProxy::builder(connection)
        .destination(destination.to_owned())?
        .path(format!("/{}", path.trim_start_matches('/')))?
        .build()
        .await
        .context("create StatusNotifier proxy")
}

fn publish(
    target: &Arc<Mutex<TraySnapshot>>,
    sender: &CalloopSender<BackendEvent>,
    next: TraySnapshot,
) {
    let changed = target.lock().is_ok_and(|mut current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
    if changed {
        let _ = sender.send(BackendEvent::Redraw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(address: &str, title: &str) -> TrayItem {
        TrayItem {
            address: address.to_owned(),
            id: title.to_lowercase(),
            title: title.to_owned(),
            status: Status::Active,
            icon_theme_path: None,
            icon_name: None,
            icon_pixmaps: Vec::new(),
            item_is_menu: false,
            menu_path: None,
            menu: Vec::new(),
        }
    }

    #[test]
    fn reducer_should_add_update_and_sort_items() {
        let mut reducer = TrayReducer::default();
        reducer.apply(TrayEvent::Upsert(item("second", "Zulu")));
        reducer.apply(TrayEvent::Upsert(item("first", "Alpha")));
        reducer.apply(TrayEvent::Upsert(item("second", "Beta")));

        assert_eq!(
            reducer
                .snapshot()
                .items
                .into_iter()
                .map(|item| item.title)
                .collect::<Vec<_>>(),
            ["Alpha", "Beta"]
        );
    }

    #[test]
    fn reducer_should_remove_an_item() {
        let mut reducer = TrayReducer::default();
        reducer.apply(TrayEvent::Upsert(item("one", "One")));
        reducer.apply(TrayEvent::Remove("one".to_owned()));

        assert!(reducer.snapshot().items.is_empty());
    }

    #[test]
    fn reducer_should_replace_stale_state_after_lag() {
        let mut reducer = TrayReducer::default();
        reducer.apply(TrayEvent::Upsert(item("old", "Old")));
        reducer.replace([item("new", "New")]);

        assert_eq!(reducer.snapshot().items, [item("new", "New")]);
    }

    #[test]
    fn menu_label_should_drop_mnemonics_and_unescape_underscores() {
        assert_eq!(menu_label("_Quit"), "Quit");
        assert_eq!(menu_label("Save __as"), "Save _as");
    }

    #[test]
    fn menu_items_should_skip_hidden_entries_and_keep_submenus() {
        let hidden = MenuItem::default();
        let child = MenuItem {
            id: 2,
            visible: true,
            label: Some("_Child".to_owned()),
            ..MenuItem::default()
        };
        let parent = MenuItem {
            id: 1,
            visible: true,
            label: Some("Parent".to_owned()),
            submenu: vec![child],
            ..MenuItem::default()
        };
        let menu = TrayMenu {
            id: 0,
            submenus: vec![hidden, parent],
        };

        let items = menu_items(Some(&menu));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].submenu[0].label, "Child");
    }

    #[test]
    fn menu_items_should_map_checkmarks_to_toggle_state() {
        let checked = MenuItem {
            id: 1,
            visible: true,
            toggle_type: ToggleType::Checkmark,
            toggle_state: ToggleState::On,
            ..MenuItem::default()
        };
        let menu = TrayMenu {
            id: 0,
            submenus: vec![checked],
        };

        assert_eq!(menu_items(Some(&menu))[0].toggle, Some(true));
    }
}
