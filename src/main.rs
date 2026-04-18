use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

register_plugin!(State);

/// Each managed pane cycles through three states on successive keybind presses:
///   HIDDEN → SHOWN (unpinned) → SHOWN (pinned) → HIDDEN
///
/// "Pinned" here is tracked by the plugin, not by Zellij's native floating-pane
/// pin feature (which has no plugin API as of zellij-tile 0.42). The practical
/// effect is that a pinned pane requires two more keybind presses to dismiss,
/// guarding against accidental closure.
///
/// Only one managed pane is shown at a time. Triggering a different pane hides
/// the current one (regardless of pin state) and shows the new one.
#[derive(Default)]
struct State {
    /// pane name → runtime PaneId; built lazily from PaneUpdate events
    pane_map: HashMap<String, PaneId>,
    /// pane name → latest PaneInfo snapshot
    pane_info: HashMap<String, PaneInfo>,
    /// ordered list of managed pane names, parsed from plugin config
    managed_panes: Vec<String>,
    /// which managed pane is currently visible (if any)
    shown_pane: Option<String>,
    /// which managed panes are in the "pinned" state
    pinned_panes: HashSet<String>,
    /// target from the most recent load() call, consumed in next PaneUpdate
    pending_target: Option<String>,
    /// whether first-time setup (permissions + subscription) has been done
    initialized: bool,
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // Parse the ordered managed-pane list from config keys pane_0_name, pane_1_name, …
        let mut managed = Vec::new();
        let mut i = 0;
        loop {
            match configuration.get(&format!("pane_{i}_name")) {
                Some(name) => {
                    managed.push(name.clone());
                    i += 1;
                }
                None => break,
            }
        }
        self.managed_panes = managed;
        self.pending_target = configuration.get("target").cloned();

        if !self.initialized {
            self.initialized = true;
            request_permission(&[
                PermissionType::ReadApplicationState,
                PermissionType::ChangeApplicationState,
            ]);
            // PermissionRequestResult triggers subscription to PaneUpdate
            subscribe(&[EventType::PermissionRequestResult]);
        } else if !self.pane_map.is_empty() {
            // Plugin already running and pane map is warm — process immediately
            if let Some(target) = self.pending_target.take() {
                self.process_target(&target);
            }
        }
        // If pane_map is empty on a subsequent load(), the pending_target will
        // be consumed once the next PaneUpdate populates the map.
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                subscribe(&[EventType::PaneUpdate]);
            }
            Event::PaneUpdate(manifest) => {
                self.rebuild_pane_map(manifest);
                if let Some(target) = self.pending_target.take() {
                    self.process_target(&target);
                }
            }
            _ => {}
        }
        false // headless — nothing to render
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // intentionally empty: this plugin is headless (size=1 borderless=true)
    }
}

impl State {
    /// Refresh pane_map and pane_info from a fresh PaneManifest.
    ///
    /// Already-discovered panes are identified by their PaneId so that title
    /// changes (e.g. broot updating the terminal title) don't lose tracking.
    /// Newly-seen managed panes are matched by their current title against the
    /// configured name — this succeeds on the first PaneUpdate after startup
    /// when the title still matches the layout `name` field.
    fn rebuild_pane_map(&mut self, manifest: PaneManifest) {
        let all_panes: Vec<PaneInfo> = manifest.panes.into_values().flatten().collect();

        // Update info snapshots for panes we already know by ID
        for pane in &all_panes {
            let mapped_name = self
                .pane_map
                .iter()
                .find(|(_, &pid)| pane_id_matches(pid, pane))
                .map(|(name, _)| name.clone());
            if let Some(name) = mapped_name {
                self.pane_info.insert(name, pane.clone());
            }
        }

        // Discover panes not yet in pane_map by matching their title to a
        // configured managed-pane name
        let unmapped: Vec<String> = self
            .managed_panes
            .iter()
            .filter(|n| !self.pane_map.contains_key(*n))
            .cloned()
            .collect();

        for name in unmapped {
            if let Some(pane) = all_panes.iter().find(|p| p.title == name) {
                let pid = make_pane_id(pane);
                self.pane_map.insert(name.clone(), pid);
                self.pane_info.insert(name, pane.clone());
            }
        }
    }

    /// Core state-machine logic for a keybind trigger naming `target`.
    ///
    /// State transitions:
    ///   HIDDEN            → (trigger) → SHOWN unpinned
    ///   SHOWN unpinned    → (trigger) → SHOWN pinned
    ///   SHOWN pinned      → (trigger) → HIDDEN
    ///   SHOWN (any state) → (other)   → HIDDEN; other pane → SHOWN unpinned
    fn process_target(&mut self, target: &str) {
        if !self.pane_map.contains_key(target) {
            // Target pane not yet discovered — ignore; will retry on next PaneUpdate
            // if pending_target is still set. Since we already took it, the user
            // must press the keybind again. Acceptable for first-trigger edge case.
            return;
        }

        // Triggering the same pane that's already shown
        if self.shown_pane.as_deref() == Some(target) {
            let is_pinned = self.pinned_panes.contains(target);
            let pid = *self.pane_map.get(target).unwrap();
            if is_pinned {
                // SHOWN pinned → HIDDEN
                hide_pane_with_id(pid);
                self.pinned_panes.remove(target);
                self.shown_pane = None;
            } else {
                // SHOWN unpinned → SHOWN pinned (no Zellij pin API; tracked internally)
                self.pinned_panes.insert(target.to_string());
            }
            return;
        }

        // Triggering a different pane — hide the current one first
        if let Some(shown) = self.shown_pane.take() {
            if let Some(shown_pid) = self.pane_map.get(&shown).copied() {
                hide_pane_with_id(shown_pid);
            }
            self.pinned_panes.remove(&shown);
        }

        // Show target pane (always starts unpinned); float it if it was embedded
        let target_pid = *self.pane_map.get(target).unwrap();
        show_pane_with_id(target_pid, true);
        self.shown_pane = Some(target.to_string());
    }
}

fn pane_id_matches(pid: PaneId, pane: &PaneInfo) -> bool {
    match pid {
        PaneId::Terminal(id) => id == pane.id && !pane.is_plugin,
        PaneId::Plugin(id) => id == pane.id && pane.is_plugin,
    }
}

fn make_pane_id(pane: &PaneInfo) -> PaneId {
    if pane.is_plugin {
        PaneId::Plugin(pane.id)
    } else {
        PaneId::Terminal(pane.id)
    }
}
