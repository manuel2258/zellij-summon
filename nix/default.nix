# Build the zellij-pane-manager.wasm plugin.
#
# Callers must supply a `rustPlatform` whose Rust toolchain includes the
# `wasm32-wasip1` target.  With oxalica/rust-overlay:
#
#   let
#     rust = pkgs.rust-bin.stable.latest.default.override {
#       targets = [ "wasm32-wasip1" ];
#     };
#     rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
#   in pkgs.callPackage ./nix/default.nix { inherit rustPlatform; }
#
# Without an overlay, `pkgs.rustPlatform` is used but the build will fail if
# the default nixpkgs Rust toolchain does not include `wasm32-wasip1`.
{ pkgs ? import <nixpkgs> {}, rustPlatform ? pkgs.rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "zellij-pane-manager";
  version = "0.1.0";

  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  buildPhase = ''
    cargo build --release --target wasm32-wasip1
  '';

  installPhase = ''
    mkdir -p $out
    cp target/wasm32-wasip1/release/zellij-pane-manager.wasm \
       $out/zellij-pane-manager.wasm
  '';

  # wasm output is not a host executable; skip the default check phase
  doCheck = false;

  meta = with pkgs.lib; {
    description = "Headless Zellij plugin for keybind-driven floating pane management";
    homepage = "https://github.com/manuel2258/zellij-summon";
    license = licenses.mit;
    maintainers = [];
  };
}
