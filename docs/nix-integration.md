# Nix / home-manager integration

This guide covers three things:

1. Adding the flake as an input and understanding its outputs
2. A minimal home-manager setup (just install the `.wasm`)
3. A full home-manager setup that generates the zellij layout and keybinds from a
   single Nix list

---

## Adding the flake input

```nix
# flake.nix (inputs block)
inputs = {
  nixpkgs.url     = "github:NixOS/nixpkgs/nixpkgs-unstable";
  home-manager    = {
    url = "github:nix-community/home-manager";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  zellij-pane-manager = {
    url = "github:manuel2258/zellij-summon";
    inputs.nixpkgs.follows = "nixpkgs";
  };
};
```

The `inputs.nixpkgs.follows` line makes the plugin use the same nixpkgs as the
rest of your flake, avoiding a second copy.

**Outputs provided per supported system:**

| Output | Contents |
|--------|----------|
| `packages.${system}.default` | The built `zellij-pane-manager.wasm` derivation |
| `packages.${system}.zellij-pane-manager` | Same derivation, named alias |
| `devShells.${system}.default` | Shell with Rust + `wasm32-wasip1` target |

Supported systems: `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, `aarch64-darwin`.

The `.wasm` file lives inside the derivation at:

```
${zellij-pane-manager.packages.${system}.default}/zellij-pane-manager.wasm
```

---

## Minimal home-manager setup

Pass the flake input to your home module via `extraSpecialArgs`, then reference
it to get a Nix store path for the `.wasm`.

### flake.nix

```nix
{
  inputs = {
    nixpkgs.url  = "github:NixOS/nixpkgs/nixpkgs-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    zellij-pane-manager = {
      url = "github:manuel2258/zellij-summon";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, home-manager, zellij-pane-manager, ... }: {
    homeConfigurations."alice" = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      extraSpecialArgs = { inherit zellij-pane-manager; };
      modules = [ ./home.nix ];
    };
  };
}
```

### home.nix

```nix
{ pkgs, zellij-pane-manager, ... }:

let
  pluginPkg  = zellij-pane-manager.packages.${pkgs.system}.default;
  pluginPath = "${pluginPkg}/zellij-pane-manager.wasm";
in {
  # The .wasm is now available as a store path in pluginPath.
  # Wire it into your existing hand-written zellij config:
  xdg.configFile."zellij/config.kdl".text = ''
    keybinds {
        shared_except "locked" {
            bind "Alt b" {
                MessagePlugin "file:${pluginPath}" {
                    payload "broot"
                }
            }
        }
    }
  '';
}
```

---

## Full home-manager setup — generate layout + keybinds from Nix

Define your panes once in a Nix list and let the module generate both the layout
KDL and keybind KDL. Adding a new pane then requires only one line.

### flake.nix

Same as the minimal setup above — only `home.nix` differs.

### home.nix

```nix
{ config, lib, pkgs, zellij-pane-manager, ... }:

let
  pluginPkg  = zellij-pane-manager.packages.${pkgs.system}.default;
  pluginPath = "${pluginPkg}/zellij-pane-manager.wasm";

  # ── Define your panes here ───────────────────────────────────────────────
  managedPanes = [
    { name = "broot";    key = "Alt b"; }
    { name = "claude";   key = "Alt c"; }
    { name = "terminal"; key = "Alt t"; }
  ];
  # ─────────────────────────────────────────────────────────────────────────

  # Plugin config block used in the layout's plugin pane only.
  # Keybinds no longer need to repeat this list.
  pluginConfigBlock = lib.concatStringsSep "\n" (
    lib.imap0 (i: p: ''
                    pane_${toString i}_name "${p.name}"
                    pane_${toString i}_key  "${p.key}"'')
      managedPanes
  );

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

  # Each keybind sends a MessagePlugin payload — just the pane name.
  # No pane list duplication needed.
  keybindBlock = lib.concatStringsSep "\n" (map (p: ''
        bind "${p.key}" {
            MessagePlugin "file:${pluginPath}" {
                payload "${p.name}"
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

Add one entry to `managedPanes` and a matching `floating_panes` block in
`layoutTab`; everything else regenerates automatically:

```nix
managedPanes = [
  { name = "broot";    key = "Alt b"; }
  { name = "claude";   key = "Alt c"; }
  { name = "terminal"; key = "Alt t"; }
  { name = "lazygit";  key = "Alt g"; }  # ← new
];
```

```kdl
# add inside floating_panes { ... } in layoutTab
pane command="lazygit" name="lazygit" start_suspended=true {
    x "5%" y "5%" width "90%" height "90%"
}
```

---

## NixOS with home-manager as a module

If you manage home-manager through NixOS rather than standalone, pass
`extraSpecialArgs` at the module level instead:

```nix
outputs = { nixpkgs, home-manager, zellij-pane-manager, ... }: {
  nixosConfigurations.mymachine = nixpkgs.lib.nixosSystem {
    system = "x86_64-linux";
    modules = [
      home-manager.nixosModules.home-manager
      {
        home-manager.extraSpecialArgs = { inherit zellij-pane-manager; };
        home-manager.users.alice      = import ./home.nix;
      }
    ];
  };
};
```

The `home.nix` module is identical to the standalone case above.

---

## Building without flakes

If you are not using flakes, build from a local checkout with `callPackage`.
You need a `rustPlatform` that includes the `wasm32-wasip1` target; with the
`oxalica/rust-overlay`:

```nix
let
  pkgs = import <nixpkgs> { overlays = [ (import rust-overlay) ]; };
  rust = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-wasip1" ];
  };
  paneManagerPkg = pkgs.callPackage ./nix/default.nix {
    rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
  };
in
# ${paneManagerPkg}/zellij-pane-manager.wasm
```

Without the overlay, `pkgs.callPackage ./nix/default.nix {}` will fall back to
the default nixpkgs `rustPlatform`, which may not include the WASM target and
will fail at build time.
