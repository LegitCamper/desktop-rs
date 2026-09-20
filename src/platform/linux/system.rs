use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::platform::linux::runtime::BackendEvent;

#[derive(Clone)]
pub struct PollClient<T> {
    snapshot: Arc<Mutex<T>>,
}

impl<T> PollClient<T>
where
    T: Clone + PartialEq + Send + 'static,
{
    pub fn start(
        name: &str,
        interval: Duration,
        initial: T,
        sender: CalloopSender<BackendEvent>,
        mut query: impl FnMut() -> T + Send + 'static,
    ) -> Self {
        let snapshot = Arc::new(Mutex::new(initial));
        let worker_snapshot = Arc::clone(&snapshot);
        let thread_name = format!("desktop-rs-{name}");
        if let Err(error) = thread::Builder::new().name(thread_name).spawn(move || {
            loop {
                let next = query();
                let changed = worker_snapshot.lock().is_ok_and(|mut current| {
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
                thread::sleep(interval);
            }
        }) {
            eprintln!("start {name} worker: {error}");
        }
        Self { snapshot }
    }

    pub fn snapshot(&self) -> T {
        self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |value| value.clone(),
        )
    }

    /// Overwrites the snapshot between polls, so a user action shows immediately
    /// instead of waiting out the poll interval. The next poll corrects it.
    pub fn publish(&self, next: T) {
        if let Ok(mut current) = self.snapshot.lock() {
            *current = next;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatterySnapshot {
    pub capacity: u32,
    pub status: String,
    pub online: bool,
    pub available: bool,
}

pub fn battery_snapshot() -> BatterySnapshot {
    battery_snapshot_at(Path::new("/sys/class/power_supply"))
}

fn battery_snapshot_at(root: &Path) -> BatterySnapshot {
    let Ok(entries) = fs::read_dir(root) else {
        return BatterySnapshot::default();
    };
    let supplies = entries.filter_map(Result::ok).map(|entry| entry.path());
    let mut capacities = Vec::new();
    let mut status = String::new();
    let mut online = false;
    for path in supplies {
        let supply_type = read_trimmed(path.join("type"));
        if supply_type.as_deref() != Some("Battery") {
            online |= read_trimmed(path.join("online")).as_deref() == Some("1");
            continue;
        }
        if let Some(capacity) =
            read_trimmed(path.join("capacity")).and_then(|value| value.parse::<u32>().ok())
        {
            capacities.push(capacity.min(100));
            if status.is_empty() {
                status = read_trimmed(path.join("status")).unwrap_or_default();
            }
        }
    }
    if capacities.is_empty() {
        return BatterySnapshot::default();
    }
    let capacity = capacities.iter().copied().sum::<u32>() / capacities.len() as u32;
    BatterySnapshot {
        capacity,
        online: online || !status.eq_ignore_ascii_case("discharging"),
        status,
        available: true,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BacklightSnapshot {
    pub percent: u32,
    pub available: bool,
}

pub fn backlight_snapshot() -> BacklightSnapshot {
    backlight_snapshot_at(Path::new("/sys/class/backlight"))
}

/// First backlight device: its sysfs name, current raw value, and maximum.
struct Backlight {
    name: String,
    raw: u64,
    maximum: u64,
}

fn backlight_at(root: &Path) -> Option<Backlight> {
    let path = first_directory(root)?;
    let raw = read_trimmed(path.join("actual_brightness"))
        .or_else(|| read_trimmed(path.join("brightness")))
        .and_then(|v| v.parse::<u64>().ok())?;
    let maximum = read_trimmed(path.join("max_brightness"))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|maximum| *maximum > 0)?;
    Some(Backlight {
        name: path.file_name()?.to_string_lossy().into_owned(),
        raw,
        maximum,
    })
}

fn backlight_snapshot_at(root: &Path) -> BacklightSnapshot {
    let Some(device) = backlight_at(root) else {
        return BacklightSnapshot::default();
    };
    BacklightSnapshot {
        percent: percent_of(device.raw, device.maximum),
        available: true,
    }
}

fn percent_of(raw: u64, maximum: u64) -> u32 {
    u32::try_from(raw.saturating_mul(100) / maximum.max(1))
        .unwrap_or(100)
        .min(100)
}

/// Raw value `delta` percentage points away from `raw`, floored at 1% so a wheel
/// down cannot black out the screen with no way to scroll back up.
fn stepped_raw(raw: u64, maximum: u64, delta: i32) -> u64 {
    let percent = i64::from(percent_of(raw, maximum)) + i64::from(delta);
    let percent = percent.clamp(1, 100) as u64;
    // Round to nearest so small steps still move a low-resolution device.
    (percent * maximum).div_ceil(100).min(maximum)
}

/// Steps brightness by `delta` percentage points through logind, which grants
/// the active session write access that `/sys/class/backlight` itself denies.
pub fn step_backlight(delta: i32) -> Result<BacklightSnapshot> {
    let device = backlight_at(Path::new("/sys/class/backlight")).context("no backlight device")?;
    let raw = stepped_raw(device.raw, device.maximum, delta);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build backlight runtime")?;
    runtime.block_on(set_brightness(&device.name, raw))?;
    Ok(BacklightSnapshot {
        percent: percent_of(raw, device.maximum),
        available: true,
    })
}

async fn set_brightness(device: &str, raw: u64) -> Result<()> {
    let connection = zbus::Connection::system()
        .await
        .context("connect to system D-Bus")?;
    proxy(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/session/self",
        "org.freedesktop.login1.Session",
    )
    .await?
    .call_method(
        "SetBrightness",
        &("backlight", device, u32::try_from(raw).unwrap_or(u32::MAX)),
    )
    .await
    .context("call logind SetBrightness")?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkSnapshot {
    pub interface: String,
    pub ssid: String,
    pub signal: u8,
    pub ipv4: String,
    pub prefix: u32,
    pub linked: bool,
    pub available: bool,
}

pub fn start_network(
    interval: Duration,
    sender: CalloopSender<BackendEvent>,
) -> PollClient<NetworkSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    PollClient::start(
        "network",
        interval,
        NetworkSnapshot::default(),
        sender,
        move || {
            let Ok(runtime) = &runtime else {
                return NetworkSnapshot::default();
            };
            runtime.block_on(query_network()).unwrap_or_default()
        },
    )
}

async fn query_network() -> Result<NetworkSnapshot> {
    let connection = zbus::Connection::system()
        .await
        .context("connect to system D-Bus")?;
    let manager = proxy(
        &connection,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    let primary: OwnedObjectPath = manager.get_property("PrimaryConnection").await?;
    if primary.as_str() == "/" {
        return Ok(NetworkSnapshot {
            available: true,
            ..NetworkSnapshot::default()
        });
    }
    let active = proxy(
        &connection,
        "org.freedesktop.NetworkManager",
        primary.as_str(),
        "org.freedesktop.NetworkManager.Connection.Active",
    )
    .await?;
    let state: u32 = active.get_property("State").await.unwrap_or_default();
    let devices: Vec<OwnedObjectPath> = active.get_property("Devices").await.unwrap_or_default();
    let ip_path: OwnedObjectPath = active
        .get_property("Ip4Config")
        .await
        .unwrap_or_else(|_| root_path());
    let specific: OwnedObjectPath = active
        .get_property("SpecificObject")
        .await
        .unwrap_or_else(|_| root_path());

    let mut snapshot = NetworkSnapshot {
        linked: state == 2,
        available: true,
        ..NetworkSnapshot::default()
    };
    if let Some(device_path) = devices.first() {
        let device = proxy(
            &connection,
            "org.freedesktop.NetworkManager",
            device_path.as_str(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await?;
        snapshot.interface = device.get_property("Interface").await.unwrap_or_default();
    }
    if ip_path.as_str() != "/" {
        let ip = proxy(
            &connection,
            "org.freedesktop.NetworkManager",
            ip_path.as_str(),
            "org.freedesktop.NetworkManager.IP4Config",
        )
        .await?;
        let addresses: Vec<std::collections::HashMap<String, OwnedValue>> =
            ip.get_property("AddressData").await.unwrap_or_default();
        if let Some(address) = addresses.first() {
            snapshot.ipv4 = property(address, "address").unwrap_or_default();
            snapshot.prefix = property(address, "prefix").unwrap_or_default();
        }
    }
    if specific.as_str() != "/" {
        let access_point = proxy(
            &connection,
            "org.freedesktop.NetworkManager",
            specific.as_str(),
            "org.freedesktop.NetworkManager.AccessPoint",
        )
        .await?;
        let ssid: Vec<u8> = access_point.get_property("Ssid").await.unwrap_or_default();
        snapshot.ssid = String::from_utf8_lossy(&ssid).into_owned();
        snapshot.signal = access_point
            .get_property("Strength")
            .await
            .unwrap_or_default();
    }
    Ok(snapshot)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BluetoothSnapshot {
    pub powered: bool,
    pub connected: u32,
    pub available: bool,
}

pub fn start_bluetooth(
    interval: Duration,
    sender: CalloopSender<BackendEvent>,
) -> PollClient<BluetoothSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    PollClient::start(
        "bluetooth",
        interval,
        BluetoothSnapshot::default(),
        sender,
        move || {
            let Ok(runtime) = &runtime else {
                return BluetoothSnapshot::default();
            };
            runtime.block_on(query_bluetooth()).unwrap_or_default()
        },
    )
}

async fn query_bluetooth() -> Result<BluetoothSnapshot> {
    let connection = zbus::Connection::system()
        .await
        .context("connect to system D-Bus")?;
    let manager = zbus::fdo::ObjectManagerProxy::builder(&connection)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let objects = manager.get_managed_objects().await?;
    let mut snapshot = BluetoothSnapshot::default();
    for interfaces in objects.values() {
        for (name, values) in interfaces {
            match name.as_str() {
                "org.bluez.Adapter1" => {
                    snapshot.available = true;
                    snapshot.powered |= property(values, "Powered").unwrap_or(false);
                }
                "org.bluez.Device1" if property(values, "Connected").unwrap_or(false) => {
                    snapshot.connected = snapshot.connected.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    Ok(snapshot)
}

fn property<T>(values: &std::collections::HashMap<String, OwnedValue>, name: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    values
        .get(name)
        .cloned()
        .and_then(|value| T::try_from(value).ok())
}

async fn proxy<'a>(
    connection: &'a zbus::Connection,
    destination: &str,
    path: &str,
    interface: &str,
) -> Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(
        connection,
        destination.to_owned(),
        path.to_owned(),
        interface.to_owned(),
    )
    .await
    .context("create D-Bus proxy")
}

fn root_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/").unwrap_or_else(|_| unreachable!())
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn first_directory(root: &Path) -> Option<PathBuf> {
    let mut paths = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("desktop-rs-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn battery_snapshot_should_read_capacity_status_and_online() -> Result<()> {
        let root = temp_dir("battery");
        let bat = root.join("BAT0");
        let ac = root.join("AC0");
        fs::create_dir_all(&bat)?;
        fs::create_dir_all(&ac)?;
        fs::write(bat.join("type"), "Battery\n")?;
        fs::write(bat.join("capacity"), "73\n")?;
        fs::write(bat.join("status"), "Charging\n")?;
        fs::write(ac.join("type"), "Mains\n")?;
        fs::write(ac.join("online"), "1\n")?;

        let snapshot = battery_snapshot_at(&root);
        fs::remove_dir_all(root)?;

        assert_eq!(
            snapshot,
            BatterySnapshot {
                capacity: 73,
                status: "Charging".into(),
                online: true,
                available: true
            }
        );
        Ok(())
    }

    #[test]
    fn battery_snapshot_should_ignore_non_battery_capacity() -> Result<()> {
        let root = temp_dir("non-battery");
        let battery = root.join("BAT0");
        let ups = root.join("UPS0");
        fs::create_dir_all(&battery)?;
        fs::create_dir_all(&ups)?;
        fs::write(battery.join("type"), "Battery\n")?;
        fs::write(battery.join("capacity"), "80\n")?;
        fs::write(battery.join("status"), "Discharging\n")?;
        fs::write(ups.join("type"), "UPS\n")?;
        fs::write(ups.join("capacity"), "20\n")?;

        let snapshot = battery_snapshot_at(&root);
        fs::remove_dir_all(root)?;

        assert_eq!(snapshot.capacity, 80);
        Ok(())
    }

    #[test]
    fn backlight_snapshot_should_convert_brightness_to_percent() -> Result<()> {
        let root = temp_dir("backlight");
        let panel = root.join("panel");
        fs::create_dir_all(&panel)?;
        fs::write(panel.join("actual_brightness"), "375")?;
        fs::write(panel.join("brightness"), "250")?;
        fs::write(panel.join("max_brightness"), "500")?;

        let snapshot = backlight_snapshot_at(&root);
        fs::remove_dir_all(root)?;

        assert_eq!(
            snapshot,
            BacklightSnapshot {
                percent: 75,
                available: true
            }
        );
        Ok(())
    }

    #[test]
    fn backlight_snapshot_should_hide_a_device_without_a_maximum() -> Result<()> {
        let root = temp_dir("backlight-nomax");
        let panel = root.join("panel");
        fs::create_dir_all(&panel)?;
        fs::write(panel.join("brightness"), "250")?;
        fs::write(panel.join("max_brightness"), "0")?;

        let snapshot = backlight_snapshot_at(&root);
        fs::remove_dir_all(root)?;

        assert_eq!(snapshot, BacklightSnapshot::default());
        Ok(())
    }

    #[test]
    fn stepped_raw_should_move_brightness_by_whole_percentage_points() {
        assert_eq!(stepped_raw(250, 500, 5), 275);
        assert_eq!(stepped_raw(250, 500, -5), 225);
    }

    #[test]
    fn stepped_raw_should_keep_at_least_one_percent_of_brightness() {
        assert_eq!(stepped_raw(0, 500, -50), 5);
    }

    #[test]
    fn stepped_raw_should_stop_at_the_device_maximum() {
        assert_eq!(stepped_raw(500, 500, 20), 500);
    }

    #[test]
    fn stepped_raw_should_still_move_a_low_resolution_device() {
        // 10 steps of 1% on a 4-step device would floor to no change at all.
        assert_eq!(stepped_raw(2, 4, 1), 3);
    }
}
