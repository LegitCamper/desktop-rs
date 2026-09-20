pub mod apps;
pub mod audio;
pub mod custom;
pub mod dbus;
pub mod notifications;
mod runtime;
pub mod status_notifier;
pub mod system;
pub mod taskbar;
pub mod window;

use anyhow::Result;

use crate::platform::linux::window::Window;

pub fn run(window: Window) -> Result<()> {
    runtime::run(window)
}
