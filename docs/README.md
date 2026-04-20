# zellij-pane-manager

A headless [Zellij](https://zellij.dev) plugin that gives you one-keybind access to any floating pane you define in your layout. Press a key to show a pane, press it again to lock it in place, press it a third time to dismiss it. Only one managed pane is visible at a time — switching to another one automatically hides the current one.

---

## What it does

You define your floating panes (broot, claude, lazygit, a plain shell, etc.) directly in your Zellij layout file. The plugin sits invisibly in the corner and manages their visibility. Each pane gets a keybind. Press the keybind and the pane floats up; press it again to "pin" it (so you have to press twice more to dismiss); press the last time to hide it. Switch between panes freely — the current one hides automatically.

---

## Installation

### Option A — download the pre-built binary

1. Download `zellij-pane-manager.wasm` from the [latest release](https://github.com/manuel2258/zellij-summon/releases/latest).
2. Place it in your Zellij plugins directory:
   ```sh
   mkdir -p ~/.config/zellij/plugins
   mv zellij-pane-manager.wasm ~/.config/zellij/plugins/
   ```
3. Add the layout and keybinds shown below.

### Option B — build from source

Requirements: Rust stable + `wasm32-wasip1` target.

```sh
rustup target add wasm32-wasip1
git clone https://github.com/manuel2258/zellij-summon
cd zellij-summon
cargo build --release --target wasm32-wasip1
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/zellij_pane_manager.wasm \
   ~/.config/zellij/plugins/zellij-pane-manager.wasm
```

### Option C — Nix

See [nix-integration.md](nix-integration.md) for a home-manager example that generates the layout and keybinds from a Nix list.

---

## Layout setup

Add this tab to your layout file (`~/.config/zellij/layouts/default.kdl` or similar).
The key constraint: **every floating pane must have a unique `name`** that matches the plugin config.

```kdl
layout {
    tab name="dev" hide_floating_panes=true {
        pane command="hx" name="editor" {
            args "."
        }

        floating_panes {
            // Preloaded — process starts immediately, hidden until triggered
            pane command="broot" name="broot" {
                x "60%" y "0%" width "40%" height "60%"
            }

            // Lazy — process only starts on first keybind press
            pane command="claude" name="claude" start_suspended=true {
                x "15%" y "5%" width "70%" height "90%"
            }

            // Plain shell
            pane name="terminal" {
                x "0%" y "60%" width "100%" height "40%"
            }
        }

        // Headless plugin — one invisible row
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/zellij-pane-manager.wasm" {
                pane_0_name "broot"
                pane_0_key "Alt b"
                pane_1_name "claude"
                pane_1_key "Alt c"
                pane_2_name "terminal"
                pane_2_key "Alt t"
            }
        }
    }
}
```

> **Tip — lazy vs preloaded:**
> - `start_suspended=true` → process starts on first trigger (saves memory)
> - No `start_suspended` → process starts with Zellij (faster first-show)
>
> After a lazy process exits (e.g. after finishing work), Zellij shows a native
> "press Enter to rerun" prompt. The plugin treats this the same as a live pane —
> the next trigger shows the prompt, and Enter restarts the process.

---

## Keybind setup

Add this block to your Zellij config (`~/.config/zellij/config.kdl`).
Each keybind sends a `MessagePlugin` payload to the always-running headless plugin — no
pane list duplication needed.

```kdl
keybinds {
    shared_except "locked" {
        bind "Alt b" {
            MessagePlugin "file:~/.config/zellij/plugins/zellij-pane-manager.wasm" {
                payload "broot"
            }
        }
        bind "Alt c" {
            MessagePlugin "file:~/.config/zellij/plugins/zellij-pane-manager.wasm" {
                payload "claude"
            }
        }
        bind "Alt t" {
            MessagePlugin "file:~/.config/zellij/plugins/zellij-pane-manager.wasm" {
                payload "terminal"
            }
        }
    }
}
```

---

## State machine

Each managed pane cycles through three states:

```
HIDDEN ──[keybind]──► SHOWN (unpinned)
                           │
                       [keybind]
                           │
                           ▼
                      SHOWN (pinned)
                           │
                       [keybind]
                           │
                           ▼
                         HIDDEN
```

- **HIDDEN → SHOWN unpinned:** pane floats up and receives focus.
- **SHOWN unpinned → SHOWN pinned:** a second press "locks" the pane. In this
  state a single additional press (rather than just one) is required to dismiss
  it, protecting against accidental closure.
- **SHOWN pinned → HIDDEN:** third press hides and unlocks the pane.

**Switching panes:** if pane A is shown (in any state) and you trigger pane B,
pane A is immediately hidden (and unpinned) and pane B shows unpinned.

> **Note on native Zellij pinning:** Zellij has a built-in floating-pane pin
> feature that keeps a pane visible while you interact with other panes. The
> plugin API does not yet expose a way to set this programmatically (as of
> zellij-tile 0.42). The "pinned" state above is internal to this plugin and
> only affects the toggle cycle length — it does not prevent Zellij from
> suppressing the pane if you click elsewhere. Native pin support will be added
> when the upstream API provides it.

---

## Config reference

All plugin config keys follow the pattern `pane_N_*` where N is 0-indexed.
These are set once in the layout plugin block; keybinds send the target via `MessagePlugin` payload.

| Key | Required | Description |
|-----|----------|-------------|
| `pane_N_name` | Yes | Must match the `name` field of the floating pane in your layout |
| `pane_N_key` | No | Informational only — documents the intended keybind; not used by the plugin |

Panes are ordered by N (0, 1, 2, …). Parsing stops at the first missing index.
The keybind `payload` is the pane name to toggle — it must match a configured `pane_N_name`.

---

## Troubleshooting

**Pane not found / plugin seems to do nothing**

The plugin discovers panes by matching their title to the configured name at
startup. If the pane's terminal application overwrites the title before the
first `PaneUpdate` event arrives, the match fails. Workarounds:
- Use `start_suspended=true` for panes whose apps set an aggressive title.
- Ensure the layout `name` field matches the application's initial title exactly.

**Grant permissions prompt**

On first load, Zellij asks you to grant `ReadApplicationState` and
`ChangeApplicationState`. Accept both — the plugin cannot function without them.

**Wrong zellij-tile version**

Ensure the installed zellij-tile version matches your running Zellij version.
Update `Cargo.toml` if necessary and rebuild.
