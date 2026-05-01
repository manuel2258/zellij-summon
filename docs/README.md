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
Every floating pane must have a unique `name`. A single headless plugin pane manages all of them.

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

        // One headless plugin pane manages all floating panes.
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/zellij-pane-manager.wasm" {
                pane_0_name "broot"
                pane_1_name "claude"
                pane_2_name "terminal"
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
Each keybind uses `MessagePlugin` (broadcast — no URL) to deliver the target pane
name to the running plugin instance. Zellij forwards the message to all running
plugins; the plugin recognises the `"toggle"` name and acts on it.

```kdl
keybinds {
    shared_except "locked" {
        bind "Alt b" {
            MessagePlugin {
                name "toggle"
                payload "broot"
            }
        }
        bind "Alt c" {
            MessagePlugin {
                name "toggle"
                payload "claude"
            }
        }
        bind "Alt t" {
            MessagePlugin {
                name "toggle"
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
> zellij-tile 0.44). The "pinned" state above is internal to this plugin and
> only affects the toggle cycle length. Native pin support will be added when
> the upstream API provides it.

---

## Config reference

All plugin config keys are set in the layout plugin block.

| Key | Required | Description |
|-----|----------|-------------|
| `pane_N_name` | Yes | Must match the `name` field of a floating pane in your layout |

`pane_N_name` keys are 0-indexed. Parsing stops at the first missing index.

Keybind `MessagePlugin` blocks use:

| Key | Value |
|-----|-------|
| `name` | Always `"toggle"` |
| `payload` | The name of the pane to toggle (must be one of the configured `pane_N_name` values) |

---

## Debugging

The plugin writes a log to `/tmp/zellij-pane-manager.log` (append mode).
Tail it alongside Zellij to trace pane discovery, keybind messages, and state transitions:

```sh
tail -f /tmp/zellij-pane-manager.log
```

---

## Troubleshooting

**Pane not found / plugin seems to do nothing**

The plugin discovers panes by matching their title to the configured name at
startup. If the pane's terminal application overwrites the title before the
first `PaneUpdate` event arrives, the match fails. Workarounds:
- Use `start_suspended=true` for panes whose apps set an aggressive title.
- Ensure the layout `name` field matches the application's initial title exactly.
- Check the log — a `WARN` line will appear when discovery fails.

**Grant permissions prompt**

On first load, Zellij asks you to grant `ReadApplicationState` and
`ChangeApplicationState`. Accept both — the plugin cannot function without them.

**Wrong zellij-tile version**

Ensure the installed zellij-tile version matches your running Zellij version.
Update `Cargo.toml` if necessary and rebuild.
