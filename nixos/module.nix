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
#
# Optionally, home-manager can be enabled for the clanker user
# (services.omega.homeManager) so that user-scoped configuration — git
# identity, ssh-agent, shell setup, etc. — can be written inline here and
# injected into home-manager, keeping all the omega-related configuration
# in one place.
# ---------------------------------------------------------------------------

{
  config,
  lib,
  pkgs,
  ...
}@args:

let
  # The home-manager flake input.  The flake output `nixosModules.omega`
  # calls this module with `args // { inherit home-manager; }`, so this is
  # set when the module is used through the flake and null when it is
  # imported directly (e.g. `import ./nixos/module.nix`).  Reading it from
  # the raw call args (not from `config`) keeps it usable in `imports` —
  # config-dependent imports would recurse.
  home-manager = args.home-manager or null;

  inherit (lib)
    mkIf
    mkEnableOption
    mkMerge
    mkOption
    mkDefault
    optional
    optionalAttrs
    types
    literalExpression
    ;

  cfg = config.services.omega;

  # Build omega from the flake source — use a single build that includes all
  # workspace binaries, one of which we wrap for the TUI socket path.
  omegaPkg = pkgs.rustPlatform.buildRustPackage {
    pname = "omega";
    version = "0.1.0";
    src = ../.;
    cargoLock.lockFile = .././Cargo.lock;
    cargoBuildFlags = [
      "-p"
      "omega-loop"
      "-p"
      "omega-sh"
      "-p"
      "omega-tui"
      "-p"
      "omega-git-host"
    ];
    nativeBuildInputs = with pkgs; [
      pkg-config
      makeWrapper
    ];
    buildInputs = with pkgs; [ openssl.dev ];
    doCheck = false;
    postInstall = ''
      wrapProgram $out/bin/omega-tui \
        --set-default OMEGA_LOOP_SOCKET_PATH /run/omega/omega-loop.sock
    '';
  };

  # Derived constants
  clankerUser = cfg.user;
  clankerGroup = cfg.group;
  clankerHome = "/persist/${clankerUser}";
  omegaDir = "${clankerHome}/omega";

  omegaShSocket = "/run/omega/omega-sh.sock";
  omegaLoopSocket = "/run/omega/omega-loop.sock";
  systemPromptPath = "/etc/omega/system-prompt.md";
  rebaseDefaultsPath = "/etc/omega/rebase-job.defaults.json";
  rolesPath = "/etc/omega/roles.json";

  # JSON payload written to `rolesPath` — the list of named role system
  # prompts (`services.omega.roles`). The daemon reads this at startup; only
  # the role *names* are exposed to clients so the TUI can wire up
  # `/<role>` slash commands.
  rolesJson = builtins.toJSON (
    map (
      r:
      ({
        name = r.name;
      })
      // (
        if r.extraSystemPrompt != "" then
          {
            system_prompt =
              r.systemPrompt + "\n\n" + r.extraSystemPrompt;
          }
        else
          {
            system_prompt = r.systemPrompt;
          }
      )
    ) cfg.roles
  );

  # Default system prompt used when the user doesn't set cfg.systemPrompt.
  defaultSystemPrompt = ''
    You are Omega, a general-purpose agent with full filesystem and shell access.

    You operate in a dedicated workspace and have full permissions to manage your
    workspace as you see fit. The user will give you tasks to accomplish with
    the tools at your disposal. Use them wisely and judiciously, and offer to
    send the user the work you've done (but not do it automatically unless the user
    says otherwise.)

    ## Communication

    Lead with the outcome rather than the steps you took to get there.
    Communicate complex concepts clearly, calibrating to the user's background
    knowledge. Prefer plain language over jargon.

    Avoid over-formatting responses. Use the minimum formatting appropriate.
    If you use lists, follow CommonMark standard (blank line before list,
    blank line between headers and content).

    Use visualisations (tables, timelines, trees) only when they make a
    relationship materially easier to understand than prose.

    ## Working

    You have access to a set of tools for reading, writing, and editing files,
    running shell commands, searching your workspace, and asking the user
    questions. Use them as needed — they are provided to you separately.

    You can create and clone git repositories.  If a repository you need
    is not already present on disk, clone it using `git clone <url>`.
    You can also initialise new repos with `git init`.

    You have nix installed and can run `nix build`, `nix develop`,
    `nix flake` commands, etc. You operate on NixOS so this should be your
    first line of action. Prefer creating nix devshell flakes over trying to
    permanently install programs into your $PATH.

    Work is performed under the "${clankerUser}" user.
    Your home directory is ${clankerHome}.
    You can create project directories under ${cfg.projectsDir}
    or clone repos there.
  '';

  # Default paragraph appended to the prompt of every session that enters a
  # project: keep the checkout in sync with upstream. Must match
  # omega-projects/src/rebase.rs `DEFAULT_UPDATE_INSTRUCTION` — keep in sync.
  defaultRebaseUpdatePrompt = ''
    Keep your checkout up to date with upstream. Before starting work in this project,
    update your checkout: run `git fetch origin` (if your worktree predates the latest
    upstream), then rebase your worktree branch onto the project's up-to-date main branch
    so the worktree is on top of the latest upstream changes. The project's main branch is
    kept in sync with upstream by a periodic job, so a simple `git rebase {branch}` is
    normally all that is needed. If rebasing surfaces conflicts, resolve them yourself
    before making changes, and make sure the worktree still builds and tests pass.
  '';

  # Default system prompt for each project's dedicated "upstream rebaser"
  # chat. Must match omega-projects/src/rebase.rs `DEFAULT_REBASER_PROMPT`
  # (modulo the {branch} placeholder) — keep in sync.
  defaultRebaserPrompt = ''
    You are the dedicated "upstream rebaser" chat for this project.

    Your job: keep the project's main branch in sync with upstream. The project's main
    branch may carry commits that exist only locally (functionality upstream won't or
    can't add) on top of the upstream history.

    When you are woken (by the periodic rebase job or by the user), do this:
    1. Run `git fetch origin` so the upstream refs are current.
    2. Rebase the main branch onto the latest upstream (`git rebase origin/{branch}` in
       your worktree). The worktree is your sole writer of the main branch.
    3. If the rebase stops on conflicts, resolve them yourself: keep the intent of
       upstream's changes AND preserve the local-only functionality. When in doubt, keep
       both sides' intent, prefer the upstream change where they genuinely conflict, and
       ask the user in the session if a choice is really ambiguous.
    4. Once the rebase is complete (continue it to the end), verify the worktree builds
       / tests pass and report what you did.

    The user can also ask you directly to rebase at any time; treat that the same way.
  '';

  # Parse a duration string ("6h", "30m", "45s", "3600", "1d") to seconds.
  parseInterval = interval:
    let
      m = builtins.match "([0-9]+)([smhd])?" interval;
    in
    if m == null then
      throw "services.omega.rebaseJob.interval: cannot parse '${interval}' (use e.g. '6h', '30m', '3600')"
    else
      let
        n = builtins.fromJSON (builtins.head m);
        unit = if builtins.length m > 1 && builtins.elemAt m 1 != "" then builtins.elemAt m 1 else "s";
        mult = {
          s = 1;
          m = 60;
          h = 3600;
          d = 86400;
        }.${unit};
      in
      n * mult;

  # Final system prompt: if the user set systemPrompt, use that as the base;
  # otherwise use the default.  extraSystemPrompt is always appended.
  systemPrompt =
    (if cfg.systemPrompt != null then cfg.systemPrompt else defaultSystemPrompt)
    + (if cfg.extraSystemPrompt != "" then "\n\n" + cfg.extraSystemPrompt else "");

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
      example = [
        "alice"
      ];
      description = ''
        Human users that should be added to the omega group so they can
        connect to omega-loop with the omega-tui TUI client.
      '';
    };

    extraGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "docker"
        "kvm"
      ];
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
          # Output token cap sent with every request. Leave unset to let the
          # upstream apply its own default (often ~8k even for 1M-context
          # models). Set to the model's real output limit to avoid
          # finish_reason="length" truncation.
          OPENAI_MAX_TOKENS=65536

        If null, env vars must be provided by other means
        (e.g. sops-nix, agenix, or environment: directives).
      '';
    };

    logLevel = mkOption {
      type = types.enum [
        "trace"
        "debug"
        "info"
        "warn"
        "error"
      ];
      default = "info";
      description = "RUST_LOG level for the omega services.";
    };

    systemPrompt = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "You are a helpful AI assistant.";
      description = ''
        Complete system prompt for the agent.  If null (the default), the
        module's built-in default prompt is used.  Set this to completely
        replace the default.

        To just append extra instructions while keeping the default, use
        `extraSystemPrompt` instead.
      '';
    };

    extraSystemPrompt = mkOption {
      type = types.str;
      default = "";
      example = ''
        You have access to Docker and can run containers.
      '';
      description = ''
        Extra text appended to the system prompt (whether the built-in
        default or a custom `systemPrompt`).  A blank line is added
        before the extra content automatically.
      '';
    };

    roles = mkOption {
      type = types.listOf (types.submodule {
        options = {
          name = mkOption {
            type = types.str;
            example = "reverseengineer";
            description = ''
              The role's name.  In the TUI this becomes a slash command:
              `/<name> <prompt>` starts a brand-new session using this
              role's system prompt, with `<prompt>` as its first input.
            '';
          };

          systemPrompt = mkOption {
            type = types.str;
            default = "";
            description = ''
              The role's system prompt — the alternative instructions the
              agent uses for sessions started with this role.  For example,
              a "reverseengineer" role could carry detailed guidance on how
              you want the agent to approach reverse-engineering work.
            '';
          };

          extraSystemPrompt = mkOption {
            type = types.str;
            default = "";
            description = ''
              Extra text appended to `systemPrompt` for this role (a blank
              line is added first).  Useful for appending role-specific
              instructions while keeping the main prompt elsewhere.
            '';
          };
        };
      });
      default = [ ];
      example = lib.literalExpression ''
        [
          {
            name = "reverseengineer";
            systemPrompt = "You are a meticulous reverse-engineering analyst. Follow my process: ...";
          }
        ]
      '';
      description = ''
        Named roles — alternative system prompts the agent can be started
        with.  Each role becomes a slash command in the omega-tui:
        `/<name> <prompt>` starts a brand-new session using that role's
        system prompt and `<prompt>` as its first input (useful for a
        specialised prompt plus a task, e.g. `/reverseengineer analyze this
        binary`).

        No custom roles are set up by default — with this empty, only the
        default system prompt (``systemPrompt``) exists.  List available
        roles in the TUI with `/roles`.
      '';
    };

    sessionDir = mkOption {
      type = types.path;
      default = "${omegaDir}/sessions";
      defaultText = literalExpression ''"/persist/clanker/omega/sessions"'';
      description = "Directory where omega-loop stores session data.";
    };

    projectsDir = mkOption {
      type = types.path;
      default = "${clankerHome}/projects";
      defaultText = literalExpression ''"/persist/clanker/projects"'';
      description = ''
        Directory where omega-loop keeps the project store (registered repos
        + per-session git worktrees).  Defaults to a directory inside the
        configured home folder.  Exported to the daemon as
        OMEGA_PROJECTS_DIR.
      '';
    };

    # -------------------------------------------------------------------
    # rebaseJob — upstream rebase cron + dedicated "upstream rebaser" chats
    # -------------------------------------------------------------------
    rebaseJob = mkOption {
      type = types.submodule {
        options = {
          enable = mkEnableOption ''
            the upstream rebase cron: keeps cron-jobbable projects' main
            branches in sync with upstream, fast-forwarding mechanically
            when possible and waking a dedicated "upstream rebaser" chat to
            rebase + resolve conflicts when local main has diverged
          '';

          interval = mkOption {
            type = types.str;
            default = "6h";
            example = "30m";
            description = ''
              Interval between scheduled runs, as a duration string
              ("30m", "6h", "1d") or a bare number of seconds ("3600").
            '';
          };

          projects = mkOption {
            type = types.listOf types.str;
            default = [ ];
            example = [ "omega" "pi-omega" ];
            description = ''
              Projects (by registered name) to keep rebased on upstream
              automatically.  The web UI can also enable/disable projects
              imperatively — see the "rebase" page — and the set here is the
              seed/default for that.
            '';
          };

          systemPrompt = mkOption {
            type = types.str;
            default = defaultRebaserPrompt;
            description = ''
              System prompt for each project's dedicated "upstream rebaser"
              chat: the agent that rebases the project's main branch onto
              upstream and auto-fixes merge conflicts (preserving local-only
              commits).  The default tells it to keep the intent of both
              sides.  Override to change the rebaser's behaviour/wording.
            '';
          };

          updatePrompt = mkOption {
            type = types.str;
            default = defaultRebaseUpdatePrompt;
            description = ''
              Paragraph appended to the system prompt of every session that
              enters a project ("keep your checkout up to date with
              upstream").  `{branch}` is replaced with the project's default
              branch name.
            '';
          };
        };
      };
      default = { };
      description = ''
        Upstream rebase cron for the project store.  On every `interval`,
        omega-loop fetches upstream for each cron-jobbable project and
        brings the project's main branch up to date: a mechanical
        fast-forward when upstream merely moved, or a woken "upstream
        rebaser" chat when the local main must be rebased onto upstream
        (conflicts are fixed by that agent).  Individual projects can also
        be toggled imperatively on the git-host "rebase" page; NixOS
        `projects` seeds that set, and a web-UI disable always wins.
      '';
    };

    # -------------------------------------------------------------------
    # omega-git-host — read-only git forge + web UI over the project store
    # -------------------------------------------------------------------
    gitHost = mkOption {
      type = types.submodule {
        options = {
          enable = mkEnableOption ''
            omega-git-host, the read-only git forge + web UI over the
            project store (git clone /{name}.git, repo pages, session
            transcripts)
          '';

          port = mkOption {
            type = types.port;
            default = 8080;
            description = "TCP port omega-git-host listens on.";
          };

          listenAddress = mkOption {
            type = types.str;
            default = "127.0.0.1";
            description = ''
              Address to bind.  Defaults to loopback on purpose: the forge
              serves full session transcripts (possibly sensitive) and is
              read-only — expose it deliberately (SSH tunnel, reverse
              proxy, tailscale) rather than binding it openly.
            '';
          };
        };
      };
      default = { };
      description = ''
        Read-only git hosting + web UI for everything omega works on
        (SELFGIT.md).  Serves every registered project as a smart-HTTP
        remote, session worktrees as branches, and session transcripts.
      '';
    };

    homeManager = mkOption {
      type = types.submodule {
        options = {
          enable = mkEnableOption "home-manager configuration for the clanker user";

          config = mkOption {
            type = types.attrs;
            default = { };
            example = literalExpression ''
              {
                programs.git = {
                  enable = true;
                  userName = "Clanker";
                  userEmail = "clanker@example.com";
                };
                services.ssh-agent.enable = true;
              }
            '';
            description = ''
              Home-manager configuration for the clanker user, written as an
              attribute set.  When `enable` is true, this is injected into
              home-manager's `home-manager.users.${clankerUser}` module, so
              user-scoped configuration (git identity, ssh-agent, shell
              setup, ...) can be kept right here together with the rest of
              the omega configuration.

              Any home-manager option can be set here — the value is merged
              with the module's built-in defaults (home directory, username,
              state version).  Home-manager's NixOS module is imported
              automatically, so no separate home-manager wiring is needed.
            '';
          };
        };
      };
      default = { };
      description = ''
        Optional home-manager integration for the clanker user.

        When enabled, home-manager's NixOS module is wired up for the
        clanker user and `config` is injected as its home-manager
        configuration.
      '';
    };
  };

  # -----------------------------------------------------------------------
  # Config
  # -----------------------------------------------------------------------
  #
  # Home-manager's NixOS module is imported unconditionally (when the
  # home-manager input is available) so the `home-manager.users.<user>`
  # option namespace exists; the actual per-user wiring below is gated on
  # `cfg.homeManager.enable`.  Imports cannot depend on `config` (it would
  # recurse), and even `mkIf false` definitions of undeclared options
  # error, so this is the only safe structure.
  imports = optional (home-manager != null) home-manager.nixosModules.home-manager;

  config = mkIf cfg.enable (
    {
      assertions = [
        {
          assertion = !cfg.homeManager.enable || home-manager != null;
          message = ''
            services.omega.homeManager.enable = true requires access to a
            home-manager input, but this module was imported without one.

            Import the module through the pi-omega flake output
            `nixosModules.omega` — it forwards its own home-manager input
            automatically.  If you import `./nixos/module.nix` directly,
            enable home-manager yourself (home-manager.nixosModules.home-manager)
            and configure the clanker user there instead.
          '';
        }
      ];

      # ----- users & groups -------------------------------------------------
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

      # ----- named roles (services.omega.roles) ----------------------------
      # Written to /etc/omega/roles.json and read by omega-loop at startup
      # via OMEGA_ROLES_PATH. Kept as a (possibly empty) array — with no
      # roles configured only the default system prompt exists.
      environment.etc."omega/roles.json".text = rolesJson;

      # ----- rebase cron defaults (services.omega.rebaseJob) ---------------
      # Written when the job is enabled; omega-loop (and the git-host rebase
      # page) read it via OMEGA_REBASE_JOB_CONFIG. The imperative state file
      # (rebase-job.json in the project store, edited from the web UI)
      # overlays these defaults.
      environment.etc."omega/rebase-job.defaults.json" = mkIf cfg.rebaseJob.enable {
        text = builtins.toJSON {
          interval_seconds = parseInterval cfg.rebaseJob.interval;
          projects = cfg.rebaseJob.projects;
          agent_prompt = cfg.rebaseJob.systemPrompt;
          update_instruction = cfg.rebaseJob.updatePrompt;
        };
      };

      # ----- tmpfiles: runtime directory with correct permissions ----------
      systemd.tmpfiles.rules = [
        "d /run/omega 0770 ${clankerUser} ${clankerGroup} -"
        "d ${omegaDir} 0770 ${clankerUser} ${clankerGroup} -"
      ];

      # ----- systemd services ----------------------------------------------

      # omega-sh — filesystem/shell tool daemon
      systemd.services.omega-sh = {
        description = "Omega-sh daemon (filesystem/shell tools)";
        after = [ "network.target" ];
        wantedBy = [ "multi-user.target" ];
        # Always include bashInteractive and coreutils in the path so the
        # shell tool (Command::new("bash")) and basic utilities are available,
        # regardless of what the user puts in cfg.packages.
        path =
          with pkgs;
          [
            bashInteractive
            coreutils
          ]
          ++ cfg.packages;

        serviceConfig = {
          User = clankerUser;
          Group = clankerGroup;

          Type = "simple";
          ExecStart = "${cfg.package}/bin/omega-sh";
          Restart = "on-failure";
          RestartSec = "5s";

          Environment = [
            "OMEGA_SOCKET_PATH=${omegaShSocket}"
            "RUST_LOG=${cfg.logLevel}"
          ];

          # 0007 umask → files/sockets created with 0660 (rw-rw----)
          UMask = "0007";

          # Security hardening
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "full";
          ProtectHome = false; # needs access for project work
          ReadWritePaths = [ clankerHome ];
          RuntimeDirectory = "omega";
          RuntimeDirectoryMode = "0770";
        };
      };

      # omega-loop — agent runtime daemon (depends on omega-sh)
      systemd.services.omega-loop = {
        description = "Omega-loop daemon (agent runtime)";
        after = [
          "network.target"
          "omega-sh.service"
        ];
        requires = [ "omega-sh.service" ];
        wantedBy = [ "multi-user.target" ];
        # Always include bashInteractive and coreutils in the path so that
        # the agent can execute shell commands via the omega-sh daemon.
        path =
          with pkgs;
          [
            bashInteractive
            coreutils
          ]
          ++ cfg.packages;

        serviceConfig = {
          User = clankerUser;
          Group = clankerGroup;

          Type = "simple";
          ExecStart = "${cfg.package}/bin/omega-loop";
          WorkingDirectory = omegaDir;
          Restart = "on-failure";
          RestartSec = "5s";

          # 0007 umask → sockets created with 0770 (srw-rw----)
          # Without this, the default umask (0022) gives 0755 which
          # prevents other omega group members from connecting.
          UMask = "0007";

          Environment = [
            "OMEGA_LOOP_SOCKET_PATH=${omegaLoopSocket}"
            "OMEGA_SOCKET_PATH=${omegaShSocket}"
            "OMEGA_SYSTEM_PROMPT_PATH=${systemPromptPath}"
            "OMEGA_ROLES_PATH=${rolesPath}"
            "OMEGA_PROJECTS_DIR=${cfg.projectsDir}"
            "OMEGA_SESSION_DIR=${cfg.sessionDir}"
            "RUST_LOG=${cfg.logLevel}"
          ]
          ++ optional cfg.rebaseJob.enable "OMEGA_REBASE_JOB_CONFIG=${rebaseDefaultsPath}";

          # Security hardening
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "full";
          ProtectHome = false;
          ReadWritePaths = [ clankerHome ];

          RuntimeDirectory = "omega";
          RuntimeDirectoryMode = "0770";

          StateDirectory = "clanker/omega";
          StateDirectoryMode = "0770";

        }
        // (
          if cfg.envFile != null then
            {
              EnvironmentFile = cfg.envFile;
            }
          else
            { }
        );

        preStart = ''
          mkdir -p '${cfg.sessionDir}'
          mkdir -p '${cfg.projectsDir}'
          ln -sfT '${cfg.sessionDir}' "${omegaDir}/sessions" 2>/dev/null || true
        '';
      };

    # omega-git-host — read-only git forge + web UI (Phase 5: NixOS service)
    systemd.services.omega-git-host = mkIf cfg.gitHost.enable {
      description = "Omega-git-host (read-only git forge + web UI)";
      after = [
        "network.target"
        "omega-loop.service"
      ];
      # `wants`, not `requires`: the forge is a read-only view over the store
      # dirs and must keep serving even if the daemon is down or restarts.
      wants = [ "omega-loop.service" ];
      wantedBy = [ "multi-user.target" ];
      # Unlike the NixOS systemd default PATH, make git (and the user's extra
      # packages) available: every page and the smart-HTTP backend shells out
      # to `git` — without this, log/refs/tree/blob/commit pages and `git
      # clone` all fail in production.
      path = with pkgs; [ git ] ++ cfg.packages;

      serviceConfig = {
        User = clankerUser;
        Group = clankerGroup;

        Type = "simple";
        ExecStart = "${cfg.package}/bin/omega-git-host";
        Restart = "on-failure";
        RestartSec = "5s";

        Environment = [
          "OMEGA_PROJECTS_DIR=${cfg.projectsDir}"
          "OMEGA_SESSION_DIR=${cfg.sessionDir}"
          "OMEGA_GIT_HOST_LISTEN=${cfg.gitHost.listenAddress}"
          "OMEGA_GIT_HOST_PORT=${toString cfg.gitHost.port}"
          "RUST_LOG=${cfg.logLevel}"
        ]
        ++ optional cfg.rebaseJob.enable "OMEGA_REBASE_JOB_CONFIG=${rebaseDefaultsPath}";

        # Security hardening — the forge serves read-only views of the store,
        # but the "rebase" page is the imperative control panel for the cron,
        # so exactly the state file + run-now marker are writable (ReadWritePaths
        # takes precedence over the ReadOnlyPaths below for these two paths).
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = false; # needs to read the project store + sessions
        ReadOnlyPaths = [ clankerHome ];
        ReadWritePaths =
          optional cfg.rebaseJob.enable "${cfg.projectsDir}/rebase-job.json"
          ++ optional cfg.rebaseJob.enable "${cfg.projectsDir}/rebase-now";

        RuntimeDirectory = "omega";
        RuntimeDirectoryMode = "0770";
      };
    };

      # ----- omega binaries on the system ---------------------------------
      environment.systemPackages = [
        cfg.package # omega-tui, omega-loop, omega-sh
      ];

      # ----- allow clanker to use nix build etc. ---------------------------
      nix.settings.trusted-users = [ clankerUser ];

    }
    // optionalAttrs (home-manager != null) {
      # ----- optional home-manager for the clanker user -------------------
      home-manager = mkIf cfg.homeManager.enable {
        # Use the system's pkgs so home-manager modules are built against
        # the same nixpkgs as the rest of the configuration.
        useGlobalPkgs = true;
        # Install home.packages into the user's own profile instead of
        # bloating the system closure.
        useUserPackages = true;

        users.${clankerUser} = mkMerge [
          {
            home = {
              username = clankerUser;
              homeDirectory = clankerHome;
              # Overridable; pick the release your nixpkgs matches.
              stateVersion = mkDefault "26.05";
            };
          }
          cfg.homeManager.config
        ];
      };
    }
  );
}
