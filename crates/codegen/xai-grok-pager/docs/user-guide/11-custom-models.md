# Custom Models

Grok connects to custom model endpoints for alternative providers, self-hosted models, and overriding built-in settings. This guide explains how to add models from Settings, select them, configure `[model.*]` endpoints, and integrate third-party providers.

---

## Default Models

New sessions start with `grok-4.5`. The built-in catalog also includes
provider-specific fallbacks such as OpenAI Codex and Kimi; each provider keeps
its own authentication. Authenticate the provider you want to use, then select
its model.

List all available models:

```bash
open-grok models
```

---

## Selecting a Model

### CLI Flag

```bash
open-grok -p "Hello" -m grok-build
```

### Slash Command

In the TUI, switch models during a session:

```
/model grok-build
```

Or use the alias:

```
/m grok-build
```

### Model Picker (Ctrl+M)

Press `Ctrl+M` from the scrollback pane to open the model picker. It lists all available models, both built-in and custom, and lets you switch with a single keystroke. With the prompt focused, `Ctrl+M` toggles multiline input instead -- use `/model` to switch without leaving the prompt.

### Config Default

Set a persistent default in `~/.opengrok/config.toml`:

```toml
[models]
default = "grok-4.5"
```

---

## Supported API Backends

Grok supports three API backends. Set `api_backend` in your `[model.*]` config to choose which protocol the model uses:

| Value | API | Default |
|-------|-----|---------|
| `"chat_completions"` | OpenAI Chat Completions (`/v1/chat/completions`) | Yes |
| `"responses"` | OpenAI Responses (`/v1/responses`) | |
| `"messages"` | Anthropic Messages (`/v1/messages`) | |

When you omit `api_backend`, Grok uses `chat_completions`.

To send provider-specific authentication or version headers -- for example, Anthropic's `x-api-key` -- use the `extra_headers` field described below. Grok sends those headers verbatim with every request to the endpoint.

---

## Settings → Custom models

You can add, override, and remove custom models from the TUI without
hand-editing `config.toml`. Open **Settings → Models → Custom models**.

That group writes the same `[model.<key>]` tables documented below. You can
still edit `~/.opengrok/config.toml` by hand for every field, including
advanced options the form does not expose (`extra_headers`, `query_params`,
`temperature`, and so on).

### Existing models

The group lists every `[model.*]` entry already in your user config. Each
row is selected while the model is present. Deselect a row to delete that
table only; other models and unrelated config stay as they are.

### Add a model

Fill the draft fields, then turn on **Save custom model**:

| Field | What to enter |
| --- | --- |
| Catalog key | Table name / catalog key (`[model.<key>]`), for example `zai:glm-special` or `my-ollama`. Letters, digits, `:`, `.`, `-`, and `_` only; no spaces or newlines. |
| Model id | Wire model id sent to the API. |
| Name | Optional display name in the picker. |
| Provider | `(inherit)` (empty) or `zai`, `runinfra`, `gemini`, `wafer`, `kimi`, `fireworks`, `deepseek`, `meta`, `xai`, `opencode_go`, `openrouter`. |
| Base URL | Optional OpenAI-compatible endpoint. Leave blank for Z AI, RunInfra, Google Gemini, Wafer, or OpenRouter to use that provider's default endpoint. |
| Context window | Token window used for auto-compaction (`1000`–`4000000`; default `200000`). |
| API backend | `chat_completions` (default), `responses`, or `messages`. |
| Env key | Environment variable that holds the API key. Prefer this over putting a key in the file. |

**Save custom model** requires a non-empty catalog key and model id. If
either is missing, Open Grok shows a warning and does not write config.
On success the draft fields clear, the new model appears in the list and
the model picker, and Settings turns **Save custom model** back off.

