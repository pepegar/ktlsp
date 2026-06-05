{
  description = "ktlsp — a simple, fast Kotlin language server (Rust + tree-sitter, no JVM)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      # The systems we build for. Add/remove as needed.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      # Helper: produce an attrset keyed by system, given a function of `pkgs`.
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Single source of truth for name/version/description/license.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

      # The package builder, parameterised over a `pkgs` so it works both as a flake output and from
      # the overlay. Deps are vendored reproducibly from Cargo.lock; the C tree-sitter grammars and
      # ring's crypto compile with the stdenv `cc` (no OpenSSL — ureq uses rustls).
      mkKtlsp =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          # Darwin links against libiconv; Linux needs nothing beyond stdenv.
          buildInputs = lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];

          meta = {
            description = cargoToml.package.description;
            homepage = "https://github.com/pepegar/ktlsp";
            license = lib.licenses.mit;
            mainProgram = "ktlsp";
            platforms = systems;
          };
        };
    in
    {
      # Overlay so other flakes can do:
      #   nixpkgs.overlays = [ ktlsp.overlays.default ];  # then pkgs.ktlsp is available
      overlays.default = _final: prev: { ktlsp = mkKtlsp prev; };

      # `nix build`, and for consumers: `ktlsp.packages.${system}.default`.
      packages = forAllSystems (pkgs: rec {
        ktlsp = mkKtlsp pkgs;
        default = ktlsp;
      });

      # `nix run github:pepegar/ktlsp` (starts the LSP on stdio).
      apps = forAllSystems (
        pkgs:
        let
          app = {
            type = "app";
            program = lib.getExe (mkKtlsp pkgs);
          };
        in
        {
          ktlsp = app;
          default = app;
        }
      );

      # `nix develop` — Rust toolchain + the package's own build inputs.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ (mkKtlsp pkgs) ];
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      });

      # `nix flake check` builds the package and runs its test suite.
      checks = forAllSystems (pkgs: {
        ktlsp = mkKtlsp pkgs;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
