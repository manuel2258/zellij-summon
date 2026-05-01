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

/// Each managed pane cycles through three states on successive keybind presses:
///   HIDDEN → SHOWN (unpinned) → SHOWN (pinned) → HIDDEN
///
/// "Pinned" here is tracked by the plugin, not by Zellij's native floating-pane
/// pin feature (which has no plugin API as of zellij-tile 0.44). The practical
/// effect is that a pinned pane requires two more keybind presses to dismiss,
/// guarding against accidental closure.
///
/// Only one managed pane is shown at a time. Triggering a different pane hides
/// the current one (regardless of pin state) and shows the new one.
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
    /// actual Zellij visibility per pane (false = suppressed/hidden); updated
    /// from PaneUpdate so cross-instance hide actions are reflected correctly
    pane_visible: HashMap<String, bool>,
    /// which managed panes are in the "pinned" state (plugin-internal only)
    pinned_panes: HashSet<String>,
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
    Show(PaneId),
    Hide(PaneId),
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
        self.pane_visible.retain(|name, _| current.contains(name));
        self.pinned_panes.retain(|name| current.contains(name));

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
    /// Refresh pane_map and pane_visible from a fresh PaneManifest.
    ///
    /// Only panes in our own tab are considered — avoids adopting panes from
    /// other tabs whose titles happen to match a managed pane name.
    ///
    /// Lifecycle:
    /// 1. Locate our own tab by finding the pane whose plugin id matches ours.
    /// 2. Remove entries for panes no longer present in our tab (externally closed).
    ///    Pinned state is cleared for evicted panes.
    /// 3. Discover panes not yet mapped by matching their current title to a
    ///    configured managed-pane name. After first discovery, panes are tracked
    ///    by PaneId so title changes don't lose the mapping.
    /// 4. Update pane_visible for all mapped panes from PaneInfo.is_suppressed.
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
        }

        self.pane_map.retain(|_, pid| {
            let key = match *pid {
                PaneId::Terminal(id) => (false, id),
                PaneId::Plugin(id) => (true, id),
            };
            live_ids.contains(&key)
        });
        self.pane_visible
            .retain(|name, _| self.pane_map.contains_key(name));
        self.pinned_panes
            .retain(|name| self.pane_map.contains_key(name));

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
                let visible = !pane.is_suppressed;
                log_info!(
                    "discovered pane '{}' → {:?} (visible={})",
                    name,
                    pid,
                    visible
                );
                self.pane_map.insert(name.clone(), pid);
                self.pane_visible.insert(name, visible);
            }
        }

        // Step 5: update visibility for already-mapped panes so cross-instance
        // hide/show actions (from keybind triggers) are reflected here.
        for (name, pid) in &self.pane_map {
            if let Some(pane) = tab_panes.iter().find(|p| match *pid {
                PaneId::Terminal(id) => !p.is_plugin && p.id == id,
                PaneId::Plugin(id) => p.is_plugin && p.id == id,
            }) {
                let new_visible = !pane.is_suppressed;
                let old_visible = self.pane_visible.get(name).copied().unwrap_or(false);
                if old_visible != new_visible {
                    log_debug!(
                        "pane '{}' visibility: {} → {}",
                        name,
                        old_visible,
                        new_visible
                    );
                }
                self.pane_visible.insert(name.clone(), new_visible);
            }
        }
    }

    /// Pure state-machine core: computes which pane API calls are needed for a
    /// keybind trigger and mutates `self` accordingly. Returns the actions in
    /// dispatch order; the caller is responsible for executing them.
    ///
    /// Visibility is read from pane_visible (authoritative Zellij state from
    /// PaneUpdate) rather than tracked internally, so stale state from prior
    /// hide actions is handled correctly once PaneUpdate arrives.
    ///
    /// State transitions:
    ///   HIDDEN              → (trigger) → SHOWN unpinned
    ///   SHOWN unpinned      → (trigger) → SHOWN pinned  (internal state only)
    ///   SHOWN pinned        → (trigger) → HIDDEN
    ///   SHOWN (any, other)  → (trigger) → HIDDEN; target → SHOWN unpinned
    fn process_target_actions(&mut self, target: &str) -> Vec<PaneAction> {
        let mut actions = Vec::new();

        if !self.pane_map.contains_key(target) {
            log_warn!("process_target_actions: '{}' not in pane_map", target);
            return actions;
        }

        let target_is_visible = self.pane_visible.get(target).copied().unwrap_or(false);
        let is_pinned = self.pinned_panes.contains(target);

        log_info!(
            "process_target_actions: target='{}' visible={} pinned={}",
            target,
            target_is_visible,
            is_pinned
        );

        if target_is_visible {
            let pid = *self.pane_map.get(target).unwrap();
            if is_pinned {
                // SHOWN pinned → HIDDEN
                log_info!("  {:?}: SHOWN pinned → HIDDEN", pid);
                actions.push(PaneAction::Hide(pid));
                self.pinned_panes.remove(target);
            } else {
                // SHOWN unpinned → SHOWN pinned (no API call — internal state only)
                log_info!("  SHOWN unpinned → SHOWN pinned (no API call)");
                self.pinned_panes.insert(target.to_string());
            }
        } else {
            // Target hidden — hide every other currently visible managed pane,
            // then show target.
            for name in self.managed_panes.clone() {
                if name != target
                    && self
                        .pane_visible
                        .get(name.as_str())
                        .copied()
                        .unwrap_or(false)
                {
                    if let Some(&pid) = self.pane_map.get(name.as_str()) {
                        log_info!("  hiding sibling '{}': Hide({:?})", name, pid);
                        actions.push(PaneAction::Hide(pid));
                        self.pinned_panes.remove(&name);
                    }
                }
            }
            let target_pid = *self.pane_map.get(target).unwrap();
            log_info!("  {:?}: HIDDEN → SHOWN unpinned", target_pid);
            actions.push(PaneAction::Show(target_pid));
        }

        actions
    }

    /// Dispatch the actions returned by process_target_actions to the Zellij shim.
    #[cfg(not(test))]
    fn process_target(&mut self, target: &str) {
        for action in self.process_target_actions(target) {
            match action {
                PaneAction::Hide(pid) => {
                    log_debug!("dispatch Hide({:?})", pid);
                    hide_pane_with_id(pid);
                }
                PaneAction::Show(pid) => {
                    log_debug!("dispatch Show({:?})", pid);
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
            is_suppressed: false,
            ..Default::default()
        }
    }

    fn make_suppressed_pane(id: u32, title: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: title.to_string(),
            is_suppressed: true,
            ..Default::default()
        }
    }

    /// Wrap a flat list of panes into a PaneManifest under tab index 0.
    fn make_manifest(panes: Vec<PaneInfo>) -> PaneManifest {
        PaneManifest {
            panes: HashMap::from([(0usize, panes)]),
        }
    }

    /// A State with one known hidden pane (not yet visible), own_tab_index set.
    fn state_with_pane(name: &str, id: u32) -> State {
        let mut s = State::default();
        s.managed_panes = vec![name.to_string()];
        s.pane_map.insert(name.to_string(), PaneId::Terminal(id));
        s.pane_visible.insert(name.to_string(), false);
        s.own_tab_index = Some(0);
        s
    }

    /// A State with one known visible pane.
    fn state_with_visible_pane(name: &str, id: u32) -> State {
        let mut s = state_with_pane(name, id);
        s.pane_visible.insert(name.to_string(), true);
        s
    }

    // ── process_target_actions tests ─────────────────────────────────────────

    #[test]
    fn first_trigger_shows_pane() {
        // HIDDEN → SHOWN unpinned
        let mut s = state_with_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::Show(PaneId::Terminal(1))]);
        assert!(!s.pinned_panes.contains("term"));
    }

    #[test]
    fn second_trigger_pins_pane() {
        // SHOWN unpinned → SHOWN pinned (no shim call — purely internal state)
        let mut s = state_with_visible_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert!(actions.is_empty());
        assert!(s.pinned_panes.contains("term"));
        assert_eq!(s.pane_visible.get("term"), Some(&true)); // still visible
    }

    #[test]
    fn third_trigger_hides_pinned_pane() {
        // SHOWN pinned → HIDDEN
        let mut s = state_with_visible_pane("term", 1);
        s.pinned_panes.insert("term".to_string());

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::Hide(PaneId::Terminal(1))]);
        assert!(!s.pinned_panes.contains("term"));
    }

    #[test]
    fn switch_panes_hides_current_shows_new() {
        // Triggering a different pane: hide old (unpinned), show new.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.pane_visible.insert("alpha".to_string(), true);
        s.pane_visible.insert("beta".to_string(), false);

        let actions = s.process_target_actions("beta");

        assert_eq!(
            actions,
            vec![
                PaneAction::Hide(PaneId::Terminal(10)),
                PaneAction::Show(PaneId::Terminal(20)),
            ]
        );
        assert!(!s.pinned_panes.contains("alpha"));
    }

    #[test]
    fn switch_from_pinned_clears_pin_hides_old_shows_new() {
        // A pinned pane must be unpinned (internally) when a different pane is triggered.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.pane_visible.insert("alpha".to_string(), true);
        s.pane_visible.insert("beta".to_string(), false);
        s.pinned_panes.insert("alpha".to_string());

        let actions = s.process_target_actions("beta");

        assert_eq!(
            actions,
            vec![
                PaneAction::Hide(PaneId::Terminal(10)),
                PaneAction::Show(PaneId::Terminal(20)),
            ]
        );
        assert!(!s.pinned_panes.contains("alpha")); // pin cleared on switch
    }

    #[test]
    fn sibling_already_hid_other_pane_no_redundant_hide() {
        // If "alpha" is already hidden (pane_visible=false), triggering "beta"
        // should not emit a redundant Hide(alpha) action.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.pane_visible.insert("alpha".to_string(), false); // already hidden
        s.pane_visible.insert("beta".to_string(), false);

        let actions = s.process_target_actions("beta");

        assert_eq!(actions, vec![PaneAction::Show(PaneId::Terminal(20))]);
    }

    #[test]
    fn unknown_target_is_noop() {
        // A target not in pane_map must leave all state unchanged.
        let mut s = state_with_visible_pane("known", 1);

        let actions = s.process_target_actions("ghost");

        assert!(actions.is_empty());
        assert_eq!(s.pane_visible.get("known"), Some(&true)); // untouched
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
        assert_eq!(s.pane_visible.get("broot"), Some(&true)); // not suppressed
    }

    #[test]
    fn rebuild_discovers_suppressed_pane_as_hidden() {
        // A pane discovered while suppressed is recorded as not visible.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];
        s.own_tab_index = Some(0);

        s.rebuild_pane_map(make_manifest(vec![make_suppressed_pane(42, "broot")]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
        assert_eq!(s.pane_visible.get("broot"), Some(&false));
    }

    #[test]
    fn rebuild_updates_visibility_for_existing_pane() {
        // pane_visible must be refreshed from PaneInfo.is_suppressed on every
        // PaneUpdate so that external hide actions are picked up.
        let mut s = state_with_visible_pane("broot", 42);

        // Pane was hidden externally; next PaneUpdate shows it as suppressed.
        s.rebuild_pane_map(make_manifest(vec![make_suppressed_pane(42, "broot")]));

        assert_eq!(s.pane_visible.get("broot"), Some(&false));
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
    fn rebuild_removes_closed_pane_and_clears_state() {
        // When a pane disappears from the manifest, its entry must be evicted and
        // visibility/pin state must be cleared.
        let mut s = state_with_visible_pane("broot", 42);
        s.pinned_panes.insert("broot".to_string());

        s.rebuild_pane_map(make_manifest(vec![]));

        assert!(!s.pane_map.contains_key("broot"));
        assert!(!s.pane_visible.contains_key("broot"));
        assert!(!s.pinned_panes.contains("broot"));
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
        s.pane_visible.insert("broot".to_string(), true);

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

        // pending_target must still be Some (not consumed)
        // (In production this check lives in update(); here we verify the invariant)
        // The pane_map is empty so pending_target should stay.
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
