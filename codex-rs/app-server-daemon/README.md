# codex-app-server-daemon

The daemon provides a local app-server that terminal clients can share. It keeps
agent work running independently of the terminal that started it.

## Commands

```sh
codex app-server daemon start
codex app-server daemon restart
codex app-server daemon stop
codex app-server daemon version
codex app-server daemon bootstrap
```

Each successful command prints one JSON object. Lifecycle responses report the
backend, socket path, local CLI version, and running app-server version when
available. `bootstrap` prepares the local managed package and starts its server.

The daemon serves its local control socket. Hosted remote control, device
pairing, automatic updates, and downloading installers are not supported.
Replace the installed package using your own build or package manager, then
restart the daemon to use it.

## Platform support

Linux, macOS, and Windows use platform-specific process and file-locking
primitives. Windows startup requires a non-elevated terminal whose host permits
detached child processes. Its canonical socket address must fit the 108-byte
AF_UNIX limit, including the terminator.

Shared clients use the environment inherited when the daemon started it. Starting
a new terminal does not change that environment. Per-client environment isolation
is not provided.

Setting `CODEX_EXEC_SERVER_URL` skips implicit daemon attachment so the selected
executor is preserved. If an implicitly discovered daemon cannot initialize a
connection, the TUI starts an embedded server. Explicit app-server endpoints
remain authoritative and report connection failures.

## Local state

State lives under `$CODEX_HOME/app-server-daemon` (normally
`~/.codex/app-server-daemon`). It includes the daemon PID and lock files, local
logs, recovery data, and `settings.json`.

`shutdownGraceSeconds` defaults to 60 and accepts values from 0 through 300.
The daemon waits this long for a graceful shutdown before forcing termination.
Stored feature overrides apply to later launches. Legacy remote-control and
updater settings are ignored and removed when settings are saved.

The managed package is local to `$CODEX_HOME/packages/app-server-daemon`.
Existing legacy packages remain usable. Starting an already running managed
server reuses it; restarting selects the current local package.
