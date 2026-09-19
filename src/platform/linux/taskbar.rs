use std::collections::{BTreeMap, HashMap};

use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::{Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};

/// One desktop workspace exposed by the compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub coordinates: Vec<u32>,
    pub active: bool,
    pub urgent: bool,
    pub hidden: bool,
}

/// One toplevel window tracked via foreign-toplevel protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toplevel {
    pub id: String,
    pub title: String,
    pub app_id: String,
    pub activated: bool,
    pub maximized: bool,
    pub minimized: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PendingWorkspace {
    name: Option<String>,
    coordinates: Option<Vec<u32>>,
    active: Option<bool>,
    urgent: Option<bool>,
    hidden: Option<bool>,
}

/// Pure reducer tracking workspaces with atomic batching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceReducer {
    workspaces: BTreeMap<String, Workspace>,
    pending: HashMap<String, PendingWorkspace>,
    committed: Vec<Workspace>,
}

impl WorkspaceReducer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn workspace_created(&mut self, id: String) {
        if !self.workspaces.contains_key(&id) {
            self.workspaces.insert(
                id.clone(),
                Workspace {
                    id: id.clone(),
                    name: String::new(),
                    coordinates: Vec::new(),
                    active: false,
                    urgent: false,
                    hidden: false,
                },
            );
        }
        self.pending.entry(id).or_default();
    }

    pub fn set_name(&mut self, id: &str, name: String) {
        self.pending.entry(id.to_owned()).or_default().name = Some(name);
    }

    pub fn set_coordinates(&mut self, id: &str, coordinates: Vec<u32>) {
        self.pending.entry(id.to_owned()).or_default().coordinates = Some(coordinates);
    }

    pub fn set_state(&mut self, id: &str, active: bool, urgent: bool, hidden: bool) {
        let entry = self.pending.entry(id.to_owned()).or_default();
        entry.active = Some(active);
        entry.urgent = Some(urgent);
        entry.hidden = Some(hidden);
    }

    pub fn workspace_removed(&mut self, id: &str) {
        self.workspaces.remove(id);
        self.pending.remove(id);
    }

    /// Flushes all pending workspace updates. Returns true if the visible list changed.
    pub fn commit_done(&mut self) -> bool {
        for (id, pending) in self.pending.drain() {
            if let Some(ws) = self.workspaces.get_mut(&id) {
                if let Some(name) = pending.name {
                    ws.name = name;
                }
                if let Some(coordinates) = pending.coordinates {
                    ws.coordinates = coordinates;
                }
                if let Some(active) = pending.active {
                    ws.active = active;
                }
                if let Some(urgent) = pending.urgent {
                    ws.urgent = urgent;
                }
                if let Some(hidden) = pending.hidden {
                    ws.hidden = hidden;
                }
            }
        }

        let mut list: Vec<Workspace> = self
            .workspaces
            .values()
            .filter(|ws| !ws.hidden)
            .cloned()
            .collect();

        list.sort_by(
            |left, right| match (left.coordinates.first(), right.coordinates.first()) {
                (Some(a), Some(b)) if a != b => a.cmp(b),
                _ => {
                    let left_num = left.name.parse::<u32>().ok();
                    let right_num = right.name.parse::<u32>().ok();
                    match (left_num, right_num) {
                        (Some(a), Some(b)) if a != b => a.cmp(&b),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        _ => left.name.cmp(&right.name).then(left.id.cmp(&right.id)),
                    }
                }
            },
        );

        let changed = list != self.committed;
        self.committed = list;
        changed
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.committed
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PendingToplevel {
    title: Option<String>,
    app_id: Option<String>,
    activated: Option<bool>,
    maximized: Option<bool>,
    minimized: Option<bool>,
    fullscreen: Option<bool>,
}

/// Pure reducer tracking foreign toplevels with atomic batching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToplevelReducer {
    toplevels: BTreeMap<String, Toplevel>,
    pending: HashMap<String, PendingToplevel>,
    active_title: Option<String>,
}

