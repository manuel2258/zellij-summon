use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

register_plugin!(State);

// ── logging ───────────────────────────────────────────────────────────────────

fn log(level: &str, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/zellij-pane-manager.log")
    {
        let _ = writeln!(f, "[{}] {}", level, msg);
    }
}

macro_rules! log_debug { ($($t:tt)*) => { log("DEBUG", &format!($($t)*)) }; }
macro_rules! log_info  { ($($t:tt)*) => { log("INFO",  &format!($($t)*)) }; }
macro_rules! log_warn  { ($($t:tt)*) => { log("WARN",  &format!($($t)*)) }; }
macro_rules! log_error { ($($t:tt)*) => { log("ERROR", &format!($($t)*)) }; }

// ── state ─────────────────────────────────────────────────────────────────────

/// Each managed pane toggles on successive keybind presses:
///   HIDDEN → SHOWN (press to show), SHOWN → HIDDEN (press again to close)
///
/// Only one managed pane is shown at a time. Triggering a different pane
/// simply focuses it (bringing it to the top of the overlapping floating
/// layer) without explicitly hiding the previous one.
///
/// # Communication
///
/// Keybinds use `MessagePlugin` (no URL, broadcast) with `name "toggle"` and
/// `payload "<pane_name>"`. The server broadcasts the PipeMessage to all running
/// plugins; ours receives it in `pipe()` and toggles the named pane. A single
/// plugin instance in the layout handles all managed panes.
#[derive(Default)]
struct State {
    /// pane name → runtime PaneId; built lazily from PaneUpdate events
    pane_map: HashMap<String, PaneId>,
    /// ordered list of managed pane names, parsed from plugin config
    managed_panes: Vec<String>,
    /// which pane the plugin last showed; None means floating layer is hidden
    active_pane: Option<String>,
    /// queued target from a pipe message that arrived before the target pane was
    /// discoverable. Retried on every subsequent PaneUpdate until found.
    pending_target: Option<String>,
    /// whether first-time setup (permissions + subscription) has been done
    initialized: bool,
    /// tab index this plugin lives in; used to exclude panes from other tabs
    own_tab_index: Option<usize>,
}

/// A side-effect that process_target_actions wants to produce.
/// The dispatcher (process_target) translates these into real shim calls.
#[derive(Debug, PartialEq)]
pub enum PaneAction {
    ShowPaneWithId(PaneId),
    HideFloatingLayer,
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // Parse the ordered managed-pane list from config keys pane_0_name, pane_1_name, …
        let mut managed = Vec::new();
        let mut i = 0;
        while let Some(name) = configuration.get(&format!("pane_{i}_name")) {
            managed.push(name.clone());
            i += 1;
        }
        self.managed_panes = managed;
        log_info!("load: managed_panes = {:?}", self.managed_panes);

        // Prune stale entries left over from a previous config (rename, removal).
        let current = self.managed_panes.clone();
        self.pane_map.retain(|name, _| current.contains(name));
        if let Some(ref active) = self.active_pane {
            if !current.contains(active) {
                self.active_pane = None;
            }
        }

        if !self.initialized {
            self.initialized = true;
            log_info!("load: first init — requesting permissions");
            request_permission(&[
                PermissionType::ReadApplicationState,
                PermissionType::ChangeApplicationState,
            ]);
            subscribe(&[EventType::PermissionRequestResult]);
        }
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                log_info!("permissions granted — subscribing to PaneUpdate");
                subscribe(&[EventType::PaneUpdate]);
            }
            Event::PermissionRequestResult(status) => {
                log_error!(
                    "permission request failed: {:?} — plugin will not function",
                    status
                );
            }
            Event::PaneUpdate(manifest) => {
                let tab_count = manifest.panes.len();
                self.rebuild_pane_map(manifest);
                log_debug!(
                    "PaneUpdate: {} tab(s), own_tab={:?}, pane_map has {} entries",
                    tab_count,
                    self.own_tab_index,
                    self.pane_map.len()
                );
                // Retry pending target now that pane_map may have grown.
                // Only take() if the target was actually found to avoid losing it.
                if let Some(ref target) = self.pending_target.clone() {
                    if self.pane_map.contains_key(target.as_str()) {
                        log_info!("pending_target '{}' now discoverable, processing", target);
                        let t = self.pending_target.take().unwrap();
                        self.process_target(&t);
                    } else {
                        log_debug!(
                            "pending_target '{}' still not in pane_map, will retry on next PaneUpdate",
                            target
                        );
                    }
                }
            }
            _ => {}
        }
        false // headless — nothing to render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.name == "toggle" {
            if let Some(target) = pipe_message.payload {
                log_info!("pipe: toggle '{}'", target);
                if self.pane_map.contains_key(&target) {
                    self.process_target(&target);
                } else {
                    log_warn!(
                        "pipe: pane '{}' not yet in pane_map (managed: {:?}), queuing as pending_target",
                        target,
                        self.managed_panes
                    );
                    self.pending_target = Some(target);
                }
            } else {
                log_warn!("pipe: 'toggle' message has no payload — ignoring");
            }
        } else {
            log_debug!(
                "pipe: ignoring message name='{}' payload={:?}",
                pipe_message.name,
                pipe_message.payload
            );
        }
        false // headless — nothing to render
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // intentionally empty: this plugin is headless (size=1 borderless=true)
    }
}