When you choose the Z AI provider and omit a base URL, Open Grok stores
the GLM Coding Plan endpoint (`https://api.z.ai/api/coding/paas/v4`, or
`OPENGROK_ZAI_API_BASE_URL` if set) and `env_key = "ZAI_API_KEY"`. RunInfra
does the same with `https://api.runinfra.ai/v1` and `RUNINFRA_GATEWAY_KEY`.
Google Gemini defaults to
`https://generativelanguage.googleapis.com/v1beta/openai/` and
`env_key = "GEMINI_API_KEY"`.
Wafer does the same with `https://pass.wafer.ai/v1` and `WAFER_API_KEY`.
OpenRouter does the same with `https://openrouter.ai/api/v1` and
`OPENROUTER_API_KEY`. That keeps API-key-only providers from inheriting an
empty or xAI endpoint.

---

## Configuring Custom Models

Add custom model endpoints in `~/.opengrok/config.toml` under `[model.<name>]`
sections. **Settings → Models → Custom models** writes these same tables:

```toml
[model.my-model]
model = "model-id"                        # Model identifier sent to the API
base_url = "https://api.example.com/v1"   # OpenAI-compatible endpoint
name = "Display Name"                     # Shown in the model picker
description = "Model description"          # Optional description
api_key = "sk-..."                        # API key for this provider (optional)
env_key = "XAI_API_KEY"                   # Env var holding the API key (optional; string or array)
api_backend = "chat_completions"          # "chat_completions", "responses", or "messages"
supports_standalone_web_search = true     # Opt in to /alpha/search when the endpoint supports it
temperature = 0.7                         # Sampling temperature
top_p = 0.95                              # Nucleus sampling parameter
max_completion_tokens = 8192              # Maximum tokens per response
context_window = 128000                   # Total context window in tokens
extra_headers = { "x-api-key" = "sk-..." } # Extra request headers, sent verbatim (optional)
query_params = { api-version = "2026-07-22" } # Query params appended to every request URL (optional)
env_http_headers = { "X-Tenant" = "TENANT_TOKEN" }    # Headers from env vars, resolved at client build (optional)
```

### Credential Resolution

Grok resolves the API key in this order:

1. The `api_key` field in the model config
2. The environment variable(s) named by `env_key` — a single string or an array of names. The first set, non-empty value wins (for example `env_key = ["ANTHROPIC_AUTH_TOKEN", "LC_ANTHROPIC_AUTH_TOKEN"]` for SSH `LC_*` forwarding)
3. Your signed-in session token (from `open-grok login`), for a model with no `api_key`/`env_key` of its own
4. The `XAI_API_KEY` environment variable (global fallback; Grok also accepts `GROK_CODE_XAI_API_KEY` for backward compatibility)

### Context Window

