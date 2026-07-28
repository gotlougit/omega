# omega-core

Pure types crate consumed by `omega-loop`, `omega-tools`, `omega-sh-client`, etc.
Contains zero logic — just data structures and one trait interface.

## Dependencies

`serde`, `serde_json`, `thiserror`, `async-trait` — nothing else.

## Modules

| Module | Contents |
|---|---|
| **`core`** | `AgentState`, `AgentContext`, `OutputChunk`, `InputMessage`, `FrameworkError`, `ToolResult`, `ToolResultData`, `ToolInfo`, `UserQuestion`, `QuestionOption`, `ToolRuntime` trait |
