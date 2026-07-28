# omega-sh

Persistent Unix-socket daemon for filesystem and shell tools.

Handles `Read`, `Write`, `Edit`, `Bash`, `Glob`, and `Grep` tool requests from
one or more omega agent processes. Offloads shell/filesystem operations from
the agent daemon into a dedicated process.

## Protocol

Newline-delimited JSON over a Unix stream socket.

```json
{"id":"<uuid>","tool":"Read","args":{"file_path":"..."}}
{"id":"<uuid>","result":{"content":{"type":"Text","data":"..."},"is_error":false}}
```

## Environment

- `OMEGA_SOCKET_PATH` — Unix socket path (default: `/tmp/omega-sh.sock`)
