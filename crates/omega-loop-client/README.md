# omega-loop-client

Client library for connecting to the `omega-loop` agent daemon over a Unix
socket.

Speaks a newline-delimited JSON protocol and provides `AgentdClient` for
sending `run`, `ask_response`, and other commands, along with typed event
deserialization for streaming responses.

## Usage

```rust,no_run
use omega_loop_client::{connect, SessionConfig};

let (reader, mut writer) = connect().await?;
// Requires the API key to be set in the environment for the daemon.
writer.send_run("session-1", "Hello!", &SessionConfig::default(), None, None, None).await?;

while let Some(event) = reader.recv_event().await? {
    // handle event, e.g. ServerEvent::RoleList { roles }
}
```

The final argument to `send_run` is an optional role name: when set and the
session does not exist yet, the daemon creates it using that role's
alternative system prompt (roles are configured in NixOS via
`services.omega.roles`).