The `context_window` value tells Grok when to trigger auto-compaction. The
Context window field in **Settings → Models → Custom models** writes this
same key. When you override a known model, Grok inherits that model's
context window unless you set `context_window`. When you define a new model
and omit `context_window`, Grok defaults to 200,000 tokens, so set it
explicitly to match your provider. Z AI's live `/models` list does not
include a window size; see [Z AI](#z-ai) for the curated defaults.

### Global Default Headers

To apply the same headers to *every* model in the catalog -- built-in, prefetched from `/v1/models`, or custom -- set them once under the global `[models]` section instead of repeating them per model:

```toml
[models]
extra_headers = { "X-Request-Tags" = "team=example,env=prod" }
```

These act as a base for each model's inference requests. A per-model `[model.<id>].extra_headers` entry overrides the global default **per key** (matched case-insensitively): a key set on the model wins, while any global-only keys are still inherited by that model. Like the per-model field, they ride on that model's inference calls -- not on separate services such as image generation or video generation -- which makes them handy for attribution tags (for example, cost tracking) without re-declaring them whenever a new model appears.

### Global Default Values

A few common per-model settings can also be set once under `[models]` as a default for *every* model. A per-model `[model.<id>]` value always wins; the global only fills in where a model (or the server's model list) left the field unset:

```toml
[models]
temperature                 = 0.7
top_p                       = 0.95
max_completion_tokens       = 8192
max_retries                 = 8
inference_idle_timeout_secs = 600
stream_tool_calls           = true
```

This is a small, fixed set of environment-wide knobs. Settings that identify a specific model (`model`, `base_url`, `api_key`, `context_window`, ...) cannot be defaulted this way, and a few settings with their own dedicated configuration -- auto-compaction (`[session]`), the system-prompt label (`[agent]`), and reasoning effort (`[models].default_reasoning_effort`) -- keep their existing homes.

> **Note on `stream_tool_calls`:** this one affects request *shape*, not just sampling. Open Grok only sends the `stream_tool_calls` field to xAI Responses. Codex, DeepSeek, Meta, Chat Completions, and Messages already stream argument fragments without a flag. The user-facing switch is Settings → Advanced → Stream tool calls (`[ui].stream_tool_calls`, default on). A global `[models] stream_tool_calls` value is a fallback when the UI field is unset. If a BYOK Responses endpoint rejects the field, opt that model out with `stream_tool_calls = false` in its `[model.<id>]` block.

### Request Query Parameters

Some gateways route or version on the query string. `query_params` appends percent-encoded query parameters to every request Grok makes for a model. For example, a gateway that selects an API version this way:

```toml
[model.my-gateway]
model = "my-model"
base_url = "https://gateway.example/v1"
api_backend = "responses"
env_key = "GATEWAY_API_KEY"
query_params = { api-version = "2026-07-22" }
```

A key that also appears in the `base_url` query string is overridden (last value wins) rather than duplicated. Query parameters are saved in the session, so do not put secrets in them: use `env_http_headers` for a secret.

### Environment-Variable Headers

`env_http_headers` maps a request header to the name of an environment variable that supplies its value, so a per-request secret never has to be written into `config.toml`:

```toml
[model.gateway]
model = "my-model"
base_url = "https://gateway.example/v1"
env_http_headers = { "X-Tenant-Token" = "GATEWAY_TENANT_TOKEN" }
```

Grok reads each variable when it builds the client for a session and places the value in the request headers only, never on disk. A header is skipped when its variable is unset or blank, and a resolved value overrides an `extra_headers` entry of the same name. Use `extra_headers` for a static value and `env_http_headers` for one that comes from the environment.

Both fields also work on a shared `[model_providers.<id>]` block. A model that points at a provider with `model_provider = "<id>"` inherits the provider's `query_params` and `env_http_headers` when it sets none of its own, matching how `extra_headers` is inherited.

---

## Overriding Built-in Models

You can override specific fields of built-in models without redefining everything. Only specify the fields you want to change:

```toml
# Override only the API key for a default model
[model.grok-build]
api_key = "my-api-key"

# Override temperature and add a custom API key
[model.grok-build]
temperature = 0.5
api_key = "sk-custom"
```

When you override a built-in model, Grok starts with the default configuration (including the correct `base_url`), then applies only the fields you specify. Unspecified fields inherit from the default.

### Priority Order

1. Your config (`[model.*]`, including tables written from Settings) -- highest priority
2. Live provider catalogs (Z AI, RunInfra, Google Gemini, Wafer, and OpenRouter `/models`, plus other prefetched `/v1/models` lists)
3. Hardcoded defaults -- lowest priority

A live Z AI, RunInfra, Google Gemini, Wafer, or OpenRouter catalog replace rebuilds that provider's picker
entries, then Open Grok re-applies `[model.*]`. Custom models that the
remote list does not return stay in the catalog, and field overrides on a
live id (for example a larger `context_window`) win.

---

## Provider Examples

### Kimi coding models

Kimi has two isolated services. Open **Settings → Models → Kimi service** to
choose Platform or Code, then paste the matching API key. Open Grok stores each
credential separately, queries that service's `/models` endpoint when possible,
and refreshes the model picker. Environment overrides remain available:
`MOONSHOT_API_KEY` for Platform and `KIMI_CODE_API_KEY` for Code.

| Service | Built-in models | Base URL | Credential |
| --- | --- | --- | --- |
| Platform | `kimi-k3` | `https://api.moonshot.ai/v1` | `MOONSHOT_API_KEY` |
| Code | `k3`, `k3-256k`, `kimi-for-coding`, `kimi-for-coding-highspeed` | `https://api.kimi.com/coding/v1` | `KIMI_CODE_API_KEY` |

Code `k3` is not the same slug as Platform `kimi-k3`. The membership API uses
`k3` / `k3-256k`; the pay-as-you-go Platform API uses `kimi-k3`.

For an explicit config-only Platform setup:

```toml
[model.kimi-k3]
model = "kimi-k3"
name = "Kimi K3"
provider = "kimi"
base_url = "https://api.moonshot.ai/v1"
api_backend = "chat_completions"
env_key = "MOONSHOT_API_KEY"
context_window = 1048576
```

For Kimi Code membership models:

```toml
[model.k3]
model = "k3"
name = "Kimi K3"
provider = "kimi"
base_url = "https://api.kimi.com/coding/v1"
api_backend = "chat_completions"
env_key = "KIMI_CODE_API_KEY"
context_window = 1048576
```

Kimi uses standard client-side function tools. Open Grok does not add
Kimi-platform-hosted tools to this provider profile.

### DeepSeek API

Open **Settings → Models → DeepSeek API key** or run `/login deepseek`, then
select one of the provider-owned entries:

| Built-in model | Backend | Notes |
| --- | --- | --- |
| `deepseek:deepseek-v4-flash` | Responses | Stable API ID for DeepSeek-V4-Flash-0731; native hosted web search; 1M context |
| `deepseek:deepseek-v4-pro` | Chat Completions | Remains on Chat Completions until DeepSeek enables Responses support |

`DEEPSEEK_API_KEY` is the environment alternative. The Flash model keeps the
stable wire ID `deepseek-v4-flash`, so it automatically reaches the current
0731 release without a versioned slug. Its Responses route is stateless: Open
Grok sends full input and does not attach Codex OAuth, prompt-cache keys,
turn-state headers, or remote-compaction behavior.

For an explicit config-only Flash setup:

```toml
[model.deepseek-flash]
model = "deepseek-v4-flash"
name = "DeepSeek V4 Flash 0731"
provider = "deepseek"
base_url = "https://api.deepseek.com"
api_backend = "responses"
env_key = "DEEPSEEK_API_KEY"
context_window = 1000000
max_completion_tokens = 384000
reasoning_effort = "high"
```

### Wafer AI

Wafer AI provides an OpenAI-compatible Chat Completions endpoint. Its provider
catalog is dynamic: Open Grok queries `GET /v1/models` at
`https://pass.wafer.ai/v1` rather than relying on a static model list. That
response lists ids only, so Open Grok assigns published context windows:
**Kimi K3 / K3 Fast**, **DeepSeek V4 Flash**, and **GLM 5.2 / 5.2 Flash** are
**1,000,000** tokens; **Kimi K2.6** is **262,144**; **GLM 5.1** is
**203,000**. Other Wafer ids stay at the 200,000 fallback. Override any value
with `context_window` in `[model.*]`. Set `WAFER_API_KEY` and select one of
the model IDs returned by that endpoint:

```toml
[model.wafer-model]
model = "your-wafer-model-id"
name = "Wafer model"
provider = "wafer"
base_url = "https://pass.wafer.ai/v1"
api_backend = "chat_completions"
env_key = "WAFER_API_KEY"
```

Wafer accepts standard client function tools. It has no native hosted web
search, Responses API, OAuth flow, or xAI-only export path. Keep the Wafer API
key provider-local; do not use `XAI_API_KEY` or an xAI session as a substitute.

### Z AI

Z AI serves GLM models over an OpenAI-compatible Chat Completions endpoint.
Its provider catalog is dynamic: Open Grok queries `GET /models` at Z AI's
GLM Coding Plan endpoint (`https://api.z.ai/api/coding/paas/v4` by default;
override with `OPENGROK_ZAI_API_BASE_URL`) rather than relying on a static
model list. That `/models` response lists ids only — it does not include
`context_window`, `context_length`, or `max_model_len`. Open Grok therefore
assigns context windows from published GLM sizes: **glm-5.3** and
**glm-5.2** are **1,000,000** tokens (max output 131,072); other listed GLM
text models are **200,000**. Override either value with `context_window` in `[model.*]` or
the Context window field under **Settings → Models → Custom models**.
**glm-5**, **glm-5.1**, **glm-5.2**, and **glm-5.3** are text-only models:
they cannot accept image input, and reading an image file returns an error
to the model.

Set `ZAI_API_KEY` (or connect it with `/login zai`) and pick one of the
returned GLM model IDs. To add a GLM id that is not in the live list, use
Settings or a `[model.*]` table with `provider = "zai"`:

```toml
[model.zai-model]
model = "glm-5.2"
name = "GLM 5.2"
provider = "zai"
base_url = "https://api.z.ai/api/coding/paas/v4"
api_backend = "chat_completions"
env_key = "ZAI_API_KEY"
context_window = 1000000
reasoning_effort = "high"   # low | medium | high | max on reasoning GLM models
```

User `[model.*]` entries win over the live catalog: a custom Z AI model
that `/models` does not return stays in the picker, and an override on a
live id (for example `[model.zai:glm-5.2]`) keeps your `context_window`
after the catalog refreshes.

Reasoning-capable GLM models accept `reasoning_effort` up to `max`; Open
Grok sends Z AI's `thinking` mode switch automatically whenever an effort is
set. Z AI accepts standard client function tools. It has no native hosted
web search, Responses API, OAuth flow, or xAI-only export path. Keep the Z
AI API key provider-local; do not use `XAI_API_KEY` or an xAI session as a
substitute.

### RunInfra

RunInfra serves a small hosted Chat Completions lineup at
`https://api.runinfra.ai/v1`. Open Grok queries `GET /v1/models` and uses
that list as the picker when it returns models; a curated fallback
(`deepseek-v4-flash`, `nemotron-3-5-lightning-30b`, `qwen3-8-2-4t-a95b`,
`qwen3-8-27b`) keeps the picker populated when the endpoint is unreachable.
Wire `context_window` and `max_output_tokens` win when the live catalog
sends values greater than zero.

Set `RUNINFRA_GATEWAY_KEY` (or `RUNINFRA_API_KEY`, or connect it with
`/login runinfra`) and pick one of the returned ids. To add an id that is
not in the live list, use Settings or a `[model.*]` table with
`provider = "runinfra"`:

```toml
[model.runinfra-flash]
model = "deepseek-v4-flash"
name = "DeepSeek V4 Flash"
provider = "runinfra"
base_url = "https://api.runinfra.ai/v1"
api_backend = "chat_completions"
env_key = "RUNINFRA_GATEWAY_KEY"
context_window = 1048576
reasoning_effort = "max"   # none | low | medium | high | max
```

User `[model.*]` entries win over the live catalog. Known hosted models
reason by default; `deepseek-v4-flash` defaults to max and rewrites
high/xhigh/max to `max`. `qwen3-8-2-4t-a95b` cannot turn thinking off.
Unknown live deployments do not get a reasoning menu. RunInfra accepts
standard client function tools. It has no native hosted web search,
Responses API, OAuth flow, or xAI-only export path. Keep the RunInfra API
key provider-local; do not use `XAI_API_KEY` or an xAI session as a
substitute.

### Google Gemini

Google Gemini (AI Studio) serves curated Chat Completions models at
`https://generativelanguage.googleapis.com/v1beta/openai/`. Open Grok keeps
four curated entries (`gemini-3.7-flash`, `gemini-3.6-flash`,
`gemini-3.5-flash-lite`, `gemini-3.1-pro-preview`) under catalog keys
`gemini:{id}`; live `/models` enrich those entries only.

Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`, or connect it with `/login gemini`)
and pick one of the curated ids. To add an id that is not curated, use
Settings or a `[model.*]` table with `provider = "gemini"`:

```toml
[model.gemini-flash]
model = "gemini-3.6-flash"
name = "Gemini 3.6 Flash"
provider = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai/"
api_backend = "chat_completions"
env_key = "GEMINI_API_KEY"
context_window = 1048576
reasoning_effort = "medium"   # see model-specific menus below
```

Gemini 3 cannot use reasoning effort `none`. `gemini-3.7-flash` and
`gemini-3.1-pro-preview` reject `minimal` (menu is low/medium/high);
`gemini-3.6-flash` and `gemini-3.5-flash-lite` offer minimal/low/medium/high.
Defaults: 3.7-flash Medium, 3.6-flash Medium, 3.5-flash-lite Minimal,
3.1-pro-preview High. Google Gemini accepts standard client function tools.
It has no native hosted web search, Responses API, OAuth flow, or xAI-only
export path. Keep the Gemini API key provider-local; do not use `XAI_API_KEY`
or an xAI session as a substitute.

### OpenRouter

OpenRouter is an isolated Chat Completions gateway at
`https://openrouter.ai/api/v1`. Open Grok queries
`GET /models` for text-input/output, tool-capable models. The
`openrouter_enabled_models` list is an explicit opt-in allowlist; an empty
list enables none. Select models in **Settings → Models → OpenRouter models**
to make them available in the picker and to subagents.

Set `OPENROUTER_API_KEY` (or connect it with `/login openrouter`), enable a
returned id, and select it. Reasoning menus use only that model's live `supported_efforts`
array. Models that omit the field have no effort selector. To add an id that
is not in the live list, use Settings or a `[model.*]` table with
`provider = "openrouter"`:

```toml
[model.openrouter-model]
model = "your-openrouter-model-id"
name = "OpenRouter model"
provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
```

OpenRouter accepts standard client function tools. Chat Completions
inference sends nested `reasoning: { effort }` and reads thinking tokens from
stream `delta.reasoning`. It has no native hosted web search, Responses API,
OAuth flow, or xAI-only export path. Keep the OpenRouter API key
provider-local; do not use `XAI_API_KEY` or an xAI session as a substitute.

### Anthropic (Claude)

Use Claude models directly via the Anthropic Messages API:

```toml
[model.claude-opus]
model = "claude-opus-4-6"
base_url = "https://api.anthropic.com/v1"
name = "Claude Opus 4.6"
api_backend = "messages"
context_window = 200000
extra_headers = { "x-api-key" = "sk-ant-...", "anthropic-version" = "2023-06-01" }
```

The `messages` backend uses the Anthropic Messages protocol. Anthropic authenticates with an `x-api-key` header rather than `Authorization: Bearer`, so pass your key through `extra_headers`, which Grok sends verbatim.

### OpenAI (Chat Completions)

```toml
[model.gpt-4o]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o"
env_key = "OPENAI_API_KEY"
```

`api_backend` defaults to `"chat_completions"`, so you don't need to set it explicitly for OpenAI.

### OpenAI (Responses API)

If your provider supports the newer Responses API:

```toml
[model.gpt-4o-responses]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o (Responses)"
api_backend = "responses"
env_key = "OPENAI_API_KEY"
```

Responses endpoints that implement Codex's standalone `/alpha/search` contract
can opt in explicitly:

```toml
[model.my-codex-gateway]
model = "gpt-custom"
provider = "codex"
base_url = "https://gateway.example/v1"
api_backend = "responses"
env_key = "GATEWAY_API_KEY"
supports_standalone_web_search = true
```

When native web search is selected, this registers `web__run`, including inside
Code Mode as `tools.web__run(...)`, and suppresses hosted `web_search`. If the
flag is absent or false, Open Grok keeps the hosted-search fallback. Official
ChatGPT Codex OAuth routes enable standalone search automatically; public
API-key routes do not.

### Ollama (Local Models)

Run models locally with [Ollama](https://ollama.ai):

```toml
[model.ollama-codellama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "CodeLlama (Ollama)"
```

Make sure Ollama is running (`ollama serve`) and the model is pulled (`ollama pull codellama`).

### Together AI

```toml
[model.together-mixtral]
model = "mistralai/Mixtral-8x7B-Instruct-v0.1"
base_url = "https://api.together.xyz/v1"
name = "Mixtral 8x7B"
env_key = "TOGETHER_API_KEY"
```

### Local OpenAI-Compatible Server

Any server that implements the OpenAI Chat Completions or Responses API:

```toml
[model.local-llama]
model = "llama-3.1-70b"
base_url = "http://localhost:8080/v1"
name = "Local Llama"
temperature = 0.8
```

---

## Custom Models Endpoint

Point Grok at a custom OpenAI-compatible `/v1/models` endpoint instead of the default. Use this when your models sit behind a corporate gateway or a self-hosted inference service.

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `GROK_MODELS_BASE_URL` | Yes | Base URL for inference. Grok fetches the model list from `{base_url}/models`. |
| `XAI_API_KEY` | Yes | API key sent as `Authorization: Bearer`. Grok also accepts `GROK_CODE_XAI_API_KEY`. |
| `GROK_MODELS_LIST_URL` | No | Override the model-list URL when it differs from `{base_url}/models`. |

### Setup

```bash
export GROK_MODELS_BASE_URL="https://api.acme.com/v1"
export XAI_API_KEY="xai-..."
open-grok
```

### Config File Alternative

```toml
[endpoints]
models_base_url = "https://api.acme.com/v1"

# Override only the API key for a specific model
[model.grok-build]
api_key = "my-api-key"
```

When you use `[endpoints]` with partial model overrides, Grok inherits the `base_url` from the endpoints config, so you do not need to specify it in each `[model.*]` section.

### Auth Behavior

When you set `models_base_url`, Grok uses API key auth (`Authorization: Bearer`) instead of session auth. You do not need `open-grok login` -- the API key is enough.

---

## Web Search Model

The `web_search` tool uses a separate model. Configure it with:

```toml
[models]
web_search = "grok-4.5"
```

Or via environment variable:

```bash
export GROK_WEB_SEARCH_MODEL="grok-4.5"
```

If you point web search at a custom model, you also need a `[model.*]` entry so Grok can reach it. Server-side ("backend") web search runs only when the model sets `supports_backend_search = true` (and the build enables backend search); it does not depend on `api_backend`:

```toml
[models]
web_search = "my-custom-model"

[model.my-custom-model]
model = "my-custom-model"
supports_backend_search = true
```

---

## Using Custom Models

```bash
# List available models (including custom)
open-grok models

# Use in the TUI via slash command
/model my-model

# Use in headless mode
open-grok -p "Hello" -m my-model

# Set as default in config.toml:
[models]
default = "my-model"
```

---

## Enterprise Deployment

A complete config for an enterprise deployment with custom models:

```toml
[cli]
auto_update = false

[auth]
auth_provider_command = "/usr/local/bin/my-company-auth-provider"
auth_provider_label = "Acme Corp"
auth_token_ttl = 3600

[models]
default = "company-grok"

[model.company-grok]
model = "grok-build"
base_url = "https://grok-proxy.acme.com/"
name = "Grok Build Latest (Proxy)"
context_window = 128000

[features]
telemetry = false
```

---

## Troubleshooting

### Model Not Found

```bash
# List available models
open-grok models

# Check config.toml for typos in [model.*] sections
```

### Connection Errors

Verify the endpoint is reachable:

```bash
curl -s https://api.example.com/v1/models \
  -H "Authorization: Bearer $XAI_API_KEY"
```

### Debug Logging

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/open-grok.log open-grok
tail -f /tmp/open-grok.log
```

Look for log entries containing `model` or `sampling` to trace model selection and API calls.
