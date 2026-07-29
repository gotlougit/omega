# omega-llm

LLM provider abstraction with unified types and multiple backends.

Provides the `LlmProvider` trait that abstracts over different LLM streaming
APIs, along with a shared type system (messages, content blocks, tool definitions,
streaming events).

## Providers

| Provider | Module | Env vars |
|---|---|---|
| **OpenAI** (Chat Completions) | `openai` | `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL` |

### Adding a new provider

Implement `LlmProvider` and its required method:
- `stream_with_tools_and_system` — streaming request with tools and system prompt

The trait's internal types provide a unified message format; providers with a
different wire format handle translation internally.
