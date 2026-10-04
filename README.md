# CustomCodex

CustomCodex is a customized fork of [OpenAI Codex](https://github.com/openai/codex)
focused on terminal agents and subagents using the model provider you configure.
It retains local coding tools while removing hosted account services and integrations
that are unnecessary for this workflow. This is an independent fork, not an official
OpenAI distribution.

## What changed

Compared with the upstream snapshot preserved on [`original`](https://github.com/amithogadi/customcodex/tree/original):

| Area                        | Changes in CustomCodex                                                                                                                                                                                                                        |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Model providers             | Model requests use the selected provider in `config.toml`. Custom providers require an explicit endpoint; there is no separate ChatGPT model-catalog fallback.                                                                                |
| Accounts                    | Removed ChatGPT login, account services, and remote-control registration. Provider credentials, API keys, provider OAuth, and AWS authentication remain supported.                                                                            |
| Telemetry                   | Removed analytics and telemetry exporters, feedback uploads, and their dependencies. Local diagnostics and token/status accounting remain available.                                                                                          |
| Plugins and tools           | Removed hosted plugin discovery and installation. Explicitly configured MCP servers, local plugins, and skills remain available.                                                                                                              |
| Desktop and browser         | Removed desktop-app integration and built-in browser/computer-control integrations.                                                                                                                                                           |
| Cloud and voice             | Removed cloud-task commands, live voice/realtime sessions, and the bundled voice runtime.                                                                                                                                                     |
| Updates                     | Removed automatic update checks and update commands. Build this checkout to use or update the customized CLI.                                                                                                                                 |
| Configuration compatibility | Retired user settings warn and have no effect, including under `--strict-config`. Managed requirements that demand removed capabilities fail with a configuration error. Saved ChatGPT credentials are ignored without modifying their files. |
| Supporting code             | Updated protocol schemas, SDK surfaces, build/release definitions, documentation, and tests to reflect the removed capabilities. Added a custom-endpoint smoke test.                                                                          |

The terminal UI, non-interactive execution, shell and file tools, sandboxing and
approvals, session history and resume, and subagents remain available. The built
executable is still named `codex`.

## Branches and upstream reference

- **`main`** contains the customized implementation, including the work developed on the former local `custom` branch.
- **`original`** preserves the former local `main` at upstream commit [`b741e480e2`](https://github.com/openai/codex/commit/b741e480e2) ("Allow transcript selection and copying while bottom modals are open (#50564)"). It is a fixed reference snapshot for finding or restoring upstream functionality.

The upstream Git history is retained. To inspect the differences or read an original file:

```sh
git fetch origin
git diff origin/original..origin/main --stat
git diff origin/original..origin/main -- path/to/file
git show origin/original:path/to/file
```

You can also [compare the branches on GitHub](https://github.com/amithogadi/customcodex/compare/original...main).

## Build

Clone this repository and build the customized CLI:

```sh
git clone https://github.com/amithogadi/customcodex.git
cd customcodex/codex-rs
cargo build --locked --release -p codex-cli --bin codex
```

The Rust toolchain is pinned in [`codex-rs/rust-toolchain.toml`](codex-rs/rust-toolchain.toml).
See [build instructions](docs/install.md) for prerequisites and development tools.
Upstream installers and packages install the upstream version.

## Launch CustomCodex

Configure your provider in `~/.customcodex/config.toml`, for example:

```toml
model = "your-model"
model_provider = "custom"

[model_providers.custom]
name = "My endpoint"
base_url = "https://your-provider.example/v1"
wire_api = "responses"
env_key = "CUSTOM_PROVIDER_API_KEY"
requires_openai_auth = false
```

Set `CUSTOM_PROVIDER_API_KEY` in your shell environment. From the repository root
(`customcodex/`), type this command to launch the CustomCodex build. If you are
still in `codex-rs/` after building, run `cd ..` first:

```sh
./codex-rs/target/release/codex
```

The executable is named `codex`, but this path selects the customized binary
built above.
It runs without a daemon by default. To request that behavior explicitly:

```sh
./codex-rs/target/release/codex --no-daemon
```

For a development build created without `--release`, use
`./codex-rs/target/debug/codex --no-daemon` instead.

This fork defaults to `~/.customcodex` for configuration and session data.
Set `CODEX_HOME` explicitly if you want to override that directory.
Project configuration is loaded from `.customcodex/config.toml` within the project.
Existing configuration must be moved or copied to the new location manually.
For an endpoint without authentication, omit `env_key`. See
[provider configuration](docs/config.md) for more options and compatibility details.

Interactive launches, including `codex resume` and `codex fork`, use an embedded
server by default. They do not discover or start a background daemon. Tools,
subagents, and saved session history remain available; running work depends on
the terminal process staying alive.

Use `./codex-rs/target/release/codex --daemon` to opt into the shared background
server. Resume and fork also accept `--daemon`. The `features.daemon_auto_start`
setting controls automatic startup only after opting in; setting it to `false` still
allows attachment to an existing daemon. `--no-daemon` remains supported.
Explicit `--remote`, `codex agents`, `codex queue`, and `codex app-server daemon`
commands retain their shared-server behavior.

## Validation

On macOS, run the isolated smoke test from the repository root after building:

```sh
python3 scripts/custom_endpoint_smoke.py codex-rs/target/release/codex
```

It exercises interactive startup, shell execution, a subagent, and session resume
against a local mock provider. It checks that saved ChatGPT credentials remain
unchanged and retired telemetry settings stay inert. A network interposer audits
the CLI process and blocks non-loopback destinations during the test; protected
macOS executables launched by shell tools can strip the interposer. This is a test
of those workflows, not a guarantee about all network activity from configured
tools or providers.

See [build instructions](docs/install.md) for the crate test workflow. Upstream
documentation and release automation may still refer to services or infrastructure
that this fork does not provide.

## License

Licensed under the [Apache-2.0 License](LICENSE). See [NOTICE](NOTICE) for upstream attribution.
