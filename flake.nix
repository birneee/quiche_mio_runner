{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rustToolchain = pkgs.rust-bin.stable."1.92.0".default.override {
          extensions = [
            "rust-src"
            "clippy"
            "rustfmt"
            "rust-analyzer"
          ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "quiche_endpoint-0.1.0" = "sha256-ZyOeNc408flsQboJ27TWjFY/f1HYKBMlOQt+neqMy9I=";
            };
          };
          src = pkgs.lib.cleanSource ./.;
          nativeBuildInputs = with pkgs; [
            clang
            cmake
            git
          ];
          env = {
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };
        };
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            clang
            cmake
            rustToolchain
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          shellHook = ''
            ln -sfn ${rustToolchain} $PWD/.rust-toolchain
          '';
        };
      }
    );
}
