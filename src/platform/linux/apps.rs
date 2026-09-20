use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use freedesktop_desktop_entry::DesktopEntry;

/// One runnable application discovered from XDG desktop entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub keywords: String,
    pub executable: Vec<String>,
    pub terminal: bool,
    pub icon: String,
    pub path: PathBuf,
}

/// Scan XDG desktop directories once. Earlier directories win duplicate IDs.
pub fn scan(desktop_dirs: &[PathBuf], locale: &str) -> Vec<App> {
    let mut by_id = BTreeMap::new();
    for directory in desktop_dirs {
        for path in desktop_files(directory) {
            let Some(id) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if by_id.contains_key(id) {
                continue;
            }
            if let Some(app) = parse(&path, id, locale) {
                by_id.insert(id.to_owned(), app);
            }
        }
    }
    by_id.into_values().collect()
}

fn desktop_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|value| value.to_str()) == Some("desktop")
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn parse(path: &Path, id: &str, locale: &str) -> Option<App> {
    let contents = fs::read_to_string(path).ok()?;
    let locales = [locale];
    let entry = DesktopEntry::from_str(path, &contents, Some(&locales)).ok()?;
    if entry.type_() != Some("Application")
        || entry.hidden()
        || entry.no_display()
        || !shown_in(entry.only_show_in())
        || hidden_in(entry.not_show_in())
        || !try_exec(&entry)
    {
        return None;
    }
    let executable = parse_exec(&entry)?;
    if !executable.first().is_some_and(|program| available(program)) {
        return None;
    }
    Some(App {
        id: id.to_owned(),
        name: entry
            .name(&locales)
            .map(|name| name.to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| id.to_owned()),
        generic_name: entry
            .generic_name(&locales)
            .map(|name| name.to_string())
            .unwrap_or_default(),
        comment: entry
            .comment(&locales)
            .map(|comment| comment.to_string())
            .unwrap_or_default(),
        keywords: entry
            .keywords(&locales)
            .map(|keywords| keywords.join(" "))
            .unwrap_or_default(),
        executable,
        terminal: entry.terminal(),
        icon: entry.icon().unwrap_or("").to_owned(),
        path: path.to_owned(),
    })
}

fn parse_exec(entry: &DesktopEntry) -> Option<Vec<String>> {
    let mut executable = entry.parse_exec().ok()?;
    // Flatpak uses these sentinels around file-forwarding field codes. They are
    // launcher metadata, not argv; the dependency intentionally preserves them.
    executable.retain(|argument| !matches!(argument.as_str(), "@@" | "@@u"));
    (!executable.is_empty()).then_some(executable)
}

fn shown_in(only: Option<Vec<&str>>) -> bool {
    !only.is_some_and(|values| !matches_desktop(values))
}

fn hidden_in(not: Option<Vec<&str>>) -> bool {
    not.is_some_and(matches_desktop)
}

fn matches_desktop(values: Vec<&str>) -> bool {
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    values.iter().any(|candidate| {
        current
            .split(':')
            .any(|active| active.eq_ignore_ascii_case(candidate))
    })
}

/// Only a present `TryExec` key filters an entry; the spec defines no Exec
/// existence check, but an `Exec` program that cannot be spawned only produces
/// a result row that fails on Enter, so both keys are resolved the same way.
fn try_exec(entry: &DesktopEntry) -> bool {
    match entry.try_exec().filter(|value| !value.is_empty()) {
        None => true,
        Some(candidate) => available(candidate),
    }
}

/// True when `candidate` resolves to an executable file, by path or through `PATH`.
fn available(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let path = PathBuf::from(candidate);
    if path.is_absolute() || candidate.contains('/') {
        executable(&path)
    } else {
        in_path(candidate)
    }
}

fn in_path(candidate: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|directory| executable(&directory.join(candidate)))
}

