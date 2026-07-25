# ---------------------------------------------------------------------------
# Omega NixOS module
#
# Sets up the omega-sh and omega-loop daemons as systemd services running
# under a dedicated system user (default: "clanker").  The services
# communicate over Unix sockets in /run/omega/ — any user in the
# "omega" group can connect to omega-loop with the omega-tui TUI client.
#
# The clanker user has git, nix, and ripgrep available so the agent can
# clone repos, run nix flakes, etc., without ever needing sudo/wheel.
# ---------------------------------------------------------------------------

{ config, lib, pkgs, ... }:

let
  inherit (lib) mkIf mkEnableOption mkOption types literalExpression;

  cfg = config.services.omega;

  # Build picrust from the flake source (../. is the flake root when this
  # module is consumed via inputs.picrust.nixosModules.picrust).
  omegaPkg = pkgs.rustPlatform.buildRustPackage {
    pname = "picrust";
    version = "0.1.0";
    src = ../.;
    cargoLock.lockFile = .././Cargo.lock;
    nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
    buildInputs = with pkgs; [ openssl.dev ];
    doCheck = false;
    postInstall = ''
      wrapProgram $out/bin/omega-tui \
        --set-default OMEGA_LOOP_SOCKET_PATH /run/omega/omega-loop.sock
    '';
  };

  # Derived constants
  clankerUser  = cfg.user;
  clankerGroup = cfg.group;
  clankerHome  = "/var/lib/${clankerUser}";
  omegaDir     = "${clankerHome}/omega";

  omegaShSocket   = "/run/omega/omega-sh.sock";
  omegaLoopSocket = "/run/omega/omega-loop.sock";
  systemPromptPath = "/etc/omega/system-prompt.md";

  # Hardcoded system prompt injected into every new session.
  systemPrompt = ''
    You are a coding agent with full filesystem and shell access.

    You can create and clone git repositories.  If a repository you need
    is not already present on disk, clone it using `git clone <url>`.
    You can also initialise new repos with `git init`.

    You have nix installed and can run `nix build`, `nix develop`,
    `nix flake` commands, etc.

    Work is performed under the "${clankerUser}" user.
    Your home directory is ${clankerHome}.
    You can create project directories under ${clankerHome}/projects/
    or clone repos there.

    Available tools:
    - Read / Write / Edit  – filesystem operations
    - Bash                 – run arbitrary shell commands
    - Glob / Grep          – file searching
    - AskUserQuestion      – ask the user for input
  '';
