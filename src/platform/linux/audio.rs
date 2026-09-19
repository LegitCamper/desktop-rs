use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;

use crate::platform::linux::runtime::BackendEvent;

/// Snapshot of the default audio sink's volume and mute state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioSnapshot {
    pub volume: u32,
    pub muted: bool,
    pub available: bool,
}

/// Commands sent from the UI thread to the audio worker.
#[derive(Debug)]
pub enum AudioCommand {
    ToggleMute,
    StepVolume { step: i32, max_volume: u32 },
}

/// Client handle held by the UI thread to query and control audio.
#[derive(Clone)]
pub struct AudioClient {
    command_tx: Sender<AudioCommand>,
    snapshot: Arc<Mutex<AudioSnapshot>>,
}

impl AudioClient {
    pub fn start(backend_sender: CalloopSender<BackendEvent>) -> Self {
        let (command_tx, command_rx) = channel::<AudioCommand>();
        let initial = query_snapshot();
        let snapshot = Arc::new(Mutex::new(initial));

        let snap_cmd = Arc::clone(&snapshot);
        let sender_cmd = backend_sender.clone();
        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-audio-cmd".into())
            .spawn(move || {
                run_command_loop(command_rx, snap_cmd, sender_cmd);
            })
        {
            eprintln!("start audio command worker: {error}");
        }

        let snap_sub = Arc::clone(&snapshot);
        let sender_sub = backend_sender;
        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-audio-sub".into())
            .spawn(move || {
                run_subscriber_loop(snap_sub, sender_sub);
            })
        {
            eprintln!("start audio subscription worker: {error}");
        }

        Self {
            command_tx,
            snapshot,
        }
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snapshot
            .lock()
            .map_or_else(|_| AudioSnapshot::default(), |guard| *guard)
    }

    pub fn toggle_mute(&self) {
        let _ = self.command_tx.send(AudioCommand::ToggleMute);
    }

    pub fn step_volume(&self, step: i32, max_volume: u32) {
        let _ = self
            .command_tx
            .send(AudioCommand::StepVolume { step, max_volume });
    }
}

fn run_command_loop(
    rx: Receiver<AudioCommand>,
    snapshot: Arc<Mutex<AudioSnapshot>>,
    sender: CalloopSender<BackendEvent>,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            AudioCommand::ToggleMute => {
                if let Err(error) = pactl_cmd(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]) {
                    eprintln!("toggle default sink mute: {error:#}");
                }
            }
            AudioCommand::StepVolume { step, max_volume } => {
                let cur = snapshot.lock().map_or(0, |guard| guard.volume);
                let next = calculate_stepped_volume(cur, step, max_volume);
                let arg = format!("{next}%");
                if let Err(error) = pactl_cmd(&["set-sink-volume", "@DEFAULT_SINK@", &arg]) {
                    eprintln!("set default sink volume: {error:#}");
                }
            }
        }
        let new_snap = query_snapshot();
        if let Ok(mut guard) = snapshot.lock() {
            *guard = new_snap;
        }
        let _ = sender.send(BackendEvent::Redraw);
    }
}

fn run_subscriber_loop(snapshot: Arc<Mutex<AudioSnapshot>>, sender: CalloopSender<BackendEvent>) {
    loop {
        let child = Command::new("pactl")
            .env("LC_ALL", "C")
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let Ok(mut child) = child else {
            thread::sleep(Duration::from_secs(5));
            continue;
        };

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if line.contains("'sink'") || line.contains("'server'") {
                    let new_snap = query_snapshot();
                    let changed = if let Ok(mut guard) = snapshot.lock() {
                        let diff = *guard != new_snap;
                        *guard = new_snap;
                        diff
                    } else {
                        false
                    };
                    if changed {
                        let _ = sender.send(BackendEvent::Redraw);
                    }
                }
            }
        }

        let _ = child.wait();
        thread::sleep(Duration::from_secs(2));
    }
}

fn pactl_cmd(args: &[&str]) -> Result<String> {
    let output = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .context("execute pactl command")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("pactl command failed: {stderr}"));
    }
    String::from_utf8(output.stdout).context("pactl output not UTF-8")
}

pub fn query_snapshot() -> AudioSnapshot {
    let vol_out = pactl_cmd(&["get-sink-volume", "@DEFAULT_SINK@"]);
    let mute_out = pactl_cmd(&["get-sink-mute", "@DEFAULT_SINK@"]);
    match (vol_out, mute_out) {
        (Ok(volume_output), Ok(mute_output)) => match (
            parse_sink_volume(&volume_output),
            parse_sink_mute(&mute_output),
        ) {
            (Some(volume), Some(muted)) => AudioSnapshot {
                volume,
                muted,
                available: true,
            },
            _ => AudioSnapshot::default(),
        },
        _ => AudioSnapshot::default(),
    }
}

/// Parses the volume percentage from `pactl get-sink-volume` output.
pub fn parse_sink_volume(output: &str) -> Option<u32> {
    for part in output.split('/') {
        let trimmed = part.trim();
        if let Some(pct) = trimmed.strip_suffix('%') {
            if let Ok(vol) = pct.trim().parse::<u32>() {
                return Some(vol);
            }
        }
    }
    None
}

/// Parses `Mute: yes` or `Mute: no` from `pactl get-sink-mute` output.
pub fn parse_sink_mute(output: &str) -> Option<bool> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Mute:") {
            let val = rest.trim();
            if val.eq_ignore_ascii_case("yes") {
                return Some(true);
            } else if val.eq_ignore_ascii_case("no") {
                return Some(false);
            }
        }
    }
    None
}

/// Clamps volume calculation to [0, max_volume].
pub fn calculate_stepped_volume(current: u32, step: i32, max_volume: u32) -> u32 {
    let next = (i64::from(current)) + (i64::from(step));
    u32::try_from(next.clamp(0, i64::from(max_volume))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sink_volume_should_extract_stereo_percentage() {
        let out = "Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB\n        balance 0.00";
        assert_eq!(parse_sink_volume(out), Some(100));

        let out_42 =
            "Volume: front-left: 27525 /  42% / -23.51 dB,   front-right: 27525 /  42% / -23.51 dB";
        assert_eq!(parse_sink_volume(out_42), Some(42));
    }

    #[test]
    fn parse_sink_volume_should_extract_mono_percentage() {
        let out = "Volume: mono: 32768 / 50% / -18.06 dB";
        assert_eq!(parse_sink_volume(out), Some(50));
    }

    #[test]
    fn parse_sink_volume_should_reject_invalid_output() {
        assert_eq!(parse_sink_volume("Connection refused"), None);
        assert_eq!(parse_sink_volume(""), None);
    }

    #[test]
    fn parse_sink_mute_should_recognize_yes_and_no() {
        assert_eq!(parse_sink_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_sink_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_sink_mute("Mute: YES"), Some(true));
        assert_eq!(parse_sink_mute("Mute: NO"), Some(false));
        assert_eq!(parse_sink_mute("Failure: no such sink"), None);
    }

    #[test]
    fn calculate_stepped_volume_should_clamp_within_bounds() {
        assert_eq!(calculate_stepped_volume(50, 5, 100), 55);
        assert_eq!(calculate_stepped_volume(50, -5, 100), 45);
        assert_eq!(calculate_stepped_volume(98, 5, 100), 100);
        assert_eq!(calculate_stepped_volume(2, -5, 100), 0);
        assert_eq!(calculate_stepped_volume(120, 10, 150), 130);
        assert_eq!(calculate_stepped_volume(145, 10, 150), 150);
    }
}
