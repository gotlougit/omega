# omega

An agent runtime and tool execution framework in Rust.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  omega-loop            (daemon)                      │
│  ┌──────────┐ ┌──────────┐ ┌───────────┐            │
│  │  agent   │ │ session  │ │ runtime   │            │
│  │  loop    │ │ manager  │ │ channels  │            │
│  └──────────┘ └──────────┘ └───────────┘            │
│  ┌──────────┐ ┌──────────┐                          │
│  │ helpers  │ │ internals│                          │
│  └──────────┘ └──────────┘                          │
└──────────────────────────────────────────────────────┘

┌──────────────┐  ┌──────────────┐  ┌─────────────────┐
│  omega-tools │  │  omega-core  │  │  omega-sh       │
│  (Tool trait │  │  (pure types │  │  (filesystem/   │
│   + builtins)│  │   + trait)   │  │   shell daemon) │
└──────────────┘  └──────────────┘  └─────────────────┘
```

## Crates

| Crate | Description |
|---|---|
| **omega-core** | Pure types + `ToolRuntime` trait — no logic, 4 deps (serde, serde_json, thiserror, async-trait) |
| **omega-tools** | `Tool` trait, `ToolRegistry`, built-in tool implementations (Bash, Read, Write, Edit, Glob, Grep, Transfer) |
| **omega-llm** | LLM provider abstraction (`LlmProvider` trait, types, OpenAI backend) |
| **omega-loop** | Agent daemon — `StandardAgent`, `AgentRuntime`, `AgentSession`, helpers, everything else |
| **omega-loop-client** | Client library for the `omega-loop` daemon protocol |
| **omega-projects** | Project store: register git repos (bare clone once) and create per-session git worktrees |
| **omega-git-host** | Self-hosted git forge + web control panel over the project store: smart-HTTP remotes (read-only as a remote), a sourcehut-style web UI for repos/worktrees/transcripts, project & session management, and live chat with the agent (stream, message, interrupt) |
| **omega-sh** | Unix-socket daemon for filesystem/shell tool execution |
| **omega-sh-client** | Client library for `omega-sh` + proxy tools |
| **cli** | Minimal TUI primitives (screen, style, terminal) |
| **omega-tui** | TUI client for `omega-loop` |
| **clankersh** | Interactive REPL for `omega-sh` |

## Binaries

- `omega-loop` — core agent daemon (LLM + tool orchestration)
- `omega-sh` — filesystem/shell tool daemon
- `omega-tui` — TUI client for `omega-loop`
- `omega-git-host` — git forge + web control panel over the project store (smart HTTP + web UI, project/session management, live chat)
- `clankersh` — REPL / one-shot client for `omega-sh`

## omega-git-host

A read-only, self-hosted git forge over the project store (full plan in
`SELFGIT.md`). Every registered project is served as a fetchable smart-HTTP
remote; every session worktree is exposed as a branch; and the web UI — in
sourcehut's minimal, no-JS styling — lets you browse repos and read the chat
transcripts behind each worktree.

The forge is read-only as a git remote (no push), and by default binds to
loopback because it serves full session transcripts, which may contain
sensitive tool output. Expose it deliberately (SSH tunnel, reverse proxy,
tailscale) if you want it reachable.

Beyond read-only hosting, the web UI is the **control panel** for the agent:

- **Projects** — create a project from an upstream git URL (optionally under
  a friendly name), and delete one. Your `main` is periodically rebased on
  upstream (see the rebase cron).
- **Sessions** — start a brand-new agent session on a project (the daemon
  creates a dedicated git worktree), and chat with it live: streamed output
  over SSE, a message box, and an **Interrupt** button — TUI capabilities in
  the browser.
- **Ship it** — once a session's work is committed in its worktree, merge
  that branch into `main` so anyone cloning the repo can fetch it.
- **Rename** — worktrees/sessions can be renamed (the model does this via
  its rename tool; the UI reflects it) and are always found again.

The web server talks to `omega-loop` over its Unix socket
(`OMEGA_LOOP_SOCKET_PATH`). Sessions live in a daemon-global registry, so
**a session keeps running even after you close the tab** — you can reopen the
chat page at any time and resume it (or let it work autonomously).

Run it directly (with the daemon):

```
OMEGA_PROJECTS_DIR=/persist/clanker/projects \
OMEGA_SESSION_DIR=/persist/clanker/omega/sessions \
OMEGA_LOOP_SOCKET_PATH=/run/omega/loop.sock \
OMEGA_GIT_HOST_LISTEN=127.0.0.1 OMEGA_GIT_HOST_PORT=8080 \
omega-git-host
```

| Variable | Default | Meaning |
|---|---|---|
| `OMEGA_PROJECTS_DIR` | `./projects` | Project store (registered repos + worktrees) |
| `OMEGA_SESSION_DIR` | `./sessions` | Session store (`metadata.json`, `history.jsonl`) |
| `OMEGA_LOOP_SOCKET_PATH` | `/tmp/omega-loop.sock` | Daemon socket for chat/control |
| `OMEGA_GIT_HOST_LISTEN` | `127.0.0.1` | Bind address — loopback on purpose, see below |
| `OMEGA_GIT_HOST_PORT` | `8080` | TCP port |

Clone, browse, read chats, and drive sessions:

```
git clone http://127.0.0.1:8080/pi-omega.git
git checkout omega/<session>   # every session worktree is served as a branch
# http://127.0.0.1:8080/ → project list → refs → session transcripts
# /<project>/sessions → “Start session” → live chat page
```

On NixOS, enable it through the module instead of running it by hand:

```nix
services.omega.gitHost.enable = true;
services.omega.gitHost.port = 8080;                  # default
services.omega.gitHost.listenAddress = "127.0.0.1";  # default
```

## Upstream rebase cron

Omega can keep a project's **main branch** in sync with upstream on a
schedule, and hand the judgment parts to a dedicated agent:

- Every interval, for each cron-jobbable project, the daemon fetches
  upstream and checks the local default branch against `origin/<default>`:
  - strictly behind → **mechanical fast-forward**;
  - up to date, or ahead only (commits upstream doesn't have) → nothing is
    touched — local-only functionality is never dropped;
  - **diverged** (local main carries commits upstream lacks *and* upstream
    moved) → the project's dedicated **recurring chat** rebases main onto
    upstream in the project's `main` worktree and **resolves the merge
    conflicts itself**, preserving the local-only commits and finishing the
    rebase.
- The recurring chat is woken on **every** run, including fast-forwards,
  clean rebases, and up-to-date branches, so it can verify the worktree
  still builds and tests pass. A run is triggered even while a previous chat
  turn is still active — there is no short-circuit for an active recurring
  chat.
- The recurring chats are ordinary persistent sessions
  (`recurring-<project>`), so you can wake one yourself anytime from the TUI
  and read its transcripts in the web UI.
- Every session that enters a project is told, via its system prompt, to
  keep its checkout up to date with upstream first.

Configuration has two sources, merged at runtime:

1. **NixOS defaults** — `services.omega.rebaseJob` (interval, projects,
   the recurring chat system prompt, and the per-session “update your checkout”
   instruction), written to `/etc/omega/rebase-job.defaults.json`.
2. **Imperative state** — the web UI's `/rebase` page toggles projects in
   and out of the cron set, changes the interval, and has a “run now”
   button; it persists to `rebase-job.json` in the project store. A
   web-UI disable always wins over the NixOS project list.

```nix
services.omega.rebaseJob = {
  enable = true;
  interval = "6h";                  # or "30m", "1d", "3600"
  projects = [ "pi-omega" ];        # seed; the web UI can add/remove more
  systemPrompt = ''                # default: rebase main on upstream + fix conflicts
    You are the recurring chat for this project...
  '';
};
```

## NixOS module

The flake provides a NixOS module (`nixosModules.omega`) that sets up
`omega-sh` and `omega-loop` as systemd services for the `clanker` user,
with sockets in `/run/omega/`. Enable it with:

```nix
{
  imports = [ pi-omega.nixosModules.omega ];

  services.omega.enable = true;
  # Optional: expose the TUI to human users via the "omega" group.
  services.omega.humanUsers = [ "alice" ];
  # Optional: extra tools for the agent (added to the clanker user's PATH).
  services.omega.packages = [ pkgs.ffmpeg ];
  # Optional: git identity for commits the agent makes (defaults shown).
  # services.omega.gitUserName = "Clanker";
  # services.omega.gitUserEmail = "clanker@example.com";
}
```

### Default git identity

The agent makes commits on your behalf (project work, upstream-rebase fixes,
...), so the clanker user needs a git identity.  It ships with a sensible
one out of the box:

- `services.omega.gitUserName` (default `"Clanker"`)
- `services.omega.gitUserEmail` (default `"clanker@example.com"`)

Both default to null-able strings — set either to `null` to disable it and
let git fall back to its own global/elsewhere identity instead.

How the identity lands in git's config depends on whether home-manager is
in use for the clanker user (see below):

- **Without home-manager** (the default): the identity is written to
  `${homeDir}/.gitconfig` at activation time, as the clanker user.
- **With `services.omega.homeManager.enable = true`**: it is injected into
  home-manager's `programs.git` (enabled automatically).  An explicit
  `homeManager.config.programs.git.userName`/`userEmail` (or
  `homeManager.config.programs.git.enable = false`) always wins over these
globals.

### Roles — specialised system prompts

"Roles" are named, NixOS-configured alternative system prompts. Each role
becomes a slash command in the `omega-tui`: `/<name> <prompt>` starts a
brand-new session using that role's system prompt, with `<prompt>` as its
first input. This is handy for giving the agent specialised instructions
and a task in one go (e.g. `/reverseengineer analyze this binary`).

No roles are set up by default — with this empty, only the default system
prompt (``services.omega.systemPrompt``) exists. List available roles in
the TUI with `/roles`.

```nix
{
  services.omega.roles = [
    {
      name = "reverseengineer";
      systemPrompt = ''
        You are a meticulous reverse-engineering analyst.

        When I give you a binary, work through it systematically:
        1. Identify the target architecture and toolchain.
        2. Map the disassembly to high-level functions.
        3. Reconstruct intent from cross-references and strings.
        4. Document your findings as you go.
      '';
    }
  ];
}
```

### Home-manager for the clanker user

The module can also wire up [home-manager](https://github.com/nix-community/home-manager)
for the clanker user (disabled by default), so user-scoped configuration
such as git identity, ssh-agent, or shell setup can be written inline and
kept together with the rest of the omega configuration.  Setting
`services.omega.gitUserName`/`gitUserEmail` automatically enables
home-manager's `programs.git` for the user; anything you set explicitly in
`config.programs.git` takes precedence over those defaults:

```nix
{
  services.omega.homeManager = {
    enable = true;
    config = {
      # gitUserName/gitUserEmail provide the defaults; these override them:
      programs.git = {
        enable = true;
        userName = "Clanker";
        userEmail = "clanker@example.com";
      };
      services.ssh-agent.enable = true;
    };
  };
}
```

When enabled, home-manager's NixOS module is imported automatically (the
flake's pinned `home-manager` input is used) and `config` is injected into
`home-manager.users.<user>`, merged with sensible defaults (home directory,
username, state version). `useGlobalPkgs` and `useUserPackages` are enabled.

If you import `./nixos/module.nix` directly instead of using the flake
output, `services.omega.homeManager` is unavailable (an assertion explains
why) — configure the clanker user with your own home-manager setup in that
case.
