use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;

use crate::platform::linux::runtime::BackendEvent;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomSnapshot {
    pub output: String,
    pub available: bool,
}

/// Pointer gesture a `Custom` widget can bind a command to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomEvent {
    Click,
    RightClick,
    MiddleClick,
    ScrollUp,
    ScrollDown,
}

/// Shell commands bound to each pointer gesture; `None` means the gesture is inert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomCommands {
    pub on_click: Option<String>,
    pub on_right_click: Option<String>,
    pub on_middle_click: Option<String>,
    pub on_scroll_up: Option<String>,
    pub on_scroll_down: Option<String>,
}

impl CustomCommands {
    fn command(&self, event: CustomEvent) -> Option<&str> {
        match event {
            CustomEvent::Click => self.on_click.as_deref(),
            CustomEvent::RightClick => self.on_right_click.as_deref(),
            CustomEvent::MiddleClick => self.on_middle_click.as_deref(),
            CustomEvent::ScrollUp => self.on_scroll_up.as_deref(),
            CustomEvent::ScrollDown => self.on_scroll_down.as_deref(),
        }
    }
}

#[derive(Clone)]
pub struct CustomClient {
    event_tx: Sender<CustomEvent>,
    snapshot: Arc<Mutex<CustomSnapshot>>,
}

impl CustomClient {
    pub fn start(
        exec: String,
        interval: Duration,
        commands: CustomCommands,
        sender: CalloopSender<BackendEvent>,
    ) -> Self {
        let (event_tx, event_rx) = channel();
        let snapshot = Arc::new(Mutex::new(CustomSnapshot::default()));
        let worker_snapshot = Arc::clone(&snapshot);
        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-custom".into())
            .spawn(move || run(exec, interval, commands, event_rx, worker_snapshot, sender))
        {
            eprintln!("start custom worker: {error}");
        }
        Self { event_tx, snapshot }
    }

    pub fn snapshot(&self) -> CustomSnapshot {
        self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |value| value.clone(),
        )
    }

    pub fn dispatch(&self, event: CustomEvent) {
        let _ = self.event_tx.send(event);
    }
}

fn run(
    exec: String,
    interval: Duration,
    commands: CustomCommands,
    events: Receiver<CustomEvent>,
    snapshot: Arc<Mutex<CustomSnapshot>>,
    sender: CalloopSender<BackendEvent>,
) {
    loop {
        publish(&snapshot, &sender, execute(&exec));
        match events.recv_timeout(interval) {
            Ok(event) => {
                if let Some(command) = commands.command(event) {
                    let _ = shell(command);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn execute(command: &str) -> CustomSnapshot {
    match shell(command) {
        Some(output) if output.status.success() => {
            let output = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            CustomSnapshot {
                available: !output.is_empty(),
                output,
            }
        }
        _ => CustomSnapshot::default(),
    }
}

fn shell(command: &str) -> Option<std::process::Output> {
    Command::new("/bin/sh").arg("-c").arg(command).output().ok()
}

fn publish(
    target: &Arc<Mutex<CustomSnapshot>>,
    sender: &CalloopSender<BackendEvent>,
    next: CustomSnapshot,
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

    #[test]
    fn execute_should_trim_successful_stdout() {
        assert_eq!(
            execute("printf 'hello\\n'"),
            CustomSnapshot {
                output: "hello".into(),
                available: true,
            }
        );
    }

    #[test]
    fn execute_should_hide_empty_output() {
        assert_eq!(execute("printf ''"), CustomSnapshot::default());
    }

    #[test]
    fn execute_should_hide_failed_commands() {
        assert_eq!(execute("exit 7"), CustomSnapshot::default());
    }

    #[test]
    fn commands_should_map_each_gesture_to_its_own_hook() {
        let commands = CustomCommands {
            on_click: Some("click".into()),
            on_right_click: Some("right".into()),
            on_middle_click: Some("middle".into()),
            on_scroll_up: Some("up".into()),
            on_scroll_down: Some("down".into()),
        };

        assert_eq!(commands.command(CustomEvent::Click), Some("click"));
        assert_eq!(commands.command(CustomEvent::RightClick), Some("right"));
        assert_eq!(commands.command(CustomEvent::MiddleClick), Some("middle"));
        assert_eq!(commands.command(CustomEvent::ScrollUp), Some("up"));
        assert_eq!(commands.command(CustomEvent::ScrollDown), Some("down"));
    }

    #[test]
    fn commands_should_report_nothing_for_unbound_gestures() {
        let commands = CustomCommands {
            on_click: Some("click".into()),
            ..CustomCommands::default()
        };

        assert_eq!(commands.command(CustomEvent::ScrollUp), None);
    }
}
