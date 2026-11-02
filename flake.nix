{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      rec {
        packages.herdr-sessionsearch = pkgs.rustPlatform.buildRustPackage {
          pname = "herdr-sessionsearch";
          version = "0.1.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # A plugin directory for `herdr plugin link`, with the binary where
          # the manifest expects the build output.
          postInstall = ''
            plugin=$out/share/herdr-sessionsearch/plugin
            mkdir -p $plugin/target/release
            sed '/^\[\[build\]\]/,/^$/d' herdr-plugin.toml > $plugin/herdr-plugin.toml
            ln -s $out/bin/herdr-sessionsearch $plugin/target/release/herdr-sessionsearch
          '';
        };
        packages.default = packages.herdr-sessionsearch;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
          ];
        };
      }
    );
}
