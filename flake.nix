{
  description = "jev-scout - zero-hallucination open-source repo and crate scout powered by TypeSafe Jev";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # Single source of truth for the version: read it straight from Cargo.toml.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (pkgs: rec {
        jev-scout = pkgs.rustPlatform.buildRustPackage {
          pname = "jev-scout";
          version = cargoToml.package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # Bundled tests are fully offline (no token/network spend), so let them run.
          # ureq uses rustls, not OpenSSL — no pkg-config/openssl native inputs needed.
          meta = with pkgs.lib; {
            description = cargoToml.package.description;
            homepage = cargoToml.package.repository;
            license = licenses.mit;
            mainProgram = "jev-scout";
          };
        };
        default = jev-scout;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer ];
          # jev-scout resolves its API key from ~/.config/openrouter/key when
          # TYPESAFE_API_KEY / OPENROUTER_API_KEY are unset (see src/jev.rs).
          shellHook = ''
            echo "jev-scout dev shell — cargo $(cargo --version 2>/dev/null | cut -d' ' -f2)"
          '';
        };
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.jev-scout}/bin/jev-scout";
          meta.description = cargoToml.package.description;
        };
      });
    };
}