fn executable(path: &Path) -> bool {
    executable_file(path) || host_path(path).is_some_and(|path| executable_file(&path))
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Host equivalent of an absolute path inside a sandboxed desktop session.
fn host_path(path: &Path) -> Option<PathBuf> {
    path.is_absolute()
        .then(|| path.strip_prefix("/").ok())
        .flatten()
        .map(|path| Path::new("/run/host").join(path))
}

/// Builds the process for `argv`. A program that only exists under `/run/host`
/// cannot be exec'd directly — it links against host libraries this sandbox does
/// not have — so it runs on the host through the Flatpak portal instead.
fn command_for(argv: &[String]) -> Command {
    let program = &argv[0];
    if !executable_file(Path::new(program))
        && host_path(Path::new(program)).is_some_and(|path| executable_file(&path))
        && available(HOST_SPAWN)
    {
        let mut command = Command::new(HOST_SPAWN);
        command.arg("--host").args(argv);
        return command;
    }
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    command
}

const HOST_SPAWN: &str = "flatpak-spawn";

/// Compact subsequence score. Higher is better; `None` means no match.
pub fn score(needle: &str, haystack: &str) -> Option<u32> {
    let needle: Vec<char> = needle
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| !character.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let haystack: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    if haystack.len() < needle.len() {
        return None;
    }
    let mut positions = Vec::with_capacity(needle.len());
    let mut haystack_index = 0;
    for character in &needle {
        let found = haystack[haystack_index..]
            .iter()
            .position(|candidate| candidate == character)?;
        haystack_index += found + 1;
        positions.push(haystack_index - 1);
    }
    let first = positions[0];
    let last = positions[positions.len() - 1];
    let mut value = 1_000u32
        .saturating_sub(u32::try_from(last - first).unwrap_or(u32::MAX))
        .saturating_sub(u32::try_from(first).unwrap_or(u32::MAX))
        .saturating_sub(
            haystack
                .len()
                .try_into()
                .unwrap_or(u32::MAX)
                .saturating_mul(2),
        );
    for index in 1..positions.len() {
        if positions[index] == positions[index - 1] + 1 {
            value = value.saturating_add(20);
        }
        if positions[index - 1] == 0 || matches!(haystack[positions[index - 1]], ' ' | '-' | '_') {
            value = value.saturating_add(10);
        }
    }
    Some(value)
}

pub fn search<'a>(query: &str, apps: &'a [App], limit: usize) -> Vec<&'a App> {
    let mut ranked = if query.trim().is_empty() {
        let mut apps = apps.iter().collect::<Vec<_>>();
        apps.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        apps.into_iter().map(|app| (0, app)).collect()
    } else {
        apps.iter()
            .filter_map(|app| {
                let name = score(query, &app.name)?;
                let keywords = score(query, &app.keywords).unwrap_or_default();
                let generic = score(query, &app.generic_name).unwrap_or_default();
                let comment = score(query, &app.comment).unwrap_or_default();
                Some((
                    name.saturating_add(100) + keywords + generic / 2 + comment / 4,
                    app,
                ))
            })
            .collect::<Vec<_>>()
    };
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then(left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then(left.id.cmp(&right.id))
    });
    ranked.truncate(limit);
    ranked.into_iter().map(|(_, app)| app).collect()
}

/// Launch argv directly. Field codes were removed by `parse_exec`; never use a shell.
/// Terminal applications run inside `terminal`'s argv prefix, which must end in the
/// flag that hands the rest of the arguments to a shell (`-e` for most emulators).
pub fn launch(app: &App, terminal: &[String]) -> Result<()> {
    if app.executable.is_empty() {
        bail!("application `{}` has no executable", app.name);
    }
    let mut command = match (app.terminal, terminal.split_first()) {
        (true, None) => bail!(
            "`{}` requests a terminal; set the `terminal` prop on AppList to launch it",
            app.name
        ),
        (true, Some((first, prefix))) => {
            let mut command = Command::new(first);
            command.args(prefix).args(&app.executable);
            command
        }
        (false, _) => command_for(&app.executable),
    };
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "launch `{}` from `{}`",
                app.executable.join(" "),
                app.path.display()
            )
        })?;
    // Desktop applications belong to the session, not this launcher. Dropping
    // the child handle after a successful spawn avoids reaping them here.
    let _ = child;
    Ok(())
}

/// Search state: one startup scan, then all queries search memory.
pub struct Apps {
    query: String,
    /// Caret offset into `query`; always on a UTF-8 character boundary.
    caret: usize,
    selected: usize,
    /// First result drawn when the match list is taller than the window.
    scroll: usize,
    /// Rows the UI shows at once.
    visible: usize,
    all: Vec<App>,
    receiver: Option<Receiver<Vec<App>>>,
    /// argv prefix that starts a terminal emulator, for `Terminal=true` entries.
    terminal: Vec<String>,
}