in
{

  # -----------------------------------------------------------------------
  # Options
  # -----------------------------------------------------------------------
  options.services.omega = {
    enable = mkEnableOption "omega agent services (omega-sh + omega-loop)";

    package = mkOption {
      type = types.package;
      default = omegaPkg;
      defaultText = literalExpression ''
        pkgs.rustPlatform.buildRustPackage { src = ../.; }
      '';
      description = "Package providing omega-sh, omega-loop, and omega-tui.";
    };

    user = mkOption {
      type = types.str;
      default = "clanker";
      description = "System user that runs the omega services.";
    };

    group = mkOption {
      type = types.str;
      default = "omega";
      description = "Group owning the omega runtime directory and sockets.";
    };

    humanUsers = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "gotlou" "alice" ];
      description = ''
        Human users that should be added to the omega group so they can
        connect to omega-loop with the omega-tui TUI client.
      '';
    };

    extraGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "docker" "kvm" ];
      description = "Additional groups to add the clanker user to.";
    };

    packages = mkOption {
      type = types.listOf types.package;
      default = with pkgs; [
        bashInteractive
        coreutils
        git
        nix
        ripgrep
        openssh
        gnused
        gawk
        findutils
        curl
        wget
      ];
      description = "
        Packages to add to the omega service users' PATH and to the system
        environment.  The agent (omega-loop) uses these for Bash/Shell tool
        calls.  Add any tools you want the agent to have access to here.
      ";
    };

    envFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/var/lib/clanker/omega/.env";
      description = ''
        Path to an EnvironmentFile loaded by the omega-loop systemd service.
        Put any environment variables omega-loop needs here, for example:

          OPENAI_API_KEY=sk-...
          OPENAI_BASE_URL=https://api.openai.com/v1
          OPENAI_MODEL=gpt-4o

        If null, env vars must be provided by other means
        (e.g. sops-nix, agenix, or environment: directives).
      '';
    };

    logLevel = mkOption {
      type = types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = "RUST_LOG level for the omega services.";
    };

    sessionDir = mkOption {
      type = types.path;
      default = "${omegaDir}/sessions";
      defaultText = literalExpression ''"/var/lib/clanker/omega/sessions"'';
      description = "Directory where omega-loop stores session data.";
    };
  };

  # -----------------------------------------------------------------------
  # Config
  # -----------------------------------------------------------------------
  config = mkIf cfg.enable {

    # ----- users & groups ------------------------------------------------
    users.groups.${clankerGroup} = {
      members = cfg.humanUsers;
    };

    users.users.${clankerUser} = {
      isNormalUser = true;
      home = clankerHome;
      group = clankerGroup;
      extraGroups = cfg.extraGroups;
      packages = cfg.packages;
      createHome = true;
      description = "Omega agent service user";
    };

    # ----- system prompt file --------------------------------------------
    environment.etc."omega/system-prompt.md".text = systemPrompt;

    # ----- tmpfiles: runtime directory with correct permissions ----------
    systemd.tmpfiles.rules = [
      "d /run/omega 0770 ${clankerUser} ${clankerGroup} -"
    ];

    # ----- systemd services ----------------------------------------------

    # omega-sh — filesystem/shell tool daemon
    systemd.services.omega-sh = {
      description = "Omega-sh daemon (filesystem/shell tools)";
      after       = [ "network.target" ];
      wantedBy    = [ "multi-user.target" ];
      # Always include bashInteractive and coreutils in the path so the
      # shell tool (Command::new("bash")) and basic utilities are available,
      # regardless of what the user puts in cfg.packages.
      path        = with pkgs; [ bashInteractive coreutils ] ++ cfg.packages;

      serviceConfig = {
        User  = clankerUser;
        Group = clankerGroup;

        Type        = "simple";
        ExecStart   = "${cfg.package}/bin/omega-sh";
        Restart     = "on-failure";
        RestartSec  = "5s";

        Environment = [
          "OMEGA_SOCKET_PATH=${omegaShSocket}"
          "RUST_LOG=${cfg.logLevel}"
        ];

        # 0007 umask → files/sockets created with 0660 (rw-rw----)
        UMask = "0007";

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp      = true;
        ProtectSystem   = "full";
        ProtectHome     = false;        # needs access for project work
        ReadWritePaths  = [ clankerHome ];
        RuntimeDirectory = "omega";
        RuntimeDirectoryMode = "0770";
      };
    };

    # omega-loop — agent runtime daemon (depends on omega-sh)
    systemd.services.omega-loop = {
      description = "Omega-loop daemon (agent runtime)";
      after       = [ "network.target" "omega-sh.service" ];
      requires    = [ "omega-sh.service" ];
      wantedBy    = [ "multi-user.target" ];
      # Always include bashInteractive and coreutils in the path so that
      # the agent can execute shell commands via the omega-sh daemon.
      path        = with pkgs; [ bashInteractive coreutils ] ++ cfg.packages;

      serviceConfig = {
        User  = clankerUser;
        Group = clankerGroup;

        Type  = "simple";
        ExecStart = "${cfg.package}/bin/omega-loop";
        WorkingDirectory = omegaDir;
        Restart    = "on-failure";
        RestartSec = "5s";

        # 0007 umask → sockets created with 0770 (srw-rw----)
        # Without this, the default umask (0022) gives 0755 which
        # prevents other omega group members from connecting.
        UMask = "0007";

        Environment = [
          "OMEGA_LOOP_SOCKET_PATH=${omegaLoopSocket}"
          "OMEGA_SOCKET_PATH=${omegaShSocket}"
          "OMEGA_SYSTEM_PROMPT_PATH=${systemPromptPath}"
          "RUST_LOG=${cfg.logLevel}"
        ];

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp      = true;
        ProtectSystem   = "full";
        ProtectHome     = false;
        ReadWritePaths  = [ clankerHome ];

        RuntimeDirectory = "omega";
        RuntimeDirectoryMode = "0770";

        StateDirectory   = "clanker/omega";
        StateDirectoryMode = "0770";

      } // (if cfg.envFile != null then {
        EnvironmentFile = cfg.envFile;
      } else { });

      preStart = ''
        # Create session directory and a symlink so omega-loop's default
        # relative path ./sessions resolves correctly.
        mkdir -p '${cfg.sessionDir}'
        ln -sfT '${cfg.sessionDir}' "${omegaDir}/sessions" 2>/dev/null || true
      '';
    };

    # ----- omega binaries on the system ---------------------------------
    environment.systemPackages = [
      cfg.package  # omega-tui (TUI), picrust (CLI)
    ];

    # ----- allow clanker to use nix build etc. ---------------------------
    nix.settings.trusted-users = [ clankerUser ];

  };
}
