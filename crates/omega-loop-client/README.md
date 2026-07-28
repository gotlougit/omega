# omega-loop-client

Client library for connecting to the `omega-loop` agent daemon over a Unix
socket.

Speaks a newline-delimited JSON protocol and provides `AgentdClient` for
sending `run`, `ask_response`, and other commands, along with typed event
deserialization for streaming responses.

## Usage

```rust,no_run
use omega_loop_client::{AgentdClient, SessionConfig};

let mut client = AgentdClient::connect().await?;
client.send_run("session-1", "Hello!", &SessionConfig::default()).await?;

while let Some(event) = client.recv_event().await? {
    // handle event
}
```
