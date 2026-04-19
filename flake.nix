{
  description = "Headless Zellij plugin for keybind-driven floating pane management";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      rustPlatformFor = pkgs:
        let
          rust = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "wasm32-wasip1" ];
          };
        in pkgs.makeRustPlatform { cargo = rust; rustc = rust; };

    in {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rustPlatform = rustPlatformFor pkgs;
          pkg = pkgs.callPackage ./nix/default.nix { inherit rustPlatform; };
        in {
          default = pkg;
          zellij-pane-manager = pkg;
        }
      );

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rust = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "wasm32-wasip1" ];
          };
        in {
          default = pkgs.mkShell {
            buildInputs = [ rust ];
          };
        }
      );
    };
}
