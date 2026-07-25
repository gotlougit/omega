{
  description = "Omega — AI coding agent with persistent omega services";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = forAllSystems (system: import nixpkgs { inherit system; });
    in
    {
      # --------------------------------------------------------------------------
      # Packages — the four omega binaries
      # --------------------------------------------------------------------------
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
          picrust = pkgs.rustPlatform.buildRustPackage {
            pname = "picrust";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
            buildInputs = with pkgs; [ openssl.dev ];
            doCheck = false;
            postInstall = ''
              wrapProgram $out/bin/omega-tui \
                --set-default OMEGA_LOOP_SOCKET_PATH /run/omega/omega-loop.sock
            '';
          };
        in
        {
          default = picrust;
          picrust = picrust;
          omega-sh = picrust;
          omega-loop = picrust;
          omega-tui = picrust;
        }
      );

      # --------------------------------------------------------------------------
      # Dev shells (retained from original config)
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
      # NixOS module — import this in your system configuration to set up
      # the omega services (omega-sh + omega-loop) as systemd units
      # running under the dedicated "clanker" user.
      #
      # Usage:
      #   # flake.nix
      #   inputs.picrust.url = "path:/home/gotlou/Code/picrust";
      #
      #   outputs = { self, nixpkgs, picrust, ... }: {
      #     nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      #       modules = [
      #         picrust.nixosModules.picrust
      #         {
      #           services.omega = {
      #             enable = true;
      #             humanUsers = [ "gotlou" ];
      #           };
      #         }
      #       ];
      #     };
      #   };
      # --------------------------------------------------------------------------
      nixosModules.picrust = import ./nixos/module.nix;
    };
}
