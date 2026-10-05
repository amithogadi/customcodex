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
The [starter configuration](config.example.toml) defines DeepSeek V4.1 Flash with
high reasoning through OpenRouter, restricted to Together with ZDR enabled. It is
embedded in the binary and installed on first local launch if `config.toml` is
missing, then startup continues to the provider key prompt. Files containing only
preferences also receive the missing defaults, preserving existing settings. No
checkout or manual copy is needed. It is the central place to edit the starter
model catalog (rebuild to ship changes). Existing installations keep their own
provider/model settings, which populate the `/model` picker.

On a normal local launch, CustomCodex asks for the selected provider's API key
only when it is missing. Entering an OpenRouter key saves `OPENROUTER_API_KEY` in
the home `.env` and continues immediately. Keys already in that file or your
environment skip this prompt. Existing provider/model configurations are left unchanged.

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

To launch from any repository, add a `customcodex` alias to `~/.zshrc`. For a
development build:

```sh
alias customcodex='/path/to/customcodex/codex-rs/target/debug/codex'
```

Replace `/path/to/customcodex` with your checkout's absolute path. For the release build above, use
`target/release/codex` instead of `target/debug/codex`. Reload your shell settings,
then launch from the repository you want to work in:

```sh
source ~/.zshrc
customcodex
```

The alias preserves your current working directory and accepts normal CLI
arguments, such as `customcodex resume` or `customcodex --daemon`.

[Build setup](docs/install.md) · [Compare with upstream snapshot](https://github.com/amithogadi/customcodex/compare/original...main) · [Apache-2.0 license](LICENSE) · [Attribution](NOTICE)
