# Configuration

## Configured model picker

`/model` lists models declared under `model_providers.<id>.models`. Selecting a
different entry starts a new chat; the previous conversation remains resumable.
Provider, model, and reasoning defaults are saved together after startup succeeds.
The picker identifies entries by both provider and model ID. Configured catalogs
do not fetch or fall back to the bundled OpenAI model list. Existing explicit
`model_catalog_json` configuration still takes precedence for runtime metadata.

For Isoquant and Qwen through OpenRouter, use:

```toml
model = "glm-5.3-flash"
model_provider = "isoquant"
model_reasoning_effort = "high"
model_reasoning_summary = "none"
web_search = "disabled"

[model_providers.isoquant]
name = "Isoquant"
base_url = "https://api.isoquant.ai/v1"
env_key = "ISOQUANT_API_KEY"
wire_api = "responses"
supports_websockets = false
http_headers = { "Isoquant-ZDR" = "required" }

[[model_providers.isoquant.models]]
id = "glm-5.3-flash"
name = "GLM-5.3-Flash"
context_window = 1048576
reasoning_effort = "high"

[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
supports_websockets = false

[[model_providers.openrouter.models]]
id = "qwen/qwen3.8-27b"
name = "Qwen-3.8-27B"
context_window = 65536
reasoning_effort = "high"
# Optional: restrict routing to one provider, or an ordered list.
# openrouter_providers = ["deepinfra"]
openrouter_zdr = true # Default when omitted.
```

The context windows above come from provider model metadata checked on 2026-10-04.
Use the endpoint's served limit, which may be smaller than the model's theoretical
maximum. The existing compaction limit is clamped to the model context window.
`Isoquant-ZDR: required` is preserved on Isoquant requests; it is not sent to other
providers.

Keys can be placed in `~/.customcodex/.env` (or `$CODEX_HOME/.env`), which the CLI
already loads. Use the environment variable names above; do not put keys in model
entries or commit them to this repository.

To add OpenRouter models, define its provider once, then add one `models` entry
per model. Replace the example values below with its exact model ID and served
context limit from `https://openrouter.ai/api/v1/models`:

```toml
[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
supports_websockets = false

[[model_providers.openrouter.models]]
id = "provider/model-id"
context_window = 32768 # Replace with the provider's advertised context limit.
# name = "My model"
# reasoning_effort = "high" # Only when supported by this model.
# openrouter_providers = ["provider-slug", "another-provider-slug"]
# openrouter_zdr = false # Explicit opt-out; omission enforces ZDR.
```

`openrouter_providers` is optional. Omit it (or use `[]`) for automatic routing.
Use one slug to pin a provider, or a list to try providers in that order, strictly
within that list. Requests send both `provider.order` and `provider.only`; if none
of your allowed providers is available, the request fails instead of using another
provider. These settings apply to every turn and retry. Use OpenRouter's exact
provider slugs, including endpoint variants when needed. Choose a context window
supported by every allowed provider; the Qwen example uses a conservative 65,536
tokens. The picker shows the configured provider restriction.
See [OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection).

`openrouter_zdr` is an optional per-model boolean, defaulting to `true` for
OpenRouter. Every OpenRouter Responses request sends `provider.zdr`, even with
automatic routing or a model absent from the configured catalog. Explicit `false`
removes the request-level ZDR requirement; it cannot disable stricter OpenRouter
account or guardrail policies. It does not require a non-ZDR endpoint.

ZDR is enforced together with `openrouter_providers`: a request fails if none of
the allowed endpoints is ZDR-eligible. Retries never relax either restriction.
The setting applies to turns, continuations, subagents, and tool-free compaction.
OpenRouter requires `supports_websockets = false` so requests use this HTTP path.
Using `openrouter_zdr` on a non-OpenRouter provider is a configuration error.

This controls inference-provider routing, not local conversation history or
external tools. `store=false` remains unchanged and is separate from ZDR.
Isoquant continues to use its existing `Isoquant-ZDR` header.
See [OpenRouter ZDR documentation](https://openrouter.ai/docs/guides/features/zdr).

As checked on 2026-10-04, OpenRouter's Cerebras endpoint for this Qwen model does
not advertise tool calling and rejects tool requests. DeepInfra passed a real CLI
tool-call and continuation check. Use a tool-capable endpoint for coding, or leave
routing automatic. A provider can support tools directly without supporting them
through OpenRouter.

Restart the CLI after editing the catalog. This first version supports text input,
text output, and function tools. Choose models advertising tool calling; image,
audio, hosted search, and image-generation tools are not advertised by these
catalog entries. Reasoning is a per-entry setting. OpenRouter uses stateless
Responses requests (`store=false`, full history, no `previous_response_id`).
Both services use the existing Responses transport. OpenRouter's function-tool
translation preserves native tool identities in local history, including subagent
messages. Configured OpenRouter models use direct function tools even when
`code_mode` or `code_mode_only` is enabled, because the adapter does not support
the freeform code-mode tool. There is no direct Cerebras adapter. Legacy
`wire_api="chat"` remains unsupported.

## Subagent limits

Both subagent backends default to 10 subagents per session, excluding the main
agent. Override the limit in `config.toml`:

```toml
[agents]
max_concurrent_threads_per_session = 10
max_depth = 1
```

The default V1 backend limits open subagent threads; close them to free slots.
Its default depth of `1` permits children but not grandchildren. V2 ignores
`max_depth` and counts the main agent in its session cap, so its default is `11`.
An explicit `features.multi_agent_v2.max_concurrent_threads_per_session` overrides
the shared `[agents]` limit and includes the main agent.

## Provider configuration

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
