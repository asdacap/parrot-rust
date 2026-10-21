{
  description = "Parrot Rust workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
  }: let
    systems = [
      "aarch64-darwin"
      "x86_64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
    pkgsFor = system:
      import nixpkgs {
        inherit system;
        overlays = [rust-overlay.overlays.default];
      };
    toolchainFor = pkgs:
      pkgs.rust-bin.stable."1.85.0".default.override {
        extensions = ["clippy" "rustfmt"];
        targets = ["aarch64-apple-darwin" "x86_64-unknown-linux-musl"];
      };
  in {
    packages = forAllSystems (system: let
      pkgs = pkgsFor system;
    in
      import ./nix/packages {
        inherit pkgs;
        rust = toolchainFor pkgs;
        root = self;
      });

    devShells = forAllSystems (system: let
      pkgs = pkgsFor system;
      rust = toolchainFor pkgs;
    in {
      default = pkgs.mkShell {
        packages = with pkgs;
          [
            alejandra
            clang
            coreutils
            git
            nodejs_22
            pkg-config
            protobuf
            rust
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [bubblewrap];
        shellHook = ''
          export CARGO_HOME="''${PARROT_CARGO_HOME:-$PWD/.tmp/cargo-home}"
        '';
      };
    });

    checks = forAllSystems (system: let
      pkgs = pkgsFor system;
    in
      {
        nix-format =
          pkgs.runCommand "parrot-nix-format" {
            nativeBuildInputs = [pkgs.alejandra];
            src = self;
          } ''
            alejandra --check "$src/flake.nix"
            touch "$out"
          '';

      }
      // import ./nix/checks {
        inherit pkgs;
        root = self;
      });

    formatter = forAllSystems (system: (pkgsFor system).alejandra);
  };
}
