# omega-llm

LLM provider abstraction with unified types and multiple backends.

Provides the `LlmProvider` trait that abstracts over different LLM streaming
APIs, along with a shared type system (messages, content blocks, tool definitions,
streaming events).

## Providers

| Provider | Module | Env vars |
|---|---|---|
| **OpenAI** (Chat Completions or Responses) | `openai` | `OPENAI_API_TYPE`, `OPENAI_API_KEY`, `OPENAI_MODEL`, Responses OAuth variables |

Chat Completions is the default. Set `OPENAI_API_TYPE=responses` to use the
Responses API with Codex credentials. Responses mode requires
`OPENAI_RESPONSES_ACCESS_TOKEN` and `OPENAI_RESPONSES_ACCOUNT_ID` and optionally
uses `OPENAI_RESPONSES_REFRESH_TOKEN` for automatic OAuth refresh. Refreshed and
rotated credentials are atomically written to `OPENAI_RESPONSES_ENV_FILE` when
configured. `OPENAI_RESPONSES_BASE_URL` defaults to
`https://chatgpt.com/backend-api/codex/responses`. `OPENAI_API_KEY` remains
exclusive to Chat Completions mode. `OPENAI_REASONING_EFFORT` explicitly sets
the Responses `reasoning.effort` value and overrides the effort inferred from a
request's thinking budget.

### Adding a new provider

Implement `LlmProvider` and its required method:
- `stream_with_tools_and_system` — streaming request with tools and system prompt

The trait's internal types provide a unified message format; providers with a
different wire format handle translation internally.
