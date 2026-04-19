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

        // Prune stale entries left over from a previous config (rename, removal).
        // Clone to avoid borrow conflict between managed_panes and the maps.
        let current = self.managed_panes.clone();
        self.pane_map.retain(|name, _| current.contains(name));
        self.pinned_panes.retain(|name| current.contains(name));
        if let Some(ref name) = self.shown_pane.clone() {
            if !current.contains(name) {
                self.shown_pane = None;
            }
        }

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
    /// Refresh pane_map from a fresh PaneManifest.
    ///
    /// Lifecycle:
    /// 1. Remove entries for panes no longer present in any tab (externally closed).
    ///    If the closed pane was shown/pinned, that state is cleared too.
    /// 2. Discover panes not yet mapped by matching their current title to a
    ///    configured managed-pane name — succeeds on the first PaneUpdate after
    ///    startup when the title still matches the layout `name` field.
    ///    After first discovery, panes are tracked by PaneId so title changes
    ///    (e.g. broot updating the terminal title) don't lose the mapping.
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
        // Clear shown/pinned state for any pane that was just evicted
        if let Some(ref name) = self.shown_pane.clone() {
            if !self.pane_map.contains_key(name) {
                self.shown_pane = None;
                self.pinned_panes.remove(name);
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
                self.pane_map.insert(name, make_pane_id(pane));
            }
        }
    }

    /// Pure state-machine core: computes which pane API calls are needed for a
    /// keybind trigger and mutates `self` accordingly. Returns the actions in
    /// dispatch order; the caller is responsible for executing them.
    ///
    /// State transitions:
    ///   HIDDEN            → (trigger) → SHOWN unpinned
    ///   SHOWN unpinned    → (trigger) → SHOWN pinned
    ///   SHOWN pinned      → (trigger) → HIDDEN
    ///   SHOWN (any state) → (other)   → HIDDEN; other pane → SHOWN unpinned
    fn process_target_actions(&mut self, target: &str) -> Vec<PaneAction> {
        let mut actions = Vec::new();

        if !self.pane_map.contains_key(target) {
            return actions;
        }

        // Triggering the same pane that's already shown
        if self.shown_pane.as_deref() == Some(target) {
            let is_pinned = self.pinned_panes.contains(target);
            let pid = *self.pane_map.get(target).unwrap();
            if is_pinned {
                // SHOWN pinned → HIDDEN
                actions.push(PaneAction::Hide(pid));
                self.pinned_panes.remove(target);
                self.shown_pane = None;
            } else {
                // SHOWN unpinned → SHOWN pinned (no Zellij pin API; tracked internally)
                self.pinned_panes.insert(target.to_string());
            }
            return actions;
        }

        // Triggering a different pane — hide the current one first
        if let Some(shown) = self.shown_pane.take() {
            if let Some(shown_pid) = self.pane_map.get(&shown).copied() {
                actions.push(PaneAction::Hide(shown_pid));
            }
            self.pinned_panes.remove(&shown);
        }

        // Show target pane (always starts unpinned); float it if it was embedded
        let target_pid = *self.pane_map.get(target).unwrap();
        actions.push(PaneAction::Show(target_pid));
        self.shown_pane = Some(target.to_string());

        actions
    }

    /// Dispatch the actions returned by process_target_actions to the Zellij shim.
    /// This is the only place shim functions are called for pane visibility.
    fn process_target(&mut self, target: &str) {
        for action in self.process_target_actions(target) {
            match action {
                PaneAction::Hide(pid) => hide_pane_with_id(pid),
                PaneAction::Show(pid) => show_pane_with_id(pid, true),
            }
        }
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
    /// PaneInfo derives Default (zellij-utils 0.42.2), so struct update syntax
    /// insulates tests from future field additions.
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

    /// A State that already knows about one pane by name→ID.
    /// Nothing is shown; nothing is pinned.
    fn state_with_pane(name: &str, id: u32) -> State {
        let mut s = State::default();
        s.managed_panes = vec![name.to_string()];
        s.pane_map.insert(name.to_string(), PaneId::Terminal(id));
        s
    }

    // ── process_target_actions tests ─────────────────────────────────────────

    #[test]
    fn first_trigger_shows_pane() {
        // HIDDEN → SHOWN unpinned
        let mut s = state_with_pane("term", 1);

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::Show(PaneId::Terminal(1))]);
        assert_eq!(s.shown_pane.as_deref(), Some("term"));
        assert!(!s.pinned_panes.contains("term"));
    }
    // Breaking mutation: remove `actions.push(PaneAction::Show(target_pid))` →
    // actions vec is empty, first assertion fails.

    #[test]
    fn second_trigger_pins_pane() {
        // SHOWN unpinned → SHOWN pinned (no shim call — purely internal state)
        let mut s = state_with_pane("term", 1);
        s.shown_pane = Some("term".to_string());

        let actions = s.process_target_actions("term");

        assert!(actions.is_empty());
        assert!(s.pinned_panes.contains("term"));
        assert_eq!(s.shown_pane.as_deref(), Some("term")); // still shown
    }
    // Breaking mutation: remove `self.pinned_panes.insert(target.to_string())` →
    // pinned_panes stays empty, second assertion fails.

    #[test]
    fn third_trigger_hides_pinned_pane() {
        // SHOWN pinned → HIDDEN
        let mut s = state_with_pane("term", 1);
        s.shown_pane = Some("term".to_string());
        s.pinned_panes.insert("term".to_string());

        let actions = s.process_target_actions("term");

        assert_eq!(actions, vec![PaneAction::Hide(PaneId::Terminal(1))]);
        assert!(s.shown_pane.is_none());
        assert!(!s.pinned_panes.contains("term"));
    }
    // Breaking mutation: change `self.shown_pane = None` to
    // `self.shown_pane = Some(target.to_string())` → shown_pane is not cleared,
    // second assertion fails.

    #[test]
    fn switch_panes_hides_current_shows_new() {
        // Triggering a different pane: hide old (unpinned), show new.
        // Order of actions is significant — hide before show.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.shown_pane = Some("alpha".to_string());

        let actions = s.process_target_actions("beta");

        assert_eq!(
            actions,
            vec![
                PaneAction::Hide(PaneId::Terminal(10)),
                PaneAction::Show(PaneId::Terminal(20)),
            ]
        );
        assert_eq!(s.shown_pane.as_deref(), Some("beta"));
    }
    // Breaking mutation: remove `actions.push(PaneAction::Hide(shown_pid))` →
    // actions is [Show(20)] only, equality assertion fails.

    #[test]
    fn switch_from_pinned_clears_pin_hides_old_shows_new() {
        // A pinned pane must be unpinned (internally) when a different pane is triggered.
        let mut s = State::default();
        s.managed_panes = vec!["alpha".to_string(), "beta".to_string()];
        s.pane_map.insert("alpha".to_string(), PaneId::Terminal(10));
        s.pane_map.insert("beta".to_string(), PaneId::Terminal(20));
        s.shown_pane = Some("alpha".to_string());
        s.pinned_panes.insert("alpha".to_string());

        let actions = s.process_target_actions("beta");

        assert_eq!(
            actions,
            vec![
                PaneAction::Hide(PaneId::Terminal(10)),
                PaneAction::Show(PaneId::Terminal(20)),
            ]
        );
        assert_eq!(s.shown_pane.as_deref(), Some("beta"));
        assert!(!s.pinned_panes.contains("alpha")); // pin cleared on switch
    }
    // Breaking mutation: remove `self.pinned_panes.remove(&shown)` in the switch
    // branch → "alpha" remains in pinned_panes after switching, last assertion fails.

    #[test]
    fn unknown_target_is_noop() {
        // A target not in pane_map must leave all state unchanged.
        let mut s = state_with_pane("known", 1);
        s.shown_pane = Some("known".to_string());

        let actions = s.process_target_actions("ghost");

        assert!(actions.is_empty());
        assert_eq!(s.shown_pane.as_deref(), Some("known")); // untouched
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
    }
    // Breaking mutation: change `p.title == name` to `p.title == "nonexistent"` →
    // nothing is inserted into pane_map, assertion fails.

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
    fn rebuild_removes_closed_pane_and_clears_shown_state() {
        // When a pane disappears from the manifest (user closed it with Ctrl+q),
        // its entry must be evicted and shown/pinned state must be cleared to
        // avoid the plugin getting stuck believing a dead pane is still visible.
        let mut s = state_with_pane("broot", 42);
        s.shown_pane = Some("broot".to_string());
        s.pinned_panes.insert("broot".to_string());

        // Manifest arrives with id=42 absent — pane was externally closed
        s.rebuild_pane_map(make_manifest(vec![]));

        assert!(!s.pane_map.contains_key("broot"));
        assert!(s.shown_pane.is_none());
        assert!(!s.pinned_panes.contains("broot"));
    }
    // Breaking mutation: remove the `self.pane_map.retain(...)` stale-cleanup block →
    // pane_map still contains "broot" after rebuild, first assertion fails.
}
