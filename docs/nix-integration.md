# Nix / home-manager integration

This guide shows how to manage `zellij-pane-manager` with Nix, including
generating the layout KDL and keybind KDL from a single Nix list so the
pane definitions stay in one place.

---

## Building the .wasm

```nix
# In your flake or configuration.nix
let
  paneManagerPkg = pkgs.callPackage ./nix/default.nix {};
in
# paneManagerPkg is a derivation; the .wasm lives at:
# ${paneManagerPkg}/zellij-pane-manager.wasm
```

If you are using the `oxalica/rust-overlay` or `fenix` overlays (recommended
for cross-compilation to wasm32-wasip1):

```nix
{ pkgs, rust-overlay, ... }:
let
  rustPkgs = pkgs.extend rust-overlay.overlays.default;
  rustWithWasm = rustPkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-wasip1" ];
  };
  paneManagerPkg = (pkgs.callPackage ./nix/default.nix {}).override {
    rustPlatform = pkgs.makeRustPlatform {
      cargo = rustWithWasm;
      rustc = rustWithWasm;
    };
  };
in { ... }
```

---

## Generating layout + keybinds from a Nix list

Define your panes once, generate everything from them:

```nix
{ config, lib, pkgs, ... }:

let
  paneManagerPkg = pkgs.callPackage ./nix/default.nix {};
  pluginPath = "${paneManagerPkg}/zellij-pane-manager.wasm";

  managedPanes = [
    { name = "broot";    key = "Alt b"; }
    { name = "claude";   key = "Alt c"; }
    { name = "terminal"; key = "Alt t"; }
  ];

  # Flat plugin config block shared by layout and every keybind
  pluginConfigBlock = lib.concatStringsSep "\n" (
    lib.imap0 (i: p: ''
                pane_${toString i}_name "${p.name}"
                pane_${toString i}_key "${p.key}"'')
      managedPanes
  );

  # Full layout tab (paste into your layout file or use as xdg.configFile)
  layoutTab = ''
    tab name="dev" hide_floating_panes=true {
        pane command="hx" name="editor" {
            args "."
        }

        floating_panes {
            pane command="broot" name="broot" {
                x "60%" y "0%" width "40%" height "60%"
            }
            pane command="claude" name="claude" start_suspended=true {
                x "15%" y "5%" width "70%" height "90%"
            }
            pane name="terminal" {
                x "0%" y "60%" width "100%" height "40%"
            }
        }

        pane size=1 borderless=true {
            plugin location="file:${pluginPath}" {
    ${pluginConfigBlock}
            }
        }
    }
  '';

  # Keybind block for config.kdl
  keybindBlock = lib.concatStringsSep "\n" (map (p: ''
        bind "${p.key}" {
            LaunchOrFocusPlugin "file:${pluginPath}" {
                floating false
    ${pluginConfigBlock}
                target "${p.name}"
            }
        }'') managedPanes);

in {
  xdg.configFile = {
    "zellij/layouts/default.kdl".text = ''
      layout {
      ${layoutTab}
      }
    '';

    "zellij/config.kdl".text = ''
      keybinds {
          shared_except "locked" {
      ${keybindBlock}
          }
      }
    '';
  };
}
```

---

## Adding a new pane

Add one line to `managedPanes`:

```nix
managedPanes = [
  { name = "broot";    key = "Alt b"; }
  { name = "claude";   key = "Alt c"; }
  { name = "terminal"; key = "Alt t"; }
  { name = "lazygit";  key = "Alt g"; }  # ← new
];
```

Then add the corresponding floating pane definition to `layoutTab` and rebuild.
The plugin config and all keybind blocks are regenerated automatically.

---

## Using with flakes

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    zellij-pane-manager = {
      url = "github:manuel2258/zellij-summon";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, zellij-pane-manager, ... }: {
    homeConfigurations.myuser = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      modules = [
        ({ pkgs, ... }: {
          home.packages = [ zellij-pane-manager.packages.${pkgs.system}.default ];
        })
      ];
    };
  };
}
```

The repository ships a `flake.nix` — pin it directly via the URL above and the
`packages.${system}.default` output will contain the built `.wasm` artifact.
