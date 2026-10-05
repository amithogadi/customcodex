# CustomCodex

A terminal-focused fork of [OpenAI Codex](https://github.com/openai/codex) using your configured model provider.

## What this fork changes

- **Configured model picker:** `/model` lists your configured models, including Isoquant and OpenRouter, without a bundled OpenAI catalog fallback. Switching models starts a new chat; the previous chat remains resumable.
- **OpenRouter routing:** optionally pin one provider or an ordered list with `openrouter_providers`. Requests stay within that list; omitting it allows automatic routing.
- **OpenRouter ZDR:** `openrouter_zdr` defaults to `true` per model, including automatic routing. Set it to `false` to opt out of the request-level requirement. Stricter account policies still apply; requests fail if no eligible endpoint remains.
- **Separate home:** configuration and session data live in `~/.customcodex`; project settings use `.customcodex/config.toml`.
- **Subagent limit:** defaults to 10 subagents per session, configurable in `config.toml`.
- **No daemon by default:** work stops when the terminal process exits. Use `--daemon` to opt into background sessions.
- Removes ChatGPT account services, telemetry, feedback uploads, and hosted plugin discovery.
- Removes desktop/browser control, cloud tasks, voice, and automatic updates.

## Configure

Put provider and model settings in `~/.customcodex/config.toml`, and API keys in
`~/.customcodex/.env` or your environment. The default home is created on startup.
If you set `CODEX_HOME`, that directory must already exist; config and `.env` are
then loaded from there. Restart after changing the model catalog.

See the [configuration guide](docs/config.md) for Isoquant and OpenRouter examples,
per-model reasoning, provider restrictions, and compatibility limits. OpenRouter
ZDR controls inference routing; local history and external tools are separate.
Isoquant uses its own `Isoquant-ZDR: required` header in the example configuration.

## Build and run

From this repository's root, using the [pinned Rust toolchain](codex-rs/rust-toolchain.toml):

```sh
cd codex-rs
cargo build --locked --release -p codex-cli --bin codex
./target/release/codex
```

To work in another repository, run this binary by its absolute path from that
repository. A `customcodex` shell alias is optional and must be configured locally.

[Build setup](docs/install.md) · [Compare with upstream snapshot](https://github.com/amithogadi/customcodex/compare/original...main) · [Apache-2.0 license](LICENSE) · [Attribution](NOTICE)
