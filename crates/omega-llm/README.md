# omega-llm

LLM provider abstraction with unified types and multiple backends.

Provides the `LlmProvider` trait that abstracts over different LLM APIs, along
with a shared type system (messages, content blocks, tool definitions, streaming
events) modelled after the Anthropic Messages API format.

## Providers

| Provider | Module | Env vars |
|---|---|---|
| **OpenAI** (Chat Completions) | `openai` | `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL` |

### Adding a new provider

Implement `LlmProvider` and its three required methods:
- `send_message` — simple text-in/text-out
- `send_with_tools_and_system` — full non-streaming request
- `stream_with_tools_and_system` — streaming request

The trait's internal types follow Anthropic's schema; providers with a different
wire format handle translation internally.
