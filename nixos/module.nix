# ---------------------------------------------------------------------------
# Picrust NixOS module
#
# Sets up the omega-sh and omega-loop daemons as systemd services running
# under a dedicated system user (default: "clanker").  The services
# communicate over Unix sockets in /run/picrust/ — any user in the
# "picrust" group can connect to omega-loop with the picrust-tui client.
#
# The clanker user has git, nix, and ripgrep available so the agent can
# clone repos, run nix flakes, etc., without ever needing sudo/wheel.
# ---------------------------------------------------------------------------

{ config, lib, pkgs, ... }:

let
  inherit (lib) mkIf mkEnableOption mkOption types literalExpression;

  cfg = config.services.picrust;

  # Build picrust from the flake source (../. is the flake root when this
  # module is consumed via inputs.picrust.nixosModules.picrust).
  picrustPkg = pkgs.rustPlatform.buildRustPackage {
    pname = "picrust";
    version = "0.1.0";
    src = ../.;
    cargoLock.lockFile = .././Cargo.lock;
    nativeBuildInputs = with pkgs; [ pkg-config ];
    buildInputs = with pkgs; [ openssl.dev ];
    doCheck = false;
  };

  # Derived constants
  clankerUser  = cfg.user;
  clankerGroup = cfg.group;
  clankerHome  = "/var/lib/${clankerUser}";
  picrustDir   = "${clankerHome}/picrust";

  omegaShSocket   = "/run/picrust/omega-sh.sock";
  omegaLoopSocket = "/run/picrust/omega-loop.sock";
  systemPromptPath = "/etc/picrust/system-prompt.md";

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
  options.services.picrust = {
    enable = mkEnableOption "picrust agent services (omega-sh + omega-loop)";

    package = mkOption {
      type = types.package;
      default = picrustPkg;
      defaultText = literalExpression ''
        pkgs.rustPlatform.buildRustPackage { src = ../.; }
      '';
      description = "The picrust package providing omega-sh, omega-loop, picrust-tui.";
    };

    user = mkOption {
      type = types.str;
      default = "clanker";
      description = "System user that runs the omega services.";
    };

    group = mkOption {
      type = types.str;
      default = "picrust";
      description = "Group owning the picrust runtime directory and sockets.";
    };

    humanUsers = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "gotlou" "alice" ];
      description = ''
        Human users that should be added to the picrust group so they can
        connect to omega-loop with picrust-tui.
      '';
    };

    extraGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "docker" "kvm" ];
      description = "Additional groups to add the clanker user to.";
    };

    extraPackages = mkOption {
      type = types.listOf types.package;
      default = with pkgs; [
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
      description = "Packages available in PATH for the omega services.";
    };

    envFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/var/lib/clanker/picrust/.env";
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
      default = "${picrustDir}/sessions";
      defaultText = literalExpression ''"/var/lib/clanker/picrust/sessions"'';
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
      createHome = true;
      description = "Picrust agent service user";
    };

    # ----- system prompt file --------------------------------------------
    environment.etc."picrust/system-prompt.md".text = systemPrompt;

    # ----- tmpfiles: runtime directory with correct permissions ----------
    systemd.tmpfiles.rules = [
      "d /run/picrust 0770 ${clankerUser} ${clankerGroup} -"
    ];

    # ----- systemd services ----------------------------------------------

    # omega-sh — filesystem/shell tool daemon
    systemd.services.omega-sh = {
      description = "Picrust omega-sh daemon (filesystem/shell tools)";
      after       = [ "network.target" ];
      wantedBy    = [ "multi-user.target" ];
      path        = cfg.extraPackages;

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
        ProtectSystem   = "strict";
        ProtectHome     = false;        # needs access for project work
        ReadWritePaths  = [ clankerHome ];
        RuntimeDirectory = "picrust";
        RuntimeDirectoryMode = "0770";
      };
    };

    # omega-loop — agent runtime daemon (depends on omega-sh)
    systemd.services.omega-loop = {
      description = "Picrust omega-loop daemon (agent runtime)";
      after       = [ "network.target" "omega-sh.service" ];
      requires    = [ "omega-sh.service" ];
      wantedBy    = [ "multi-user.target" ];
      path        = cfg.extraPackages;

      serviceConfig = {
        User  = clankerUser;
        Group = clankerGroup;

        Type  = "simple";
        ExecStart = "${cfg.package}/bin/omega-loop";
        WorkingDirectory = picrustDir;
        Restart    = "on-failure";
        RestartSec = "5s";

        Environment = [
          "OMEGA_LOOP_SOCKET_PATH=${omegaLoopSocket}"
          "OMEGA_SOCKET_PATH=${omegaShSocket}"
          "OMEGA_SYSTEM_PROMPT_PATH=${systemPromptPath}"
          "RUST_LOG=${cfg.logLevel}"
        ];

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp      = true;
        ProtectSystem   = "strict";
        ProtectHome     = false;
        ReadWritePaths  = [ clankerHome ];

        StateDirectory   = "clanker/picrust";
        StateDirectoryMode = "0770";

      } // (if cfg.envFile != null then {
        EnvironmentFile = cfg.envFile;
      } else { });

      preStart = ''
        # Create session directory and a symlink so omega-loop's default
        # relative path ./sessions resolves correctly.
        mkdir -p '${cfg.sessionDir}'
        ln -sfT '${cfg.sessionDir}' "${picrustDir}/sessions" 2>/dev/null || true
      '';
    };

    # ----- extra packages installed on the system ------------------------
    environment.systemPackages = with pkgs; [
      cfg.package  # provides picrust-tui, picrust CLI
      git
      nix
      ripgrep
    ];

    # ----- allow clanker to use nix build etc. ---------------------------
    nix.settings.trusted-users = [ clankerUser ];

  };
}
