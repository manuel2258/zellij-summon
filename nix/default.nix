{ pkgs ? import <nixpkgs> {} }:

pkgs.rustPlatform.buildRustPackage {
  pname = "zellij-pane-manager";
  version = "0.1.0";

  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = with pkgs; [
    # rust-bin provides the wasm32-wasip1 target; adjust if using rustup overlay
    (rust-bin.stable.latest.default.override {
      targets = [ "wasm32-wasip1" ];
    })
  ];

  # Disable the default host-native build and switch to wasm target
  buildPhase = ''
    cargo build --release --target wasm32-wasip1
  '';

  installPhase = ''
    mkdir -p $out
    cp target/wasm32-wasip1/release/zellij_pane_manager.wasm \
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
