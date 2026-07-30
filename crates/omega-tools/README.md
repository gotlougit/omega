# omega-tools

Tool system for the omega agent framework.

Provides the `Tool` trait and `ToolRegistry` (interfaces) plus all built-in
tool implementations. This crate can be versioned independently — add new
tools or fix existing ones without touching `omega-core` or `omega-loop`.

## Dependencies

- `omega-core` — types + `ToolRuntime` trait
- `omega-llm` — `ToolDefinition`, `ToolInputSchema`
- `tokio`, `async-trait`, `serde`, `serde_json`, `glob`, `uuid`, `tracing`

## Modules

| Module | Contents |
|---|---|
| **`tool`** | `Tool` trait — implement to add new tools |
| **`registry`** | `ToolRegistry` — register tools and execute them by name |
| **`common`** | BashTool, ReadTool, WriteTool, EditTool, GlobTool, GrepTool, TransferTool |
