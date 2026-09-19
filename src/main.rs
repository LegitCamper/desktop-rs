mod config;
mod platform;
mod ui;

use anyhow::{Context, Result};

use platform::linux::notifications;

const USAGE: &str = "usage: desktop-rs [bar|launcher|notification|daemon]";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = args.first().map_or("bar", String::as_str);
    if name == "daemon" {
        return notifications::daemon().context("desktop-rs `daemon` failed");
    }
    if matches!(name, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }

    let mut windows = config::load()?;
    let mut window = windows
        .remove(name)
        .with_context(|| format!("unknown command `{name}`; {USAGE}"))?;
    if name == "notification" {
        notifications::Notice::from_args(&args[1..]).apply(&mut window);
    }
    platform::linux::run(window).with_context(|| format!("desktop-rs `{name}` failed"))
}
