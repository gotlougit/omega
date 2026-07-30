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
| **omega-sh** | Unix-socket daemon for filesystem/shell tool execution |
| **omega-sh-client** | Client library for `omega-sh` + proxy tools |
| **cli** | Minimal TUI primitives (screen, style, terminal) |
| **omega-tui** | TUI client for `omega-loop` |
| **clankersh** | Interactive REPL for `omega-sh` |

## Binaries

- `omega-loop` — core agent daemon (LLM + tool orchestration)
- `omega-sh` — filesystem/shell tool daemon
- `omega-tui` — TUI client for `omega-loop`
- `clankersh` — REPL / one-shot client for `omega-sh`