impl Apps {
    pub fn scan() -> Self {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
            let locale = locale.split('.').next().unwrap_or("en_US").to_owned();
            let apps = scan(&default_dirs(), &locale);
            let _ = sender.send(apps);
        });
        Self {
            receiver: Some(receiver),
            ..Self::empty()
        }
    }

    /// Index already in hand; the live scan runs on a thread instead.
    #[cfg(test)]
    pub fn from_index(all: Vec<App>) -> Self {
        Self {
            all,
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            query: String::new(),
            caret: 0,
            selected: 0,
            scroll: 0,
            visible: 8,
            all: Vec::new(),
            receiver: None,
            terminal: Vec::new(),
        }
    }

    pub fn set_visible(&mut self, rows: usize) {
        self.visible = rows.max(1);
        self.sync();
    }

    pub fn set_terminal(&mut self, argv: Vec<String>) {
        self.terminal = argv;
    }

    pub fn poll(&mut self) -> Result<bool> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Ok(false);
        };
        match receiver.try_recv() {
            Ok(apps) => {
                self.all = apps;
                self.receiver = None;
                self.sync();
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                Err(anyhow!(
                    "desktop-entry scanner stopped before publishing an index"
                ))
            }
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Query split at the caret, for drawing the caret inside the text.
    pub fn query_around_caret(&self) -> (&str, &str) {
        self.query.split_at(self.caret)
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.caret = self.query.len();
        self.sync();
    }

    /// Inserts typed text at the caret and steps the caret past it.
    pub fn type_text(&mut self, text: &str) {
        self.query.insert_str(self.caret, text);
        self.caret += text.len();
        self.sync();
    }

    /// Moves the caret one character left or right; `delta` is in characters.
    pub fn move_caret(&mut self, delta: isize) {
        self.caret = match delta.cmp(&0) {
            std::cmp::Ordering::Less => self.previous_boundary(),
            std::cmp::Ordering::Greater => self.next_boundary(),
            std::cmp::Ordering::Equal => self.caret,
        };
    }

    pub fn caret_to_start(&mut self) {
        self.caret = 0;
    }

    pub fn caret_to_end(&mut self) {
        self.caret = self.query.len();
    }

    /// Drops one word with `word`, otherwise one character, before the caret.
    pub fn backspace(&mut self, word: bool) {
        let start = if word {
            self.word_start()
        } else {
            self.previous_boundary()
        };
        self.query.replace_range(start..self.caret, "");
        self.caret = start;
        self.sync();
    }

    /// Removes everything after the caret, as Ctrl+K does.
    pub fn delete_to_end(&mut self) {
        self.query.truncate(self.caret);
        self.sync();
    }

    pub fn clear_query(&mut self) {
        self.set_query(String::new());
    }

    fn previous_boundary(&self) -> usize {
        self.query[..self.caret]
            .chars()
            .next_back()
            .map_or(0, |character| self.caret - character.len_utf8())
    }

    fn next_boundary(&self) -> usize {
        self.query[self.caret..]
            .chars()
            .next()
            .map_or(self.caret, |character| self.caret + character.len_utf8())
    }

    /// Start of the whitespace-delimited word ending at the caret.
    fn word_start(&self) -> usize {
        let trimmed = self.query[..self.caret].trim_end_matches(char::is_whitespace);
        trimmed
            .char_indices()
            .rfind(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8())
    }

    /// All matches, untruncated; `selected` indexes into this list.
    pub fn matches(&self) -> Vec<&App> {
        search(&self.query, &self.all, usize::MAX)
    }

    /// Drawn rows, each paired with whether it holds the selection.
    pub fn page(&self) -> Vec<(&App, bool)> {
        let matches = self.matches();
        let Some(window) = matches.get(self.scroll..) else {
            return Vec::new();
        };
        window
            .iter()
            .take(self.visible)
            .enumerate()
            .map(|(row, app)| (*app, self.scroll + row == self.selected))
            .collect()
    }

    pub fn selected(&self) -> Option<&App> {
        self.matches().into_iter().nth(self.selected)
    }

    pub fn select(&mut self, delta: isize) {
        let total = self.matches().len();
        if total > 0 {
            self.selected =
                ((self.selected as isize + delta).clamp(0, total as isize - 1)) as usize;
            self.sync_scroll();
        }
    }

    /// Jump selection to an absolute match index, as from a pointer press.
    pub fn select_index(&mut self, index: usize) {
        let total = self.matches().len();
        if index < total {
            self.selected = index;
            self.sync_scroll();
        }
    }

    /// Moves selection to the row named by a hit-test id; true when it moved.
    pub fn select_id(&mut self, id: &str) -> bool {
        let Some(index) = self.matches().into_iter().position(|app| app.id == id) else {
            return false;
        };
        let before = self.selected;
        self.select_index(index);
        before != index
    }

    pub fn select_last(&mut self) {
        let last = self.matches().len().saturating_sub(1);
        self.select_index(last);
    }

    /// Moves by one page of rows, as PageUp and PageDown do.
    pub fn page_by(&mut self, direction: isize) {
        self.select(direction * self.visible as isize);
    }

    /// Starts the selected application; `Ok(true)` means the UI may close.
    pub fn activate(&mut self) -> Result<bool> {
        let Some(app) = self.selected().cloned() else {
            return Ok(false);
        };
        launch(&app, &self.terminal)
            .with_context(|| format!("activate `{}`", app.name))
            .map(|()| true)
    }

    pub fn is_scanning(&self) -> bool {
        self.receiver.is_some()
    }

    fn sync(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    fn sync_scroll(&mut self) {
        if self.selected >= self.scroll + self.visible {
            self.scroll = self.selected + 1 - self.visible;
        } else if self.selected < self.scroll {
            self.scroll = self.selected;
        }
    }
}

/// Desktop directories in XDG precedence order.
pub fn default_dirs() -> Vec<PathBuf> {
    default_dirs_from(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_DATA_DIRS"),
    )
}

fn default_dirs_from(
    data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    data_dirs: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(home) = data_home
        .filter(|value| !value.is_empty())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/share").into_os_string()))
    {
        directories.push(PathBuf::from(home).join("applications"));
    }
    let variable = data_dirs
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    directories
        .extend(std::env::split_paths(&variable).map(|directory| directory.join("applications")));
    let mut seen = HashSet::new();
    directories.retain(|directory| seen.insert(directory.clone()));
    directories
}

#[cfg(test)]
mod tests {
    #![expect(clippy::panic, reason = "assertion failure in tests")]

    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "desktop-rs-apps-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn write(root: &Path, name: &str, body: &str) -> Result<PathBuf> {
        fs::create_dir_all(root)?;
        let path = root.join(name);
        fs::write(&path, body)?;
        Ok(path)
    }

    fn app(name: &str) -> App {
        App {
            id: name.into(),
            name: name.into(),
            generic_name: String::new(),
            comment: String::new(),
            keywords: String::new(),
            executable: vec!["true".into()],
            terminal: false,
            icon: String::new(),
            path: PathBuf::from(format!("/tmp/{name}.desktop")),
        }
    }

    #[test]
    fn default_dirs_should_prefer_and_deduplicate_xdg_data_home() {
        let home = PathBuf::from("/tmp/desktop-rs-xdg-data-home");
        let data_dirs =
            std::env::join_paths([home.as_path(), Path::new("/two")]).unwrap_or_default();
        let directories =
            default_dirs_from(Some(home.clone().into_os_string()), None, Some(data_dirs));

        assert_eq!(
            directories,
            [
                home.join("applications"),
                PathBuf::from("/two/applications")
            ]
        );
    }

    #[test]
    fn scan_should_filter_entries_and_preserve_xdg_precedence() -> Result<()> {
        let root = temp_dir("scan");
        let first = root.join("first");
        let second = root.join("second");
        write(
            &first,
            "visible.desktop",
            "[Desktop Entry]\nType=Application\nName=Visible\nExec=/bin/true\nIcon=visible-icon\n",
        )?;
        write(
            &second,
            "visible.desktop",
            "[Desktop Entry]\nType=Application\nName=Shadowed\nExec=/bin/true\n",
        )?;
        write(
            &first,
            "nodisplay.desktop",
            "[Desktop Entry]\nType=Application\nName=Hidden\nNoDisplay=true\nExec=/bin/true\n",
        )?;
        write(
            &first,
            "link.desktop",
            "[Desktop Entry]\nType=Link\nName=Link\nURL=https://example.com\n",
        )?;
        write(
            &first,
            "missing.desktop",
            "[Desktop Entry]\nType=Application\nName=Missing\nTryExec=definitely-not-installed-desktop-rs\nExec=/bin/true\n",
        )?;

        let apps = scan(&[first, second], "en_US");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Visible");
        assert_eq!(apps[0].icon, "visible-icon");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn search_should_rank_prefix_word_and_subsequence_matches() {
        let apps = vec![
            app("Files"),
            app("Firefox"),
            app("Terminal"),
            app("Text Editor"),
        ];
        assert_eq!(search("fi", &apps, 10)[0].name, "Files");
        assert_eq!(search("fire", &apps, 10)[0].name, "Firefox");
        assert_eq!(search("te", &apps, 10)[0].name, "Terminal");
        assert_eq!(search("x ed", &apps, 10)[0].name, "Text Editor");
        assert!(search("zzz", &apps, 10).is_empty());
    }

    #[test]
    fn exec_parsing_should_remove_empty_url_field() -> Result<()> {
        let root = temp_dir("exec");
        let path = write(
            &root,
            "editor.desktop",
            "[Desktop Entry]\nType=Application\nName=Editor\nExec=/bin/sh %U\n",
        )?;
        let app = parse(&path, "editor", "en_US").context("parse editor entry")?;
        assert_eq!(app.executable, ["/bin/sh".to_owned()]);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn exec_parsing_should_remove_flatpak_file_forwarding_markers() -> Result<()> {
        let root = temp_dir("flatpak-exec");
        let path = write(
            &root,
            "flatpak.desktop",
            "[Desktop Entry]\nType=Application\nName=Flatpak App\nExec=/bin/sh run --file-forwarding org.example.App @@u %U @@\n",
        )?;
        let app = parse(&path, "flatpak", "en_US").context("parse Flatpak entry")?;

        assert_eq!(
            app.executable,
            ["/bin/sh", "run", "--file-forwarding", "org.example.App"]
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn launch_should_execute_argv() -> Result<()> {
        let root = temp_dir("launch");
        let marker = root.join("marker");
        let script = root.join("marker.sh");
        fs::create_dir_all(&root)?;
        fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display()))?;
        chmod_executable(&script)?;
        launch(
            &App {
                executable: vec![script.display().to_string()],
                ..app("marker")
            },
            &[],
        )?;
        let ran = wait_until(&marker);
        fs::remove_dir_all(root)?;
        if !ran {
            bail!("marker script did not run");
        }
        Ok(())
    }

    /// A program that only exists under `/run/host` must not be exec'd by that
    /// path: it links against host libraries absent from the sandbox and dies
    /// with 127 after a successful `spawn`, which looks like nothing happening.
    #[test]
    fn command_for_should_route_host_only_programs_through_the_portal() {
        let argv = vec!["/definitely/not/here/flatpak".to_owned(), "run".to_owned()];
        let command = command_for(&argv);

        assert_eq!(command.get_program(), "/definitely/not/here/flatpak");

        let host_only = ["/usr/bin/flatpak".to_owned(), "run".to_owned()];
        if executable_file(Path::new(&host_only[0])) || !available(HOST_SPAWN) {
            return; // Not a sandboxed session; nothing to assert.
        }
        if !host_path(Path::new(&host_only[0])).is_some_and(|path| executable_file(&path)) {
            return; // No host flatpak either.
        }
        let command = command_for(&host_only);
        assert_eq!(command.get_program(), HOST_SPAWN);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--host", "/usr/bin/flatpak", "run"]);
    }

    #[test]
    fn launch_should_run_terminal_entries_through_the_configured_argv() -> Result<()> {
        let root = temp_dir("terminal");
        let marker = root.join("marker");
        let script = root.join("terminal.sh");
        fs::create_dir_all(&root)?;
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\n", marker.display()),
        )?;
        chmod_executable(&script)?;
        let editor = App {
            executable: vec!["/usr/bin/editor".to_owned()],
            terminal: true,
            ..app("editor")
        };

        let Err(error) = launch(&editor, &[]) else {
            let _ = fs::remove_dir_all(&root);
            panic!("a terminal entry without a configured terminal must not silently launch");
        };
        assert!(
            error.to_string().contains("requests a terminal"),
            "unexpected error: {error}"
        );

        launch(&editor, &[script.display().to_string()])?;
        let ran = wait_until(&marker);
        let written = fs::read_to_string(&marker);
        fs::remove_dir_all(root)?;
        if !ran {
            bail!("terminal wrapper did not run");
        }
        assert_eq!(written?.trim(), "/usr/bin/editor");
        Ok(())
    }

    /// Empty-query order is by name: five, four, one, three, two.
    #[test]
    fn page_should_window_the_match_list() {
        let mut apps = Apps::from_index(vec![
            app("one"),
            app("two"),
            app("three"),
            app("four"),
            app("five"),
        ]);
        apps.set_visible(2);

        apps.select_last();
        let page = apps.page();
        assert_eq!(page.len(), 2);
        assert_eq!(
            page.iter()
                .map(|(app, _)| app.name.as_str())
                .collect::<Vec<_>>(),
            ["three", "two"]
        );
        assert!(page[1].1, "the last row should hold selection");
        assert!(!page[0].1);

        apps.page_by(-1);
        assert_eq!(apps.selected().map(|app| app.name.as_str()), Some("one"));
    }

    #[test]
    fn scan_should_drop_entries_whose_exec_program_is_missing() -> Result<()> {
        let root = temp_dir("missing-exec");
        write(
            &root,
            "stale-flatpak.desktop",
            "[Desktop Entry]\nType=Application\nName=Stale\nExec=/definitely/not/installed/flatpak run com.example.App\n",
        )?;
        write(
            &root,
            "stale-bare.desktop",
            "[Desktop Entry]\nType=Application\nName=Bare\nExec=definitely-not-installed-desktop-rs\n",
        )?;
        write(
            &root,
            "present.desktop",
            "[Desktop Entry]\nType=Application\nName=Present\nExec=/bin/sh -c true\n",
        )?;

        let apps = scan(std::slice::from_ref(&root), "en_US");
        fs::remove_dir_all(root)?;

        assert_eq!(
            apps.iter().map(|app| app.name.as_str()).collect::<Vec<_>>(),
            ["Present"]
        );
        Ok(())
    }

    #[test]
    fn launch_should_report_the_program_and_desktop_entry_path() {
        let missing = App {
            executable: vec!["/definitely/not/installed/flatpak".to_owned(), "run".into()],
            ..app("stale")
        };

        let Err(error) = launch(&missing, &[]) else {
            panic!("a missing program must not report a successful launch");
        };

        let chain = format!("{error:#}");
        assert!(
            chain.contains("/definitely/not/installed/flatpak run")
                && chain.contains("stale.desktop")
                && chain.contains("No such file or directory"),
            "unexpected error chain: {chain}"
        );
    }

    #[test]
    fn caret_should_edit_multibyte_text_in_place() {
        let mut apps = Apps::from_index(vec![app("Files")]);

        apps.type_text("aé中b");
        apps.move_caret(-1);
        apps.move_caret(-1);
        apps.type_text("X");
        assert_eq!(apps.query(), "aéX中b");
        assert_eq!(apps.query_around_caret(), ("aéX", "中b"));

        apps.backspace(false);
        assert_eq!(apps.query(), "aé中b");
        apps.move_caret(1);
        apps.move_caret(1);
        assert_eq!(apps.query_around_caret(), ("aé中b", ""));
        apps.move_caret(1);
        assert_eq!(apps.query_around_caret(), ("aé中b", ""));
    }

    #[test]
    fn caret_shortcuts_should_jump_and_delete_around_the_caret() {
        let mut apps = Apps::from_index(vec![app("Files")]);

        apps.type_text("text editor");
        apps.caret_to_start();
        assert_eq!(apps.query_around_caret(), ("", "text editor"));
        apps.move_caret(-1);
        assert_eq!(apps.query_around_caret(), ("", "text editor"));

        apps.caret_to_end();
        apps.backspace(true);
        assert_eq!(apps.query(), "text ");

        apps.type_text("editor");
        apps.move_caret(-1);
        apps.delete_to_end();
        assert_eq!(apps.query(), "text edito");
    }

    #[test]
    fn backspace_should_drop_one_word_or_one_character() {
        let mut apps = Apps::from_index(vec![app("Files")]);

        apps.type_text("text editor");
        apps.backspace(false);
        assert_eq!(apps.query(), "text edito");
        apps.backspace(true);
        assert_eq!(apps.query(), "text ");
        apps.backspace(true);
        assert_eq!(apps.query(), "");
        apps.clear_query();
        assert_eq!(apps.query(), "");
    }

    #[test]
    fn select_id_should_move_selection_and_report_change() {
        let mut apps = Apps::from_index(vec![app("one"), app("two"), app("three")]);
        apps.set_visible(2);

        assert!(apps.select_id("two"), "selection should move");
        assert_eq!(apps.selected().map(|app| app.id.as_str()), Some("two"));
        assert!(!apps.select_id("two"), "a repeat click should be a no-op");
        assert!(!apps.select_id("absent"));
    }

    #[test]
    fn activate_should_report_an_empty_match_list_without_launching() {
        let mut apps = Apps::from_index(vec![app("one")]);
        apps.set_query("nothing matches this".to_owned());

        assert!(
            !apps.activate().unwrap_or(true),
            "no selection means no launch"
        );
    }

    /// Waits for a spawned process to create `path`; the launch is async.
    fn wait_until(path: &Path) -> bool {
        for _ in 0..100 {
            if path.exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn chmod_executable(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
}
