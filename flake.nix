{
  description = "Ely.by and Mojang skins for the Pumpkin Minecraft server (WASM, plugin ABI v0.2)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      # Per-system package set, toolchain and the Rust platform built from it.
      perSystem =
        f:
        forAllSystems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            rustPlatform = pkgs.makeRustPlatform {
              cargo = toolchain;
              rustc = toolchain;
            };
          in
          f { inherit pkgs toolchain rustPlatform; }
        );
      manifest = (nixpkgs.lib.importTOML ./Cargo.toml).package;
    in
    {
      packages = perSystem (
        { pkgs, rustPlatform, ... }:
        {
          # The plugin component: result/lib/skinrestorer.wasm
          default = rustPlatform.buildRustPackage {
            pname = manifest.name;
            inherit (manifest) version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # .cargo/config.toml selects wasm32-wasip2; the host-side unit tests run in checks.
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target wasm32-wasip2
              runHook postBuild
            '';
            doCheck = false;
            # ELF strip does not understand wasm; cargo already strips (profile.release).
            dontStrip = true;
            installPhase = ''
              runHook preInstall
              install -Dm644 target/wasm32-wasip2/release/pumpkin_skinrestorer.wasm $out/lib/skinrestorer.wasm
              runHook postInstall
            '';
          };
        }
      );

      checks = perSystem (
        { pkgs, rustPlatform, ... }:
        let
          host = pkgs.stdenv.hostPlatform.rust.rustcTarget;
          check =
            name: script:
            rustPlatform.buildRustPackage {
              pname = "pumpkin-skinrestorer-${name}";
              version = "0";
              src = pkgs.lib.cleanSource ./.;
              cargoLock.lockFile = ./Cargo.lock;
              buildPhase = "true";
              checkPhase = script;
              installPhase = "touch $out";
            };
        in
        {
          # The pure modules are tested natively (the plugin glue is wasm-only).
          tests = check "tests" ''
            cargo test --offline --target ${host}
          '';
          lint = check "lint" ''
            cargo fmt --check
            cargo clippy --offline --release -- -D warnings
            cargo clippy --offline --target ${host} --tests -- -D warnings
          '';
        }
      );

      devShells = perSystem (
        { pkgs, toolchain, ... }:
        {
          default = pkgs.mkShell {
            packages = [
              toolchain
              pkgs.wasm-tools
              pkgs.just
            ];
          };
        }
      );

      formatter = perSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
