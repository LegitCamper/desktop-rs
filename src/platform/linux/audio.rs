use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;

use crate::platform::linux::runtime::BackendEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioTarget {
    Sink,
    Source,
}

impl AudioTarget {
    fn noun(self) -> &'static str {
        match self {
            Self::Sink => "sink",
            Self::Source => "source",
        }
    }

    fn pactl_target(self) -> &'static str {
        match self {
            Self::Sink => "@DEFAULT_SINK@",
            Self::Source => "@DEFAULT_SOURCE@",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioSnapshot {
    pub volume: u32,
    pub muted: bool,
    pub available: bool,
}

#[derive(Debug)]
pub enum AudioCommand {
    ToggleMute(AudioTarget),
    StepVolume {
        target: AudioTarget,
        step: i32,
        max_volume: u32,
    },
}

#[derive(Clone)]
pub struct AudioClient {
    command_tx: Sender<AudioCommand>,
    sink: Arc<Mutex<AudioSnapshot>>,
    source: Arc<Mutex<AudioSnapshot>>,
}

impl AudioClient {
    pub fn start(backend_sender: CalloopSender<BackendEvent>) -> Self {
        let (command_tx, command_rx) = channel();
        let sink = Arc::new(Mutex::new(query_snapshot(AudioTarget::Sink)));
        let source = Arc::new(Mutex::new(query_snapshot(AudioTarget::Source)));

        let command_sink = Arc::clone(&sink);
        let command_source = Arc::clone(&source);
        let command_sender = backend_sender.clone();
        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-audio-cmd".into())
            .spawn(move || {
                run_command_loop(command_rx, command_sink, command_source, command_sender)
            })
        {
            eprintln!("start audio command worker: {error}");
        }

        let subscriber_sink = Arc::clone(&sink);
        let subscriber_source = Arc::clone(&source);
        if let Err(error) = thread::Builder::new()
            .name("desktop-rs-audio-sub".into())
            .spawn(move || run_subscriber_loop(subscriber_sink, subscriber_source, backend_sender))
        {
            eprintln!("start audio subscription worker: {error}");
        }

        Self {
            command_tx,
            sink,
            source,
        }
    }

    pub fn snapshot(&self, target: AudioTarget) -> AudioSnapshot {
        let snapshot = match target {
            AudioTarget::Sink => &self.sink,
            AudioTarget::Source => &self.source,
        };
        snapshot
            .lock()
            .map_or_else(|_| AudioSnapshot::default(), |guard| *guard)
    }

    pub fn toggle_mute(&self, target: AudioTarget) {
        let _ = self.command_tx.send(AudioCommand::ToggleMute(target));
    }

    pub fn step_volume(&self, target: AudioTarget, step: i32, max_volume: u32) {
        let _ = self.command_tx.send(AudioCommand::StepVolume {
            target,
            step,
            max_volume,
        });
    }
}

fn run_command_loop(
    rx: Receiver<AudioCommand>,
    sink: Arc<Mutex<AudioSnapshot>>,
    source: Arc<Mutex<AudioSnapshot>>,
    sender: CalloopSender<BackendEvent>,
) {
    while let Ok(command) = rx.recv() {
        match command {
            AudioCommand::ToggleMute(target) => {
                let action = format!("set-{}-mute", target.noun());
                if let Err(error) = pactl_cmd(&[&action, target.pactl_target(), "toggle"]) {
                    eprintln!("toggle default {} mute: {error:#}", target.noun());
                }
                refresh(target, &sink, &source);
            }
            AudioCommand::StepVolume {
                target,
                step,
                max_volume,
            } => {
                let current = match target {
                    AudioTarget::Sink => &sink,
                    AudioTarget::Source => &source,
                }
                .lock()
                .map_or(0, |guard| guard.volume);
                let next = calculate_stepped_volume(current, step, max_volume);
                let action = format!("set-{}-volume", target.noun());
                let argument = format!("{next}%");
                if let Err(error) = pactl_cmd(&[&action, target.pactl_target(), &argument]) {
                    eprintln!("set default {} volume: {error:#}", target.noun());
                }
                refresh(target, &sink, &source);
            }
        }
        let _ = sender.send(BackendEvent::Redraw);
    }
}

fn run_subscriber_loop(
    sink: Arc<Mutex<AudioSnapshot>>,
    source: Arc<Mutex<AudioSnapshot>>,
    sender: CalloopSender<BackendEvent>,
) {
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
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let target = if line.contains("'source'") {
                    Some(AudioTarget::Source)
                } else if line.contains("'sink'") {
                    Some(AudioTarget::Sink)
                } else {
                    None
                };
                let changed = target.is_some_and(|target| refresh(target, &sink, &source));
                let server = line.contains("'server'");
                if server {
                    refresh(AudioTarget::Sink, &sink, &source);
                    refresh(AudioTarget::Source, &sink, &source);
                }
                if changed || server {
                    let _ = sender.send(BackendEvent::Redraw);
                }
            }
        }
        let _ = child.wait();
        thread::sleep(Duration::from_secs(2));
    }
}

fn refresh(
    target: AudioTarget,
    sink: &Arc<Mutex<AudioSnapshot>>,
    source: &Arc<Mutex<AudioSnapshot>>,
) -> bool {
    let next = query_snapshot(target);
    let snapshot = match target {
        AudioTarget::Sink => sink,
        AudioTarget::Source => source,
    };
    snapshot.lock().is_ok_and(|mut current| {
        let changed = *current != next;
        *current = next;
        changed
    })
}

fn pactl_cmd(args: &[&str]) -> Result<String> {
    let output = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .context("execute pactl command")?;
    if !output.status.success() {
        return Err(anyhow!(
            "pactl command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).context("pactl output not UTF-8")
}

pub fn query_snapshot(target: AudioTarget) -> AudioSnapshot {
    let volume = format!("get-{}-volume", target.noun());
    let mute = format!("get-{}-mute", target.noun());
    match (
        pactl_cmd(&[&volume, target.pactl_target()]),
        pactl_cmd(&[&mute, target.pactl_target()]),
    ) {
        (Ok(volume), Ok(mute)) => match (parse_volume(&volume), parse_mute(&mute)) {
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

pub fn parse_volume(output: &str) -> Option<u32> {
    output
        .split('/')
        .find_map(|part| part.trim().strip_suffix('%')?.trim().parse::<u32>().ok())
}

pub fn parse_mute(output: &str) -> Option<bool> {
    let value = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Mute:"))?
        .trim();
    if value.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if value.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}

pub fn calculate_stepped_volume(current: u32, step: i32, max_volume: u32) -> u32 {
    let next = i64::from(current) + i64::from(step);
    u32::try_from(next.clamp(0, i64::from(max_volume))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_should_extract_stereo_percentage() {
        let out =
            "Volume: front-left: 27525 /  42% / -23.51 dB, front-right: 27525 / 42% / -23.51 dB";
        assert_eq!(parse_volume(out), Some(42));
    }

    #[test]
    fn parse_volume_should_extract_mono_percentage() {
        assert_eq!(
            parse_volume("Volume: mono: 32768 / 50% / -18.06 dB"),
            Some(50)
        );
    }

    #[test]
    fn parse_mute_should_recognize_yes_and_no() {
        assert_eq!(parse_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_mute("Mute: no\n"), Some(false));
    }

    #[test]
    fn calculate_stepped_volume_should_clamp_within_bounds() {
        assert_eq!(calculate_stepped_volume(98, 5, 100), 100);
        assert_eq!(calculate_stepped_volume(2, -5, 100), 0);
    }
}
