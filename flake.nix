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

      # The Kotlin compile-daemon sidecar: a JVM built with Gradle that drives the real compiler
      # (kotlin-build-tools-api) for the opt-in `kotlin-daemon` diagnostics backend. Gradle fetches
      # its deps from Maven, which a sandboxed nix build can't do — so dependencies are pinned in
      # `sidecar/deps.json` via nixpkgs' gradle mitm-cache. Regenerate that lock after changing the
      # sidecar's dependencies with:  nix run .#sidecar.mitmCache.updateScript
      mkSidecar =
        pkgs:
        pkgs.stdenv.mkDerivation (finalAttrs: {
          pname = "ktlsp-sidecar";
          version = cargoToml.package.version;

          src = ./sidecar;

          nativeBuildInputs = [
            pkgs.gradle
            pkgs.makeWrapper
          ];

          mitmCache = pkgs.gradle.fetchDeps {
            pkg = finalAttrs.finalPackage;
            data = ./sidecar/deps.json;
          };
          # mitm-cache binds a local proxy; the Darwin sandbox needs local networking allowed.
          __darwinAllowLocalNetworking = true;

          gradleBuildTask = "installDist";
          # Pure JVM build, no behavioural tests to run here.
          doCheck = false;

          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r build/install/ktlsp-sidecar $out/ktlsp-sidecar
            # The installDist launcher resolves `java` via JAVA_HOME; pin it to a JRE so the
            # sidecar runs without a JDK on the user's PATH.
            wrapProgram $out/ktlsp-sidecar/bin/ktlsp-sidecar \
              --set JAVA_HOME ${pkgs.jdk21.home}
            runHook postInstall
          '';

          meta = {
            description = "ktlsp Kotlin compile-daemon sidecar";
            license = lib.licenses.mit;
            platforms = systems;
          };
        });

      mkKtlsp =
        pkgs:
        let
          sidecar = mkSidecar pkgs;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Darwin links against libiconv; Linux needs nothing beyond stdenv.
          buildInputs = lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];

          # Point ktlsp at the packaged sidecar launcher (used only by the opt-in kotlin-daemon
          # backend; the default gradle backend never spawns it). --set-default so a user/test can
          # still override KTLSP_SIDECAR_BIN.
          postInstall = ''
            wrapProgram $out/bin/ktlsp \
              --set-default KTLSP_SIDECAR_BIN ${sidecar}/ktlsp-sidecar/bin/ktlsp-sidecar
          '';

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
        sidecar = mkSidecar pkgs;
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