impl ToplevelReducer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn toplevel_created(&mut self, id: String) {
        if !self.toplevels.contains_key(&id) {
            self.toplevels.insert(
                id.clone(),
                Toplevel {
                    id: id.clone(),
                    title: String::new(),
                    app_id: String::new(),
                    activated: false,
                    maximized: false,
                    minimized: false,
                    fullscreen: false,
                },
            );
        }
        self.pending.entry(id).or_default();
    }

    pub fn set_title(&mut self, id: &str, title: String) {
        self.pending.entry(id.to_owned()).or_default().title = Some(title);
    }

    pub fn set_app_id(&mut self, id: &str, app_id: String) {
        self.pending.entry(id.to_owned()).or_default().app_id = Some(app_id);
    }

    pub fn set_state(
        &mut self,
        id: &str,
        activated: bool,
        maximized: bool,
        minimized: bool,
        fullscreen: bool,
    ) {
        let entry = self.pending.entry(id.to_owned()).or_default();
        entry.activated = Some(activated);
        entry.maximized = Some(maximized);
        entry.minimized = Some(minimized);
        entry.fullscreen = Some(fullscreen);
    }

    /// Commits pending changes for `id`. Returns true if the focused window title changed.
    pub fn commit_toplevel(&mut self, id: &str) -> bool {
        if let Some(pending) = self.pending.remove(id) {
            if let Some(toplevel) = self.toplevels.get_mut(id) {
                if let Some(title) = pending.title {
                    toplevel.title = title;
                }
                if let Some(app_id) = pending.app_id {
                    toplevel.app_id = app_id;
                }
                if let Some(activated) = pending.activated {
                    toplevel.activated = activated;
                }
                if let Some(maximized) = pending.maximized {
                    toplevel.maximized = maximized;
                }
                if let Some(minimized) = pending.minimized {
                    toplevel.minimized = minimized;
                }
                if let Some(fullscreen) = pending.fullscreen {
                    toplevel.fullscreen = fullscreen;
                }
            }
        }
        self.update_active_title()
    }

    pub fn toplevel_closed(&mut self, id: &str) -> bool {
        self.toplevels.remove(id);
        self.pending.remove(id);
        self.update_active_title()
    }

    fn update_active_title(&mut self) -> bool {
        let new_active = self
            .toplevels
            .values()
            .find(|toplevel| toplevel.activated)
            .map(|toplevel| {
                if !toplevel.title.trim().is_empty() {
                    toplevel.title.clone()
                } else {
                    toplevel.app_id.clone()
                }
            });
        let changed = new_active != self.active_title;
        self.active_title = new_active;
        changed
    }

    pub fn active_title(&self) -> Option<&str> {
        self.active_title.as_deref()
    }
}

/// Live Wayland taskbar integration tracking compositor workspaces and active windows.
#[derive(Default)]
pub struct Taskbar {
    workspace_manager: Option<ext_workspace_manager_v1::ExtWorkspaceManagerV1>,
    _toplevel_manager: Option<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1>,
    workspace_handles: HashMap<String, ext_workspace_handle_v1::ExtWorkspaceHandleV1>,
    workspace_objects: HashMap<ObjectId, String>,
    toplevel_objects: HashMap<ObjectId, String>,
    pub workspaces: WorkspaceReducer,
    pub toplevels: ToplevelReducer,
}

impl Taskbar {
    pub fn bind<D>(globals: &GlobalList, queue_handle: &QueueHandle<D>) -> Self
    where
        D: Dispatch<ext_workspace_manager_v1::ExtWorkspaceManagerV1, ()>
            + Dispatch<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()>
            + Dispatch<ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()>
            + Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()>
            + Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()>
            + 'static,
    {
        let workspace_manager = globals
            .bind::<ext_workspace_manager_v1::ExtWorkspaceManagerV1, _, _>(queue_handle, 1..=1, ())
            .ok();
        let _toplevel_manager = globals
            .bind::<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, _, _>(
                queue_handle,
                1..=3,
                (),
            )
            .ok();

        Self {
            workspace_manager,
            _toplevel_manager,
            workspace_handles: HashMap::new(),
            workspace_objects: HashMap::new(),
            toplevel_objects: HashMap::new(),
            workspaces: WorkspaceReducer::new(),
            toplevels: ToplevelReducer::new(),
        }
    }

    pub fn activate_workspace(&self, id: &str) {
        if let (Some(handle), Some(manager)) =
            (self.workspace_handles.get(id), &self.workspace_manager)
        {
            handle.activate();
            manager.commit();
        }
    }

    pub fn on_workspace_created(
        &mut self,
        handle: ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    ) -> String {
        let object_id = handle.id();
        let id = format!("ws_{}", object_id.protocol_id());
        self.workspace_objects.insert(object_id, id.clone());
        self.workspace_handles.insert(id.clone(), handle);
        self.workspaces.workspace_created(id.clone());
        id
    }

    pub fn on_workspace_name(&mut self, object_id: &ObjectId, name: String) {
        if let Some(id) = self.workspace_objects.get(object_id) {
            self.workspaces.set_name(id, name);
        }
    }

    pub fn on_workspace_coordinates(&mut self, object_id: &ObjectId, coordinates: Vec<u32>) {
        if let Some(id) = self.workspace_objects.get(object_id) {
            self.workspaces.set_coordinates(id, coordinates);
        }
    }

    pub fn on_workspace_state(
        &mut self,
        object_id: &ObjectId,
        active: bool,
        urgent: bool,
        hidden: bool,
    ) {
        if let Some(id) = self.workspace_objects.get(object_id) {
            self.workspaces.set_state(id, active, urgent, hidden);
        }
    }

    pub fn on_workspace_removed(&mut self, object_id: &ObjectId) {
        if let Some(id) = self.workspace_objects.remove(object_id) {
            self.workspace_handles.remove(&id);
            self.workspaces.workspace_removed(&id);
        }
    }

    pub fn on_workspace_done(&mut self) -> bool {
        self.workspaces.commit_done()
    }

