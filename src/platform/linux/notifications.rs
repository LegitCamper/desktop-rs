//! Native `org.freedesktop.Notifications` service.
//!
//! The daemon owns the bus name and nothing else: each notification is shown by
//! re-executing this binary as `desktop-rs notification`, reusing the existing
//! one-surface runtime. Notices are drained one at a time so two popups never
//! fight over the same screen corner.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use anyhow::{Context, Result};
use zbus::zvariant::OwnedValue;

use crate::platform::linux::window::Window;
use crate::ui::element::{Content, Element};

const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
/// Ceiling for a single popup, including clients asking to never expire.
/// Without it one sticky notice would block every later one in the queue.
const MAX_HOLD_MS: u32 = 30_000;

/// One notification, in the form the surface process needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notice {
    pub app: String,
    pub summary: String,
    pub body: String,
    /// Visible time before sliding out. `None` keeps the configured hold.
    pub timeout_ms: Option<u32>,
}

impl Notice {
    /// Reads the `--app`/`--summary`/`--body`/`--timeout` argv pairs.
    pub fn from_args(args: &[String]) -> Self {
        let mut notice = Self::default();
        for pair in args.chunks_exact(2) {
            let [key, value] = pair else { continue };
            match key.as_str() {
                "--app" => notice.app = value.clone(),
                "--summary" => notice.summary = value.clone(),
                "--body" => notice.body = value.clone(),
                "--timeout" => notice.timeout_ms = value.parse().ok(),
                _ => {}
            }
        }
        notice
    }

    fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--app".to_owned(),
            self.app.clone(),
            "--summary".to_owned(),
            self.summary.clone(),
            "--body".to_owned(),
            self.body.clone(),
        ];
        if let Some(timeout) = self.timeout_ms {
            args.push("--timeout".to_owned());
            args.push(timeout.to_string());
        }
        args
    }

    /// Replaces `{app}`, `{summary}`, and `{body}` in the tree's text nodes.
    pub fn apply(&self, window: &mut Window) {
        fill(&mut window.root, self);
        if let (Some(timeout), Some(animation)) = (self.timeout_ms, window.animation.as_mut()) {
            animation.hold_ms = Some(timeout);
        }
    }

    fn substitute(&self, value: &str) -> String {
        value
            .replace("{app}", &self.app)
            .replace("{summary}", &self.summary)
            .replace("{body}", &self.body)
    }
}

fn fill(element: &mut Element, notice: &Notice) {
    if let Content::Text(text) = &mut element.content {
        text.value = notice.substitute(&text.value);
    }
    for child in &mut element.children {
        fill(child, notice);
    }
}

/// Visible time for a client's `expire_timeout`, in the D-Bus convention:
/// negative means "server default", zero means "never".
fn hold_ms(expire_timeout: i32) -> Option<u32> {
    match expire_timeout {
        negative if negative < 0 => None,
        0 => Some(MAX_HOLD_MS),
        millis => Some(
            u32::try_from(millis)
                .unwrap_or(MAX_HOLD_MS)
                .min(MAX_HOLD_MS),
        ),
    }
}

struct Server {
    queue: Sender<Notice>,
    next_id: AtomicU32,
}

// ponytail: no actions, icons, urgency styling, or NotificationClosed /
// ActionInvoked signals; `notify-send --wait` will not return early. Add them
// once a surface can host buttons and the runtime owns more than one window.
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[expect(clippy::too_many_arguments, reason = "D-Bus method signature")]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let _ = self.queue.send(Notice {
            app: app_name,
            summary,
            body,
            timeout_ms: hold_ms(expire_timeout),
        });
        if replaces_id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            replaces_id
        }
    }

    fn close_notification(&self, _id: u32) {}

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_owned()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "desktop-rs".to_owned(),
            "desktop-rs".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
            "1.2".to_owned(),
        )
    }
}

/// Owns the notification bus name until the process is stopped.
pub fn daemon() -> Result<()> {
    let (queue, receiver) = channel();
    thread::Builder::new()
        .name("desktop-rs-notify".into())
        .spawn(move || show_queued(&receiver))
        .context("start notification queue worker")?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("create notification runtime")?;
    runtime.block_on(async {
        let _connection = zbus::connection::Builder::session()
            .context("connect to session D-Bus")?
            .name(BUS_NAME)
            .context("register notification bus name")?
            .serve_at(
                OBJECT_PATH,
                Server {
                    queue,
                    next_id: AtomicU32::new(1),
                },
            )
            .context("serve notification object")?
            .build()
            .await
            .with_context(|| format!("claim `{BUS_NAME}`; another daemon may already own it"))?;
        std::future::pending::<()>().await;
        Ok(())
    })
}

/// Shows one notice at a time; a failing popup never stops the daemon.
fn show_queued(receiver: &Receiver<Notice>) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("locate desktop-rs binary: {error}");
            return;
        }
    };
    for notice in receiver {
        if let Err(error) = Command::new(&exe)
            .arg("notification")
            .args(notice.to_args())
            .status()
        {
            eprintln!("show notification: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::animation::{Animation, Slide};
    use crate::ui::element::Text;
    use crate::ui::style::{Color, Style};

    fn notice() -> Notice {
        Notice {
            app: "mail".to_owned(),
            summary: "New message".to_owned(),
            body: "from Ada".to_owned(),
            timeout_ms: Some(1500),
        }
    }

    fn text_node(value: &str) -> Element {
        Element {
            content: Content::Text(Text {
                value: value.to_owned(),
                color: Color::rgba(0xff, 0xff, 0xff, 0xff),
                font_size: 14,
                font_family: String::new(),
                align: crate::ui::element::Align::Start,
            }),
            ..Element::new(Style::panel(Color::TRANSPARENT))
        }
    }

    #[test]
    fn args_should_round_trip_through_argv() {
        let notice = notice();

        assert_eq!(Notice::from_args(&notice.to_args()), notice);
    }

    #[test]
    fn from_args_should_ignore_unknown_and_dangling_flags() {
        let args = ["--summary", "hi", "--nope", "x", "--body"].map(str::to_owned);

        let parsed = Notice::from_args(&args);

        assert_eq!(parsed.summary, "hi");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn apply_should_substitute_placeholders_and_override_the_hold() {
        let mut window = Window::notification();
        window.animation = Some(Animation {
            slide: Slide::Right,
            distance: 400,
            duration_ms: 200,
            hold_ms: Some(4000),
        });
        window.root.children = vec![text_node("{app}: {summary}"), text_node("{body}")];

        notice().apply(&mut window);

        let values: Vec<_> = window
            .root
            .children
            .iter()
            .map(|child| match &child.content {
                Content::Text(text) => text.value.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(values, ["mail: New message", "from Ada"]);
        assert_eq!(
            window.animation.and_then(|animation| animation.hold_ms),
            Some(1500)
        );
    }

    #[test]
    fn hold_ms_should_follow_the_dbus_expiry_convention() {
        assert_eq!(hold_ms(-1), None);
        assert_eq!(hold_ms(0), Some(MAX_HOLD_MS));
        assert_eq!(hold_ms(2000), Some(2000));
        assert_eq!(hold_ms(i32::MAX), Some(MAX_HOLD_MS));
    }
}
