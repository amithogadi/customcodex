# Configuration

This branch runs terminal agents and subagents using the selected provider in
`config.toml`. For example, an endpoint that supports the Responses API can be
configured as follows:

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

Set `CUSTOM_PROVIDER_API_KEY` in your environment. For a local endpoint that
requires no authentication, omit `env_key`. Custom providers require an explicit
`base_url`. The optional Responses proxy similarly requires `--upstream-url`.
Configured provider headers, credential commands, provider OAuth, and AWS
authentication remain supported.

Telemetry exporters, feedback uploads, hosted plugin discovery and installation,
ChatGPT account services, cloud-task commands, desktop/browser integration,
remote-control registration, voice sessions, and automatic update checks are
removed. Model requests use the selected provider; there is no separate ChatGPT
model-catalog fallback. Explicit MCP servers, local plugins and skills, shell and
file tools, sandbox policy, session history, and subagents remain available.

Old user settings for removed services produce warnings and have no effect,
including with `--strict-config`. Managed requirements that demand removed
capabilities fail with a configuration error. Startup ignores saved ChatGPT
credentials without modifying their files. Local diagnostic logs and token/status
accounting remain available.

On macOS, validate a built CLI with the isolated endpoint smoke test:

```sh
python3 scripts/custom_endpoint_smoke.py codex-rs/target/release/codex
```

It exercises interactive startup, shell execution, a subagent, and session resume
against a local mock provider. A subprocess-only network interposer records
connection attempts and rejects non-loopback destinations. The test also checks
that old ChatGPT credentials remain unchanged and retired telemetry settings stay
inert. This audits the CLI process; macOS may strip the interposer from protected
system executables launched by shell tools.

The upstream [basic](https://developers.openai.com/codex/config-basic) and
[advanced](https://developers.openai.com/codex/config-advanced) documentation can
still help with retained settings, but includes services absent from this branch.

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.
