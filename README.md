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

One exception to "read-only": the `/rebase` page is the *imperative control
panel* for the upstream rebase cron (see below) — it writes the cron state
file into the project store.

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
  - strictly behind → **mechanical fast-forward**, no model involved;
  - up to date, or ahead only (commits upstream doesn't have) → nothing —
    local-only functionality is never touched;
  - **diverged** (local main carries commits upstream lacks *and* upstream
    moved) → the project's dedicated **upstream rebaser** chat is woken. It
    rebases main onto upstream in the project's `main` worktree and
    **resolves the merge conflicts itself**, preserving the local-only
    commits and finishing the rebase.
- The rebaser chats are ordinary persistent sessions
  (`rebaser-<project>`), so you can wake one yourself anytime from the TUI
  and read its transcripts in the web UI.
- Every session that enters a project is told, via its system prompt, to
  keep its checkout up to date with upstream first.

Configuration has two sources, merged at runtime:

1. **NixOS defaults** — `services.omega.rebaseJob` (interval, projects,
   the rebaser system prompt, and the per-session “update your checkout”
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
    You are the upstream rebaser for this project...
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
}
```

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
