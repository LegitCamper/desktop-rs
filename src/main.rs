mod config;
mod platform;
mod ui;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let name = std::env::args().nth(1).unwrap_or_else(|| "bar".to_owned());
    let mut windows = config::load()?;
    let window = windows.remove(&name).with_context(|| {
        format!("unknown window `{name}`; expected bar, notification, or launcher")
    })?;
    platform::linux::run(window).with_context(|| format!("desktop-rs `{name}` failed"))
}
