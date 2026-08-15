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
| **omega-git-host** | Read-only self-hosted git forge over the project store: smart-HTTP remotes + a sourcehut-style web UI (repo pages, worktrees-as-branches, session transcripts) |
| **omega-sh** | Unix-socket daemon for filesystem/shell tool execution |
| **omega-sh-client** | Client library for `omega-sh` + proxy tools |
| **cli** | Minimal TUI primitives (screen, style, terminal) |
| **omega-tui** | TUI client for `omega-loop` |
| **clankersh** | Interactive REPL for `omega-sh` |

## Binaries

- `omega-loop` — core agent daemon (LLM + tool orchestration)
- `omega-sh` — filesystem/shell tool daemon
- `omega-tui` — TUI client for `omega-loop`
- `omega-git-host` — read-only git forge over the project store (smart HTTP + web UI)
- `clankersh` — REPL / one-shot client for `omega-sh`

## omega-git-host

A read-only, self-hosted git forge over the project store (full plan in
`SELFGIT.md`). Every registered project is served as a fetchable smart-HTTP
remote; every session worktree is exposed as a branch; and the web UI — in
sourcehut's minimal, no-JS styling — lets you browse repos and read the chat
transcripts behind each worktree.

Run it directly:

```
OMEGA_PROJECTS_DIR=/persist/clanker/projects \
OMEGA_SESSION_DIR=/persist/clanker/omega/sessions \
OMEGA_GIT_HOST_LISTEN=127.0.0.1 \
OMEGA_GIT_HOST_PORT=8080 \
omega-git-host
```

| Variable | Default | Meaning |
|---|---|---|
| `OMEGA_PROJECTS_DIR` | `./projects` | Project store (registered repos + worktrees) |
| `OMEGA_SESSION_DIR` | `./sessions` | Session store (`metadata.json`, `history.jsonl`) |
| `OMEGA_GIT_HOST_LISTEN` | `127.0.0.1` | Bind address — loopback on purpose, see below |
| `OMEGA_GIT_HOST_PORT` | `8080` | TCP port |

Clone, browse, read chats:

```
git clone http://127.0.0.1:8080/pi-omega.git
git checkout omega/<session>   # every session worktree is served as a branch
# http://127.0.0.1:8080/ → project list → refs → session transcripts
```

The forge is read-only (no push) and by default binds to loopback because it
serves full session transcripts, which may contain sensitive tool output.
Expose it deliberately (SSH tunnel, reverse proxy, tailscale) if you want it
reachable.

On NixOS, enable it through the module instead of running it by hand:

```nix
services.omega.gitHost.enable = true;
services.omega.gitHost.port = 8080;                  # default
services.omega.gitHost.listenAddress = "127.0.0.1";  # default
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
}
```

### Home-manager for the clanker user

The module can also wire up [home-manager](https://github.com/nix-community/home-manager)
for the clanker user (disabled by default), so user-scoped configuration
such as git identity, ssh-agent, or shell setup can be written inline and
kept together with the rest of the omega configuration:

```nix
{
  services.omega.homeManager = {
    enable = true;
    config = {
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