impl State {
    /// Refresh pane_map from a fresh PaneManifest.
    ///
    /// Only panes in our own tab are considered — avoids adopting panes from
    /// other tabs whose titles happen to match a managed pane name.
    ///
    /// Lifecycle:
    /// 1. Locate our own tab by finding the pane whose plugin id matches ours.
    /// 2. Remove entries for panes no longer present in our tab (externally closed).
    ///    active_pane is cleared if the active pane was evicted.
    /// 3. Discover panes not yet mapped by matching their current title to a
    ///    configured managed-pane name. After first discovery, panes are tracked
    ///    by PaneId so title changes don't lose the mapping.
    fn rebuild_pane_map(&mut self, manifest: PaneManifest) {
        // Step 1: locate our own tab. Skipped in test builds (own_tab_index set directly).
        #[cfg(not(test))]
        {
            let own_plugin_id = get_plugin_ids().plugin_id;
            for (tab_idx, panes) in &manifest.panes {
                if panes.iter().any(|p| p.is_plugin && p.id == own_plugin_id) {
                    if self.own_tab_index != Some(*tab_idx) {
                        log_info!("plugin located in tab {}", tab_idx);
                        self.own_tab_index = Some(*tab_idx);
                    }
                    break;
                }
            }
        }

        // Step 2: filter to own tab panes; fall back to all tabs if tab not found yet.
        let tab_panes: Vec<PaneInfo> = match self.own_tab_index {
            Some(idx) => manifest.panes.get(&idx).cloned().unwrap_or_default(),
            None => {
                log_warn!(
                    "own tab not yet found — scanning all tabs (cross-tab contamination risk)"
                );
                manifest.panes.into_values().flatten().collect()
            }
        };

        // Step 3: remove entries for panes that no longer appear in the manifest.
        // Use (is_plugin, id) pairs to avoid collision between Terminal and Plugin
        // panes that share the same numeric id.
        let live_ids: HashSet<(bool, u32)> =
            tab_panes.iter().map(|p| (p.is_plugin, p.id)).collect();

        let evicted: Vec<String> = self
            .pane_map
            .iter()
            .filter(|(_, pid)| {
                let key = match **pid {
                    PaneId::Terminal(id) => (false, id),
                    PaneId::Plugin(id) => (true, id),
                };
                !live_ids.contains(&key)
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in &evicted {
            log_warn!("pane '{}' evicted (no longer in manifest)", name);
            if self.active_pane.as_deref() == Some(name.as_str()) {
                log_info!("active pane '{}' evicted — clearing active_pane", name);
                self.active_pane = None;
            }
        }

        self.pane_map.retain(|_, pid| {
            let key = match *pid {
                PaneId::Terminal(id) => (false, id),
                PaneId::Plugin(id) => (true, id),
            };
            live_ids.contains(&key)
        });

        // Step 4: discover panes not yet in pane_map by matching their title.
        let unmapped: Vec<String> = self
            .managed_panes
            .iter()
            .filter(|n| !self.pane_map.contains_key(*n))
            .cloned()
            .collect();

        for name in unmapped {
            if let Some(pane) = tab_panes.iter().find(|p| p.title == name) {
                let pid = make_pane_id(pane);
                log_info!("discovered pane '{}' → {:?}", name, pid);
                self.pane_map.insert(name, pid);
            }
        }
    }

    /// Pure state-machine core: computes which pane API calls are needed for a
    /// keybind trigger and mutates `self` accordingly. Returns the actions in
    /// dispatch order; the caller is responsible for executing them.
    ///
    /// State transitions:
    ///   active == target  → HideFloatingLayer  (toggle off)
    ///   active != target  → ShowPaneWithId      (focus / toggle on)
    fn process_target_actions(&mut self, target: &str) -> Vec<PaneAction> {
        if !self.pane_map.contains_key(target) {
            log_warn!("process_target_actions: '{}' not in pane_map", target);
            return vec![];
        }

        if self.active_pane.as_deref() == Some(target) {
            log_info!(
                "process_target_actions: '{}' active → hiding floating layer",
                target
            );
            self.active_pane = None;
            vec![PaneAction::HideFloatingLayer]
        } else {
            let pid = *self.pane_map.get(target).unwrap();
            log_info!("process_target_actions: showing '{}' ({:?})", target, pid);
            self.active_pane = Some(target.to_string());
            vec![PaneAction::ShowPaneWithId(pid)]
        }
    }

    /// Dispatch the actions returned by process_target_actions to the Zellij shim.
    #[cfg(not(test))]
    fn process_target(&mut self, target: &str) {
        for action in self.process_target_actions(target) {
            match action {
                PaneAction::HideFloatingLayer => {
                    log_debug!("dispatch HideFloatingLayer");
                    let _ = hide_floating_panes(None);
                }
                PaneAction::ShowPaneWithId(pid) => {
                    log_debug!("dispatch ShowPaneWithId({:?})", pid);
                    show_pane_with_id(pid, true, true);
                }
            }
        }
    }

    // In test builds the shim functions are unavailable (WASM host imports).
    // Run process_target_actions for its state-mutation side effects only.
    #[cfg(test)]
    fn process_target(&mut self, target: &str) {
        let _ = self.process_target_actions(target);
    }
}

fn make_pane_id(pane: &PaneInfo) -> PaneId {
    if pane.is_plugin {
        PaneId::Plugin(pane.id)
    } else {
        PaneId::Terminal(pane.id)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_pane(id: u32, is_plugin: bool, title: &str) -> PaneInfo {
        PaneInfo {
            id,
            is_plugin,
            title: title.to_string(),
            ..Default::default()
        }
    }

    /// Wrap a flat list of panes into a PaneManifest under tab index 0.
    fn make_manifest(panes: Vec<PaneInfo>) -> PaneManifest {
        PaneManifest {
            panes: HashMap::from([(0usize, panes)]),
        }
    }

    /// A State with one known pane (not active), own_tab_index set.
    fn state_with_pane(name: &str, id: u32) -> State {
        let mut s = State::default();
        s.managed_panes = vec![name.to_string()];
        s.pane_map.insert(name.to_string(), PaneId::Terminal(id));
        s.own_tab_index = Some(0);
        s
    }

    /// A State with one known pane that is the active (shown) pane.
    fn state_with_active_pane(name: &str, id: u32) -> State {
        let mut s = state_with_pane(name, id);
        s.active_pane = Some(name.to_string());
        s
    }

    // ── process_target_actions tests ─────────────────────────────────────────

    #[test]
    fn first_trigger_shows_pane() {
        // HIDDEN → SHOWN: pane not active, pressing key shows it
        let mut s = state_with_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert_eq!(
            actions,
            vec![PaneAction::ShowPaneWithId(PaneId::Terminal(1))]
        );
        assert_eq!(s.active_pane, Some("term".to_string()));
    }

    #[test]
    fn second_trigger_hides_layer() {
        // SHOWN → HIDDEN: pane is active, pressing same key hides floating layer
        let mut s = state_with_active_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::HideFloatingLayer]);
        assert_eq!(s.active_pane, None);
    }

    #[test]
    fn switching_pane_shows_new_pane() {
        // Triggering a different pane focuses it without hiding the layer
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.active_pane = Some("alpha".to_string());
        s.own_tab_index = Some(0);

        let actions = s.process_target_actions("beta");

        assert_eq!(
            actions,
            vec![PaneAction::ShowPaneWithId(PaneId::Terminal(20))]
        );
        assert_eq!(s.active_pane, Some("beta".to_string()));
    }

    #[test]
    fn unknown_target_is_noop() {
        // A target not in pane_map must leave all state unchanged.
        let mut s = state_with_active_pane("known", 1);

        let actions = s.process_target_actions("ghost");

        assert!(actions.is_empty());
        assert_eq!(s.active_pane, Some("known".to_string())); // untouched
    }

    // ── rebuild_pane_map tests ───────────────────────────────────────────────

    #[test]
    fn rebuild_discovers_pane_by_title() {
        // An unmapped managed pane is found by matching its title in the manifest.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);

        s.rebuild_pane_map(make_manifest(vec![make_pane(42, false, "broot")]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
    }

    #[test]
    fn rebuild_pane_map_is_stable_after_title_change() {
        // Once a pane is in pane_map, its entry is preserved even if the terminal
        // application overwrites the pane title — tracking is by PaneId, not title.
        let mut s = state_with_pane("broot", 42);

        s.rebuild_pane_map(make_manifest(vec![make_pane(
            42,
            false,
            "broot - /home/user",
        )]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
    }

    #[test]
    fn rebuild_removes_closed_pane_and_clears_active() {
        // When a pane disappears from the manifest, its entry must be evicted and
        // active_pane must be cleared if it was the active pane.
        let mut s = state_with_active_pane("broot", 42);

        s.rebuild_pane_map(make_manifest(vec![]));

        assert!(!s.pane_map.contains_key("broot"));
        assert_eq!(s.active_pane, None);
    }

    #[test]
    fn rebuild_eviction_leaves_other_active_pane_intact() {
        // Evicting a non-active pane must not clear active_pane.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.active_pane = Some("beta".to_string());
        s.own_tab_index = Some(0);

        // alpha disappears, beta stays
        s.rebuild_pane_map(make_manifest(vec![make_pane(20, false, "beta")]));

        assert!(!s.pane_map.contains_key("alpha"));
        assert_eq!(s.active_pane, Some("beta".to_string())); // untouched
    }

    #[test]
    fn rebuild_ignores_panes_from_other_tabs() {
        // When own_tab_index is Some(0), a matching pane in tab 1 must not be adopted.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);

        let manifest = PaneManifest {
            panes: HashMap::from([
                (0usize, vec![]),                              // own tab — no broot
                (1usize, vec![make_pane(42, false, "broot")]), // other tab — has broot
            ]),
        };
        s.rebuild_pane_map(manifest);

        assert!(
            !s.pane_map.contains_key("broot"),
            "pane from another tab must not be adopted"
        );
    }

    #[test]
    fn rebuild_pane_id_collision_terminal_plugin_same_numeric_id() {
        // A Plugin pane and a Terminal pane with the same numeric id must not
        // interfere — the Terminal entry must be evicted correctly when only the
        // Plugin pane survives.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);
        // Pre-populate with a Terminal pane at id=5
        s.pane_map.insert("broot".to_string(), PaneId::Terminal(5));

        // Manifest now contains a Plugin pane at id=5 (different kind, same number)
        s.rebuild_pane_map(make_manifest(vec![make_pane(5, true, "something_else")]));

        // Terminal(5) is gone; broot entry must be evicted since Terminal(5) is not live
        assert!(!s.pane_map.contains_key("broot"));
    }

    #[test]
    fn pending_target_retained_across_pane_update_when_not_found() {
        // If the target pane is not yet discoverable, pending_target must survive
        // the PaneUpdate so it can be retried on the next one.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);
        s.pending_target = Some("broot".to_string());

        // PaneUpdate with no matching pane
        s.rebuild_pane_map(make_manifest(vec![]));

        // Simulate the pending_target check from update() manually
        let should_process = s.pane_map.contains_key("broot");
        assert!(!should_process, "pane not found — must not process yet");
        assert_eq!(s.pending_target, Some("broot".to_string()));
    }

    #[test]
    fn pending_target_processed_when_pane_later_discovered() {
        // After a second PaneUpdate that brings the pane into scope,
        // the pending_target should be resolvable.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);

        // First PaneUpdate: pane not yet present
        s.rebuild_pane_map(make_manifest(vec![]));
        assert!(s.pane_map.is_empty());

        // Second PaneUpdate: pane now present
        s.rebuild_pane_map(make_manifest(vec![make_pane(42, false, "broot")]));
        assert_eq!(
            s.pane_map.get("broot"),
            Some(&PaneId::Terminal(42)),
            "pane must be discoverable on second PaneUpdate"
        );
    }
}
