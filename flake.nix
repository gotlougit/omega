{
  description = "Omega — AI coding agent with persistent omega services";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, home-manager, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = forAllSystems (system: import nixpkgs { inherit system; });

      mkOmegaPkg =
        {
          pkgs,
          pname,
          cargoBuildFlags,
        }:
        pkgs.rustPlatform.buildRustPackage {
          inherit pname cargoBuildFlags;
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ];
          buildInputs = with pkgs; [ openssl.dev ];
          doCheck = false;
        };
    in
    {
      # --------------------------------------------------------------------------
      # Packages — each omega binary built from the workspace
      # --------------------------------------------------------------------------
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
        in
        {
          default = self.packages.${system}.omega-sh;

          omega-sh = mkOmegaPkg {
            inherit pkgs;
            pname = "omega-sh";
            cargoBuildFlags = [
              "-p"
              "omega-sh"
            ];
          };

          omega-loop = mkOmegaPkg rec {
            inherit pkgs;
            pname = "omega-loop";
            cargoBuildFlags = [
              "-p"
              "omega-loop"
            ];
            postInstall = ''
              wrapProgram $out/bin/omega-tui \
                --set-default OMEGA_LOOP_SOCKET_PATH /run/omega/omega-loop.sock
            '';
          };

          omega-tui = mkOmegaPkg {
            inherit pkgs;
            pname = "omega-tui";
            cargoBuildFlags = [
              "-p"
              "omega-tui"
            ];
          };
        }
      );

      # --------------------------------------------------------------------------
      # Dev shells
      # --------------------------------------------------------------------------
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
          mkShell = pkgs.mkShell.override {
            stdenv = if pkgs.stdenv.isLinux then pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv else pkgs.stdenv;
          };
        in
        {
          default = mkShell {
            name = "rustdev";
            shellHook = ''
              export CARGO_HOME="$(realpath ./.localcargo)"
              export _ZO_DATA_DIR="$(realpath ./.localzoxide)"
            '';
            buildInputs = [
              pkgs.pkg-config
              pkgs.openssl.dev
              pkgs.rustc
              pkgs.cargo
              pkgs.rustfmt
              pkgs.clippy
              pkgs.rust-analyzer
              pkgs.gdb
              pkgs.git
              pkgs.nix
              pkgs.ripgrep
            ];
          };
        }
      );

      # --------------------------------------------------------------------------
      # NixOS module
      # --------------------------------------------------------------------------
      #
      # The module is a function taking the standard module args so the
      # module system can import it as `imports = [ pi-omega.nixosModules.omega ]`.
      # The flake's home-manager input is closed over and forwarded to the
      # module, which uses it to optionally wire up home-manager for the
      # clanker user (services.omega.homeManager).
      nixosModules.omega =
        { config, lib, pkgs, ... }@args:
        import ./nixos/module.nix (args // { inherit home-manager; });
    };
}