    pub fn on_toplevel_created(
        &mut self,
        handle: zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    ) -> String {
        let object_id = handle.id();
        let id = format!("top_{}", object_id.protocol_id());
        self.toplevel_objects.insert(object_id, id.clone());
        self.toplevels.toplevel_created(id.clone());
        id
    }

    pub fn on_toplevel_title(&mut self, object_id: &ObjectId, title: String) {
        if let Some(id) = self.toplevel_objects.get(object_id) {
            self.toplevels.set_title(id, title);
        }
    }

    pub fn on_toplevel_app_id(&mut self, object_id: &ObjectId, app_id: String) {
        if let Some(id) = self.toplevel_objects.get(object_id) {
            self.toplevels.set_app_id(id, app_id);
        }
    }

    pub fn on_toplevel_state(
        &mut self,
        object_id: &ObjectId,
        activated: bool,
        maximized: bool,
        minimized: bool,
        fullscreen: bool,
    ) {
        if let Some(id) = self.toplevel_objects.get(object_id) {
            self.toplevels
                .set_state(id, activated, maximized, minimized, fullscreen);
        }
    }

    pub fn on_toplevel_done(&mut self, object_id: &ObjectId) -> bool {
        if let Some(id) = self.toplevel_objects.get(object_id) {
            self.toplevels.commit_toplevel(id)
        } else {
            false
        }
    }

    pub fn on_toplevel_closed(&mut self, object_id: &ObjectId) -> bool {
        if let Some(id) = self.toplevel_objects.remove(object_id) {
            self.toplevels.toplevel_closed(&id)
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_should_sort_by_coordinate_then_numeric_name() {
        let mut reducer = WorkspaceReducer::new();

        reducer.workspace_created("ws_1".into());
        reducer.set_name("ws_1", "10".into());
        reducer.set_coordinates("ws_1", vec![2]);

        reducer.workspace_created("ws_2".into());
        reducer.set_name("ws_2", "2".into());
        reducer.set_coordinates("ws_2", vec![1]);

        reducer.workspace_created("ws_3".into());
        reducer.set_name("ws_3", "1".into());
        reducer.set_coordinates("ws_3", vec![1]); // tie-break by name "1" < "2"

        reducer.workspace_created("ws_hidden".into());
        reducer.set_name("ws_hidden", "hidden".into());
        reducer.set_state("ws_hidden", false, false, true);

        assert!(reducer.commit_done(), "initial commit should report change");
        let list = reducer.workspaces();
        assert_eq!(list.len(), 3, "hidden workspaces should be omitted");
        assert_eq!(list[0].name, "1");
        assert_eq!(list[1].name, "2");
        assert_eq!(list[2].name, "10");
    }

    #[test]
    fn workspaces_should_track_active_and_urgent_states() {
        let mut reducer = WorkspaceReducer::new();

        reducer.workspace_created("ws_1".into());
        reducer.set_name("ws_1", "1".into());
        reducer.set_state("ws_1", true, false, false);

        reducer.workspace_created("ws_2".into());
        reducer.set_name("ws_2", "2".into());
        reducer.set_state("ws_2", false, true, false);

        reducer.commit_done();
        let list = reducer.workspaces();
        assert!(list[0].active);
        assert!(!list[0].urgent);
        assert!(!list[1].active);
        assert!(list[1].urgent);

        // Switch active workspace
        reducer.set_state("ws_1", false, false, false);
        reducer.set_state("ws_2", true, false, false);
        assert!(reducer.commit_done());
        assert!(!reducer.workspaces()[0].active);
        assert!(reducer.workspaces()[1].active);
    }

    #[test]
    fn toplevel_reducer_should_track_activated_window_title() {
        let mut reducer = ToplevelReducer::new();

        reducer.toplevel_created("top_1".into());
        reducer.set_title("top_1", "Firefox".into());
        reducer.set_app_id("top_1", "firefox".into());
        reducer.set_state("top_1", true, false, false, false);

        assert!(reducer.commit_toplevel("top_1"));
        assert_eq!(reducer.active_title(), Some("Firefox"));

        // Second window opens and takes focus
        reducer.toplevel_created("top_2".into());
        reducer.set_title("top_2", "Terminal".into());
        reducer.set_state("top_2", true, false, false, false);
        reducer.set_state("top_1", false, false, false, false);

        reducer.commit_toplevel("top_1");
        assert!(reducer.commit_toplevel("top_2"));
        assert_eq!(reducer.active_title(), Some("Terminal"));

        // Focus closed -> fallback to none
        assert!(reducer.toplevel_closed("top_2"));
        assert_eq!(reducer.active_title(), None);
    }

    #[test]
    fn toplevel_should_fallback_to_app_id_when_title_is_empty() {
        let mut reducer = ToplevelReducer::new();

        reducer.toplevel_created("top_1".into());
        reducer.set_app_id("top_1", "org.wezfurlong.wezterm".into());
        reducer.set_state("top_1", true, false, false, false);

        assert!(reducer.commit_toplevel("top_1"));
        assert_eq!(reducer.active_title(), Some("org.wezfurlong.wezterm"));
    }
}
