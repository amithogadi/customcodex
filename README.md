# CustomCodex

A terminal-focused fork of [OpenAI Codex](https://github.com/openai/codex) using your configured model provider.

## Run

With the local alias configured in `~/.zshrc`, launch from any repository:

```sh
customcodex
```

Open a new terminal or run `source ~/.zshrc` to load the alias. It uses the local
development build and keeps your current working directory. No daemon runs by
default; use `customcodex --daemon` to opt in.

## What this fork changes

- Uses `~/.customcodex` for configuration and session data, and `.customcodex/config.toml` for project settings.
- Uses your selected provider without a separate ChatGPT model-catalog fallback.
- Removes ChatGPT account services, telemetry, feedback uploads, and hosted plugin discovery.
- Removes desktop/browser control, cloud tasks, voice, and automatic updates.
- Keeps terminal tools, sandboxing, approvals, MCP, local plugins, skills, subagents, and session resume.

Configure your model provider and credentials in `~/.customcodex/config.toml`.
`CODEX_HOME` overrides the home directory. See [configuration](docs/config.md).

## Limitations

- **No daemon by default:** agent work stops when the terminal process exits, and terminals do not share live agent sessions. Use `--daemon` for background sessions.
- Picking a different model starts a new chat.
- No web search.

## Build and launch directly

From this repository's root, using the [pinned Rust toolchain](codex-rs/rust-toolchain.toml):

```sh
cd codex-rs
cargo build --locked --release -p codex-cli --bin codex
./target/release/codex
```

[Build setup](docs/install.md) · [Compare with upstream snapshot](https://github.com/amithogadi/customcodex/compare/original...main) · [Apache-2.0 license](LICENSE) · [Attribution](NOTICE)
