use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

register_plugin!(State);

const LOG_PATH: &str = "/tmp/zellij-pane-manager.log";

fn log(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        writeln!(f, "{msg}").ok();
    }
}

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
/// # N-instance design
///
/// Zellij 0.44 identifies plugin instances by (URL + user_configuration). To
/// let LaunchOrFocusPlugin find the running headless plugin instance, the
/// keybind config must exactly match the layout plugin config. This means one
/// layout plugin instance per managed pane, each with a unique `target` key
/// plus the shared pane list (pane_0_name, pane_1_name, …). The matching
/// keybind has the identical config. On each keypress LaunchOrFocusPlugin finds
/// the right instance and calls load() on it; load() reads `target` and acts.
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
    /// queued target from load() that arrived before pane_map was populated
    pending_target: Option<String>,
    /// whether first-time setup (permissions + subscription) has been done
    initialized: bool,
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
        log(&format!(
            "[load] called — config keys: {:?}",
            configuration.keys().collect::<Vec<_>>()
        ));
        log(&format!("[load] full config: {:?}", configuration));

        // Parse the ordered managed-pane list from config keys pane_0_name, pane_1_name, …
        let mut managed = Vec::new();
        let mut i = 0;
        while let Some(name) = configuration.get(&format!("pane_{i}_name")) {
            managed.push(name.clone());
            i += 1;
        }
        self.managed_panes = managed;
        log(&format!("[load] managed_panes: {:?}", self.managed_panes));

        // Prune stale entries left over from a previous config (rename, removal).
        let current = self.managed_panes.clone();
        self.pane_map.retain(|name, _| current.contains(name));
        self.pane_visible.retain(|name, _| current.contains(name));
        self.pinned_panes.retain(|name| current.contains(name));

        // Each layout plugin instance has a unique `target` key. LaunchOrFocusPlugin
        // matches (URL + full config) to find this exact instance, then calls load().
        if let Some(target) = configuration.get("target").cloned() {
            log(&format!(
                "[load] target={target:?} pane_map_empty={}",
                self.pane_map.is_empty()
            ));
            if self.pane_map.is_empty() {
                // pane_map not yet populated — queue for first PaneUpdate
                log(&format!("[load] queuing pending_target={target:?}"));
                self.pending_target = Some(target);
            } else {
                // Warm path: pane_map already populated from a prior PaneUpdate
                log(&format!(
                    "[load] warm path — pane_map={:?} pane_visible={:?}",
                    self.pane_map.keys().collect::<Vec<_>>(),
                    self.pane_visible
                ));
                self.process_target(&target);
            }
        } else {
            log("[load] no 'target' key in config — this instance will not act on any keybind");
        }

        log(&format!("[load] initialized={}", self.initialized));
        if !self.initialized {
            self.initialized = true;
            request_permission(&[
                PermissionType::ReadApplicationState,
                PermissionType::ChangeApplicationState,
            ]);
            // PermissionRequestResult triggers subscription to PaneUpdate
            subscribe(&[EventType::PermissionRequestResult]);
        }
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                log("[update] PermissionRequestResult::Granted — subscribing to PaneUpdate");
                subscribe(&[EventType::PaneUpdate]);
            }
            Event::PaneUpdate(manifest) => {
                self.rebuild_pane_map(manifest);
                log(&format!(
                    "[update] PaneUpdate — pane_map={:?} pane_visible={:?} pending={:?}",
                    self.pane_map.keys().collect::<Vec<_>>(),
                    self.pane_visible,
                    self.pending_target
                ));
                if let Some(target) = self.pending_target.take() {
                    log(&format!("[update] draining pending_target={target:?}"));
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
    /// Refresh pane_map and pane_visible from a fresh PaneManifest.
    ///
    /// Lifecycle:
    /// 1. Remove entries for panes no longer present in any tab (externally closed).
    ///    Pinned state is cleared for evicted panes.
    /// 2. Discover panes not yet mapped by matching their current title to a
    ///    configured managed-pane name. After first discovery, panes are tracked
    ///    by PaneId so title changes don't lose the mapping.
    /// 3. Update pane_visible for all mapped panes from PaneInfo.is_suppressed.
    ///    This keeps cross-instance visibility changes (from sibling plugin
    ///    instances) reflected in this instance's state.
    fn rebuild_pane_map(&mut self, manifest: PaneManifest) {
        let all_panes: Vec<PaneInfo> = manifest.panes.into_values().flatten().collect();

        // Remove entries for panes that no longer appear in the manifest
        let live_ids: HashSet<u32> = all_panes.iter().map(|p| p.id).collect();
        self.pane_map.retain(|_, pid| {
            let id = match *pid {
                PaneId::Terminal(id) | PaneId::Plugin(id) => id,
            };
            live_ids.contains(&id)
        });
        // Sync visibility and pin state to match evictions
        self.pane_visible
            .retain(|name, _| self.pane_map.contains_key(name));
        self.pinned_panes
            .retain(|name| self.pane_map.contains_key(name));

        // Discover panes not yet in pane_map by matching their title
        let unmapped: Vec<String> = self
            .managed_panes
            .iter()
            .filter(|n| !self.pane_map.contains_key(*n))
            .cloned()
            .collect();

        for name in unmapped {
            if let Some(pane) = all_panes.iter().find(|p| p.title == name) {
                self.pane_map.insert(name.clone(), make_pane_id(pane));
                self.pane_visible.insert(name, !pane.is_suppressed);
            }
        }

        // Update visibility for already-mapped panes so cross-instance hide/show
        // actions (issued by sibling plugin instances) are reflected here.
        for (name, pid) in &self.pane_map {
            let pane_id = match *pid {
                PaneId::Terminal(id) | PaneId::Plugin(id) => id,
            };
            if let Some(pane) = all_panes.iter().find(|p| p.id == pane_id) {
                self.pane_visible.insert(name.clone(), !pane.is_suppressed);
            }
        }
    }

    /// Pure state-machine core: computes which pane API calls are needed for a
    /// keybind trigger and mutates `self` accordingly. Returns the actions in
    /// dispatch order; the caller is responsible for executing them.
    ///
    /// Visibility is read from pane_visible (authoritative Zellij state from
    /// PaneUpdate) rather than tracked internally, so stale state from sibling
    /// plugin instances hiding our panes is handled correctly.
    ///
    /// State transitions:
    ///   HIDDEN              → (trigger) → SHOWN unpinned
    ///   SHOWN unpinned      → (trigger) → SHOWN pinned
    ///   SHOWN pinned        → (trigger) → HIDDEN
    ///   SHOWN (any, other)  → (trigger) → HIDDEN; target → SHOWN unpinned
    fn process_target_actions(&mut self, target: &str) -> Vec<PaneAction> {
        let mut actions = Vec::new();

        if !self.pane_map.contains_key(target) {
            return actions;
        }

        let target_is_visible = self.pane_visible.get(target).copied().unwrap_or(false);

        if target_is_visible {
            // Target already visible — advance the pin cycle
            let is_pinned = self.pinned_panes.contains(target);
            let pid = *self.pane_map.get(target).unwrap();
            if is_pinned {
                // SHOWN pinned → HIDDEN
                actions.push(PaneAction::Hide(pid));
                self.pinned_panes.remove(target);
            } else {
                // SHOWN unpinned → SHOWN pinned (internal state only)
                self.pinned_panes.insert(target.to_string());
            }
        } else {
            // Target hidden — hide every other currently visible managed pane,
            // then show target. Using pane_visible means sibling instances that
            // already hid their own panes won't produce redundant Hide actions.
            for name in self.managed_panes.clone() {
                if name != target
                    && self
                        .pane_visible
                        .get(name.as_str())
                        .copied()
                        .unwrap_or(false)
                {
                    if let Some(&pid) = self.pane_map.get(name.as_str()) {
                        actions.push(PaneAction::Hide(pid));
                        self.pinned_panes.remove(&name);
                    }
                }
            }
            let target_pid = *self.pane_map.get(target).unwrap();
            actions.push(PaneAction::Show(target_pid));
        }

        actions
    }

    /// Dispatch the actions returned by process_target_actions to the Zellij shim.
    #[cfg(not(test))]
    fn process_target(&mut self, target: &str) {
        log(&format!(
            "[process_target] target={target:?} pane_map={:?} pane_visible={:?} pinned={:?}",
            self.pane_map.keys().collect::<Vec<_>>(),
            self.pane_visible,
            self.pinned_panes
        ));
        let actions = self.process_target_actions(target);
        log(&format!("[process_target] actions={actions:?}"));
        for action in actions {
            match action {
                PaneAction::Hide(pid) => {
                    log(&format!("[process_target] hide_pane_with_id({pid:?})"));
                    hide_pane_with_id(pid);
                }
                PaneAction::Show(pid) => {
                    log(&format!("[process_target] show_pane_with_id({pid:?})"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Build a PaneInfo with only the semantically relevant fields set.
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

    /// A State with one known hidden pane (not yet visible).
    fn state_with_pane(name: &str, id: u32) -> State {
        let mut s = State::default();
        s.managed_panes = vec![name.to_string()];
        s.pane_map.insert(name.to_string(), PaneId::Terminal(id));
        s.pane_visible.insert(name.to_string(), false);
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
    // Breaking mutation: remove `actions.push(PaneAction::Show(target_pid))` →
    // actions vec is empty, first assertion fails.

    #[test]
    fn second_trigger_pins_pane() {
        // SHOWN unpinned → SHOWN pinned (no shim call — purely internal state)
        let mut s = state_with_visible_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert!(actions.is_empty());
        assert!(s.pinned_panes.contains("term"));
        assert_eq!(s.pane_visible.get("term"), Some(&true)); // still visible
    }
    // Breaking mutation: remove `self.pinned_panes.insert(target.to_string())` →
    // pinned_panes stays empty, second assertion fails.

    #[test]
    fn third_trigger_hides_pinned_pane() {
        // SHOWN pinned → HIDDEN
        let mut s = state_with_visible_pane("term", 1);
        s.pinned_panes.insert("term".to_string());

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::Hide(PaneId::Terminal(1))]);
        assert!(!s.pinned_panes.contains("term"));
    }
    // Breaking mutation: keep `self.pinned_panes` populated after hide →
    // pinned_panes still contains "term", last assertion fails.

    #[test]
    fn switch_panes_hides_current_shows_new() {
        // Triggering a different pane: hide old (unpinned), show new.
        // Order of actions is significant — hide before show.
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
    // Breaking mutation: remove `actions.push(PaneAction::Hide(pid))` in the
    // "hide other visible" loop → actions is [Show(20)] only, equality fails.

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
    // Breaking mutation: remove `self.pinned_panes.remove(&name)` in the loop →
    // "alpha" remains in pinned_panes after switching, last assertion fails.

    #[test]
    fn sibling_already_hid_other_pane_no_redundant_hide() {
        // If a sibling instance already hid "alpha" (pane_visible["alpha"] = false),
        // triggering "beta" should not emit a redundant Hide(alpha) action.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.pane_visible.insert("alpha".to_string(), false); // already hidden
        s.pane_visible.insert("beta".to_string(), false);

        let actions = s.process_target_actions("beta");

        assert_eq!(actions, vec![PaneAction::Show(PaneId::Terminal(20))]);
    }
    // Breaking mutation: remove the `pane_visible` guard → Hide(10) is emitted
    // even though alpha is already hidden, equality fails.

    #[test]
    fn unknown_target_is_noop() {
        // A target not in pane_map must leave all state unchanged.
        let mut s = state_with_visible_pane("known", 1);

        let actions = s.process_target_actions("ghost");

        assert!(actions.is_empty());
        assert_eq!(s.pane_visible.get("known"), Some(&true)); // untouched
    }
    // Breaking mutation: remove the early-return guard
    // `if !self.pane_map.contains_key(target) { return actions; }` →
    // execution reaches `self.pane_map.get(target).unwrap()` and panics.

    // ── rebuild_pane_map tests ───────────────────────────────────────────────

    #[test]
    fn rebuild_discovers_pane_by_title() {
        // An unmapped managed pane is found by matching its title in the manifest.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];

        s.rebuild_pane_map(make_manifest(vec![make_pane(42, false, "broot")]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
        assert_eq!(s.pane_visible.get("broot"), Some(&true)); // not suppressed
    }
    // Breaking mutation: change `p.title == name` to `p.title == "nonexistent"` →
    // nothing is inserted into pane_map, assertion fails.

    #[test]
    fn rebuild_discovers_suppressed_pane_as_hidden() {
        // A pane discovered while suppressed is recorded as not visible.
        let mut s = State::default();
        s.managed_panes = vec!["broot".to_string()];

        s.rebuild_pane_map(make_manifest(vec![make_suppressed_pane(42, "broot")]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
        assert_eq!(s.pane_visible.get("broot"), Some(&false));
    }

    #[test]
    fn rebuild_updates_visibility_for_existing_pane() {
        // pane_visible must be refreshed from PaneInfo.is_suppressed on every
        // PaneUpdate so that cross-instance hide actions are picked up.
        let mut s = state_with_visible_pane("broot", 42);

        // Sibling instance hid broot; next PaneUpdate shows it as suppressed
        s.rebuild_pane_map(make_manifest(vec![make_suppressed_pane(42, "broot")]));

        assert_eq!(s.pane_visible.get("broot"), Some(&false));
    }
    // Breaking mutation: remove the "update visibility for already-mapped panes"
    // block → pane_visible stays true after the suppressed PaneUpdate, fails.

    #[test]
    fn rebuild_pane_map_is_stable_after_title_change() {
        // Once a pane is in pane_map, its entry is preserved even if the terminal
        // application overwrites the pane title — tracking is by PaneId, not title.
        let mut s = state_with_pane("broot", 42);

        // Same numeric id, but title now shows the current working directory
        s.rebuild_pane_map(make_manifest(vec![make_pane(
            42,
            false,
            "broot - /home/user",
        )]));

        assert_eq!(s.pane_map.get("broot"), Some(&PaneId::Terminal(42)));
    }
    // Breaking mutation: add `self.pane_map.clear()` at the start of rebuild_pane_map →
    // pane_map is empty after rebuild; discovery fails (title no longer matches "broot"),
    // so assertion fails.

    #[test]
    fn rebuild_removes_closed_pane_and_clears_state() {
        // When a pane disappears from the manifest (user closed it with Ctrl+q),
        // its entry must be evicted and visibility/pin state must be cleared.
        let mut s = state_with_visible_pane("broot", 42);
        s.pinned_panes.insert("broot".to_string());

        // Manifest arrives with id=42 absent — pane was externally closed
        s.rebuild_pane_map(make_manifest(vec![]));

        assert!(!s.pane_map.contains_key("broot"));
        assert!(!s.pane_visible.contains_key("broot"));
        assert!(!s.pinned_panes.contains("broot"));
    }
    // Breaking mutation: remove the `self.pane_map.retain(...)` stale-cleanup block →
    // pane_map still contains "broot" after rebuild, first assertion fails.
}
