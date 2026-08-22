# Authentication

Grok supports several authentication methods, including interactive browser login, enterprise single sign-on (SSO), and headless CI/CD runners.

---

## Browser Login (Default)

On first launch, Grok opens your browser to authenticate with grok.com:

```bash
open-grok
```

Grok stores credentials in `~/.opengrok/auth.json` and reuses them across sessions. Grok refreshes access tokens automatically in the background. When a token can't be refreshed, Grok prompts you to sign in again. Credentials without a server-provided expiry fall back to a 30-day lifetime.

### Credential storage

Tokens in `~/.opengrok/auth.json` (and MCP OAuth tokens in `~/.opengrok/mcp_credentials.json`) are written with owner-only permissions (`0600` on Unix). Anyone with filesystem access to those paths can use the credentials, so:

- Prefer full-disk encryption (FileVault, BitLocker, LUKS, or equivalent).
- Do not copy `auth.json` or `mcp_credentials.json` into shared directories, tickets, or chat.
- On multi-user hosts, keep `$HOME` / `$OPENGROK_HOME` private to your account.

### Re-authenticate

To switch accounts or resolve an authentication problem, run:

```bash
open-grok login
```

Running `open-grok login` starts the sign-in flow again, replacing your cached session. By default, it opens your browser and signs in through SpaceXAI OAuth at `auth.x.ai`. Pass a flag to select a different flow:

| Flag | Description |
|------|-------------|
| `--oauth` | Sign in through SpaceXAI OAuth at `auth.x.ai`. This is the default, so the flag is optional. |
| `--device-auth` (alias `--device-code`) | Sign in with the device-code flow for headless or remote environments. |

To sign out of xAI, run `open-grok logout`. It clears only the xAI credentials in
`~/.opengrok/auth.json`.

### OpenAI Codex OAuth

Grok Build can also connect a ChatGPT account for OpenAI Codex models and
quota usage. This is a second, independent account: connecting or
disconnecting Codex never replaces the xAI login used by the Grok shell.

```bash
open-grok login --codex
open-grok login --codex --device-auth   # headless or remote environments
open-grok logout --codex
```

Inside an active session, use `/login codex` and `/logout codex` for the same
independent account. Bare `/login` and `/logout` continue to operate on xAI.

The Codex flow follows the OpenAI Codex OAuth authorization-code + PKCE setup,
refreshes access tokens automatically, and stores the resulting tokens in
`~/.opengrok/codex-auth.json` with owner-only permissions. Keeping that file
separate from `~/.opengrok/auth.json` prevents a Codex refresh, logout, or usage
failure from changing xAI authentication or paywall state.

Run `/usage` to view xAI billing and OpenAI Codex quota windows together. If
Codex is not connected, the OpenAI section says so and points to
`open-grok login --codex`; xAI usage still loads independently.

---

## API Key

For CI/CD, automation, or environments without browser access, use an API key from [console.x.ai](https://console.x.ai):

```bash
export XAI_API_KEY="xai-..."
open-grok
```

Grok uses the API key as a fallback when no session token is active. If you have already signed in interactively, the stored session token takes precedence. To fall back to the API key, run `open-grok logout` or delete `~/.opengrok/auth.json`.

### Wafer AI

Wafer AI uses an isolated API key with the OpenAI-compatible Chat Completions
API. It does not use Open Grok's xAI or Codex login flows. Set the key before
starting Open Grok:

```bash
export WAFER_API_KEY="wafer-..."
open-grok
```

Wafer uses `https://pass.wafer.ai/v1` and discovers available models from
`GET /v1/models`; model IDs are not hardcoded by this guide. Wafer supports
standard client function tools but does not provide native hosted web search.
Its key and model catalog remain isolated from other providers and Wafer
sessions cannot export data to xAI-only services.

### Z AI

Z AI (GLM models) uses an isolated API key with the OpenAI-compatible Chat
Completions API. It does not use Open Grok's xAI or Codex login flows. Set
the key before starting Open Grok, or connect it in a session with
`/login zai` (Settings → Models → Z AI API key):

```bash
export ZAI_API_KEY="zai-..."
open-grok
```

The default base URL is Z AI's GLM Coding Plan endpoint
`https://api.z.ai/api/coding/paas/v4`; override it with
`OPENGROK_ZAI_API_BASE_URL` (for example, the standard
`https://api.z.ai/api/paas/v4`). Models are discovered from `GET /models`
with a curated GLM fallback when the endpoint is unavailable. Reasoning GLM
models expose low/medium/high/max efforts; requesting any effort sends Z
AI's `thinking` mode switch automatically. Z AI supports standard client
function tools but no native hosted web search. Its key and model catalog
remain isolated from other providers and Z AI sessions cannot export data
to xAI-only services.

### RunInfra

RunInfra uses an isolated API key with the OpenAI-compatible Chat Completions
API. It does not use Open Grok's xAI or Codex login flows. Set the key before
starting Open Grok, or connect it in a session with `/login runinfra`
(Settings → Models → RunInfra API key):

```bash
export RUNINFRA_GATEWAY_KEY="rp_..."
open-grok
```

`RUNINFRA_API_KEY` is accepted as an alias. The default base URL is
`https://api.runinfra.ai/v1`; override it with
`OPENGROK_RUNINFRA_API_BASE_URL`. Models are discovered from `GET /v1/models`
with a curated hosted fallback when the endpoint is unavailable. Known hosted
models reason by default; `deepseek-v4-flash` defaults to max effort.
RunInfra supports standard client function tools but no native hosted web
search. Its key and model catalog remain isolated from other providers and
RunInfra sessions cannot export data to xAI-only services.

### Google Gemini

Google Gemini (AI Studio) uses an isolated API key with the OpenAI-compatible
Chat Completions API. It does not use Open Grok's xAI or Codex login flows. Set
the key before starting Open Grok, or connect it in a session with
`/login gemini` (Settings → Models → Google Gemini API key):

```bash
export GEMINI_API_KEY="..."
open-grok
```

`GOOGLE_API_KEY` is accepted as an alias. The default base URL is
`https://generativelanguage.googleapis.com/v1beta/openai/`; override it with
`OPENGROK_GEMINI_API_BASE_URL`. Trusted hosts are HTTPS
`generativelanguage.googleapis.com` only. The curated models are
`gemini-3.7-flash`, `gemini-3.6-flash`, `gemini-3.5-flash-lite`, and
`gemini-3.1-pro-preview` (catalog keys `gemini:{id}`); live `/models` enrich
those entries only. Gemini 3 cannot use reasoning effort `none`.
`gemini-3.7-flash` and `gemini-3.1-pro-preview` reject `minimal`
(low/medium/high); `gemini-3.6-flash` and `gemini-3.5-flash-lite` offer
minimal/low/medium/high. Defaults: Medium, Medium, Minimal, and High
respectively. Google Gemini supports standard client function tools but no
native hosted web search, Responses API, or OAuth. Its key and model catalog
remain isolated from other providers and Gemini sessions cannot export data to
xAI-only services.

### OpenRouter

OpenRouter uses an isolated API key with the OpenAI-compatible Chat
Completions API. It does not use Open Grok's xAI or Codex login flows. Set
the key before starting Open Grok, or connect it in a session with
`/login openrouter` (Settings → Models → OpenRouter API key):

```bash
export OPENROUTER_API_KEY="sk-or-..."
open-grok
```

The default base URL is `https://openrouter.ai/api/v1`; override it with
`OPENGROK_OPENROUTER_API_BASE_URL`. Stored keys are sent only to
`https://openrouter.ai`. Open Grok queries `GET /models?output_modalities=all`
and adds every text-output model to the picker. Image and embedding-only
models are omitted. An empty Settings allowlist keeps the full live catalog;
narrow it from **Settings → Models → OpenRouter models**. Reasoning menus use
each model's live `supported_efforts` list; models that omit that field have
no effort selector. OpenRouter supports standard client function tools but no
native hosted web search, Responses API, or OAuth. Its key and model catalog
remain isolated from other providers and OpenRouter sessions cannot export
data to xAI-only services.

---

## OIDC (Customer SSO)

Authenticate developers through your own Identity Provider (IdP) -- such as Okta, Azure AD, or Auth0 -- instead of grok.com.

### 1. Register a public client in your IdP

- Grant type: Authorization Code with PKCE (Proof Key for Code Exchange)
- Redirect URI: `http://127.0.0.1/callback` -- a loopback address. Grok binds a random port at sign-in time, and most IdPs treat the loopback redirect as port-agnostic per [RFC 8252](https://tools.ietf.org/html/rfc8252).
- No client secret. PKCE replaces it.

### 2. Configure the CLI

Via config file:

```toml
# ~/.opengrok/config.toml
[grok_com_config.oidc]
issuer = "https://acme.okta.com"
client_id = "0oa1b2c3d4e5f6g7h8i9"
```

Or via environment variables:

```bash
export GROK_OIDC_ISSUER="https://acme.okta.com"
export GROK_OIDC_CLIENT_ID="0oa1b2c3d4e5f6g7h8i9"
```

You can also override the API endpoint to point at your own proxy:

```bash
export GROK_CLI_CHAT_PROXY_BASE_URL="https://grok-proxy.acme.com/v1"
```

### 3. Run `open-grok`

The CLI discovers endpoints via `{issuer}/.well-known/openid-configuration`, opens the IdP login page, and stores tokens in `~/.opengrok/auth.json`. Tokens auto-refresh silently via the stored `refresh_token`.

### Optional fields

| Field | Default | Notes |
|-------|---------|-------|
| `scopes` | `["openid", "profile", "email", "offline_access", "api:access"]` | `offline_access` enables silent token refresh |
| `audience` | None | Required by some IdPs (e.g., Auth0) |

---

## External Auth Provider

When browser-based login isn't possible -- for example, on sandboxed VMs, CI runners, or air-gapped networks -- delegate authentication to an external binary or script.

### How It Works

```
+--------------+     sh -c     +------------------------+
|     Grok     |-------------->|  your auth binary      |
|              |               |                        |
|  reads       |<-- stdout ----|  prints token          |
|  auth.json   |               |                        |
|              |   (stderr)    |  prints status/URLs    |--> surfaced to user
+--------------+               +------------------------+
```

1. Grok runs your command via `sh -c "<command>"`
2. Your binary runs whatever auth flow it needs (SSO, device code, certificate exchange)
3. **stderr** carries human-readable output, such as login URLs and status messages. Grok reads stderr and surfaces it to the user; in the TUI, it turns the first `https://` URL into a clickable sign-in link.
4. **stdout** is captured by Grok and saved as the access token
5. Exit 0 = success; exit non-zero = Grok falls back to interactive login

### The stdout / stderr Contract

| Stream | What to print | Who sees it |
|--------|---------------|-------------|
| **stdout** | The token -- nothing else | Grok (parsed and stored in auth.json) |
| **stderr** | Login URLs, status messages, errors | The user (Grok reads stderr and shows the sign-in URL as a clickable link in the TUI) |

**Do not print anything to stdout except the token.** No progress messages, no debug output. Grok reads stdout, trims surrounding whitespace, and parses the result as a token.

### stdout Token Format

**Bare string** -- just the raw token:

```
eyJhbGciOiJSUzI1NiIs...
```

**JSON** -- with optional refresh token and expiry:

```json
{"access_token": "eyJhbGciOi...", "refresh_token": "ref-tok", "expires_in": 3600}
```

Use JSON if your tokens expire and you want Grok to automatically re-run the binary before expiry.

### Configuration

Via config file:

```toml
# ~/.opengrok/config.toml
[auth]
auth_provider_command = "/usr/local/bin/my-auth-provider"
auth_provider_label = "Acme Corp"   # optional -- customizes the TUI login button
auth_token_ttl = 3600               # optional -- token lifetime in seconds
```

Or via environment variables:

```bash
export GROK_AUTH_PROVIDER_COMMAND="/usr/local/bin/my-auth-provider"
export GROK_AUTH_PROVIDER_LABEL="Acme Corp"
export GROK_AUTH_TOKEN_TTL=3600
```

### Token Refresh

Grok runs your binary on two different contracts, and `GROK_AUTH_EXPIRED` is how
it tells them apart. Each run fully replaces the stored credential, so emit the
same JSON fields (such as `issuer`) on every invocation, including refreshes.

- **`GROK_AUTH_EXPIRED=1` — a headless refresh.** Grok is re-minting over a
  credential it already holds: a near-expiry rotation, or a token the server
  rejected. Nobody is watching. stdin is closed, your stderr is swallowed, and
  the binary is given a few seconds before it is killed. Mint silently or exit
  non-zero — never block.
- **Unset — a sign-in.** `open-grok login`, the sign-in screen, or the
  escalation Grok performs when a headless run couldn't mint. A user is
  waiting, your stderr reaches them, and you have 300 seconds — enough for a
  browser round trip or a device code.

```bash
#!/bin/sh
if [ "$GROK_AUTH_EXPIRED" = "1" ]; then
    # Headless: silent refresh only. Declining is the fast, correct answer
    # when your SSO session has lapsed and only the user can renew it.
    echo "Refreshing token..." >&2
    TOKEN=$(my-company-auth --refresh --silent) || exit 1
else
    echo "Authenticating via Acme Corp SSO..." >&2
    TOKEN=$(my-company-auth --login --interactive)
fi

if [ -z "$TOKEN" ]; then
    echo "Authentication failed" >&2
    exit 1
fi

echo "{\"access_token\": \"$TOKEN\", \"expires_in\": 3600}"
```

When the headless run can't produce a token, Grok stops treating the stored
credential as usable and starts the sign-in flow instead — the same one you get
on a machine that has never signed in, with your binary's stderr shown, so a
device-code URL or a browser prompt reaches you. Exiting promptly on
`GROK_AUTH_EXPIRED=1` is what makes that handover fast; a binary that blocks
instead makes you wait out the refresh timeout on every start. Mid-session, the
turn fails with a re-auth prompt and `/login` re-runs the binary interactively.

One case stays ambiguous, and only in **leader mode** (`--leader`, or
`[cli] use_leader = true`; off by default): with no credential at all, the
leader makes one extra attempt in the background just after startup, and that
run has the variable unset, like a sign-in. A binary that mints without help
(service account, keytab, mounted token) succeeds there and the session heals
itself. One that must prompt just sits, up to the 300s sign-in ceiling —
nothing waits on it, the sign-in screen is already up, and that run's stderr
goes to `~/.opengrok/leader.log` rather than to you.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `GROK_AUTH_PROVIDER_COMMAND` | Path to your auth binary |
| `GROK_AUTH_PROVIDER_LABEL` | Display name on the TUI login screen (e.g., "Acme Corp") |
| `GROK_AUTH_TOKEN_TTL` | Token lifetime in seconds (for bare-string tokens without `expires_in`) |
| `GROK_AUTH_EXPIRED` | Set to `1` on a headless refresh: don't prompt, and don't hand back a cached token. Unset on a sign-in, where a user is attached |
| `GROK_AUTH_EARLY_INVALIDATION_SECS` | Seconds before expiry to proactively refresh (default: 300) |

---

## Device Code Flow

For headless environments (SSH sessions, Docker containers, remote VMs) where no browser is available locally:

```bash
open-grok login --device-auth    # or: open-grok login --device-code
```

This prints a URL and code to the terminal. Open the URL on any device, enter the code, and complete authentication. Grok polls until the login is confirmed.

You can also implement the device-code flow through an [External Auth Provider](#external-auth-provider) for full control.

---

## Automatic Credential Refresh

Grok automatically refreshes expired credentials:

- **Before expiry:** If your auth provider returned `expires_in` (JSON output) or you set `auth_token_ttl`, Grok re-runs the auth binary ~5 minutes before expiry.
- **On auth error:** If the server returns 401 Unauthorized, Grok refreshes the credentials and retries the request.
- **OIDC:** If a `refresh_token` is available, Grok silently refreshes via your IdP without re-opening the browser.

Tune the refresh buffer:

```bash
# Refresh 5 minutes before expiry (default)
export GROK_AUTH_EARLY_INVALIDATION_SECS=300

# Disable the proactive buffer: refresh at expiry or on a 401 (set to 0)
export GROK_AUTH_EARLY_INVALIDATION_SECS=0
```

---

## Hot Reload

Grok picks up changes to `~/.opengrok/auth.json` automatically. If you update credentials externally (for example, with a script that writes new tokens), Grok uses the new credentials on the next API call without a restart.

---

## Auth Precedence

Grok resolves credentials for each request in this order, highest to lowest:

1. **Per-model `api_key` or `env_key`** -- set under `[model.<name>]` in `config.toml`. Wins whenever present.
2. **Active session token** -- obtained through browser, OIDC/OAuth2, or external-provider login and stored in `~/.opengrok/auth.json`.
3. **`XAI_API_KEY`** -- fallback when no session token is active.

When more than one login flow is configured, Grok populates the session token from the first available source, highest to lowest:

1. **External auth provider** (`auth_provider_command`)
2. **Enterprise OIDC** -- when OIDC is configured, through `[grok_com_config.oidc]` in `config.toml` or the `GROK_OIDC_ISSUER` and `GROK_OIDC_CLIENT_ID` environment variables
3. **SpaceXAI OAuth2 browser login** -- the default

During a session, the active method handles all mid-session refreshes.

---

## Related settings

Coding-data sharing — **Coding data, retention, and training** in Settings,
which `/privacy` opens — does not change these config knobs:

| Setting | How to set it |
|---------|---------------|
| `[features] telemetry` | `config.toml` or `GROK_TELEMETRY_ENABLED` |
| `[telemetry] trace_upload` | `config.toml` or `GROK_TELEMETRY_TRACE_UPLOAD` |
| External OpenTelemetry | `GROK_EXTERNAL_OTEL` / `[telemetry] otel_*`. See [Monitoring Usage](24-monitoring-usage.md). |

Organization policy can lock coding-data sharing. When Zero Data Retention
(ZDR) is active, the setting cannot be changed and the row shows `ZDR` in
place of the value. Other policy locks are shown as `Policy Managed`.

See [Monitoring Usage](24-monitoring-usage.md#related-settings) and [Configuration](05-configuration.md#telemetry).

---

## Troubleshooting

### Debug logging

Set `RUST_LOG` to control the verbosity of the file log and headless stderr output. (The TUI's on-screen tracing pane uses a fixed filter and ignores `RUST_LOG`.) In the TUI, file logging defaults to `DEBUG`; in headless mode (`-p`), `RUST_LOG` defaults to `off` so only the answer is printed — set `RUST_LOG=error` (or broader) to see logs on stderr.

In the TUI, set `GROK_LOG_FILE` to an absolute path to write logs to that file:

```bash
GROK_LOG_FILE=/tmp/open-grok.log RUST_LOG=debug open-grok
tail -f /tmp/open-grok.log
```

`GROK_LOG_FILE` is treated as a literal file path. A relative value such as `1` writes a file named `1` in the current directory.

In headless mode, logs go to stderr. Redirect them to a file:

```bash
RUST_LOG=debug open-grok -p "hello" 2> /tmp/open-grok.log
```

### Common log messages

| Log message | What it means |
|-------------|---------------|
| `auth: running external auth provider (headless refresh)` / `(interactive login)` | Grok is running your binary, and on which contract |
| `auth: external auth provider returned fresh token` | Grok parsed and stored the token |
| `auth: external auth provider failed` | Binary exited non-zero or stdout was empty |
| `auth: external auth provider timed out (likely needs interactive auth), killing` | Binary did not exit before the timeout and was killed |
| `auth: failed to start external auth provider` | Command could not be spawned (binary not found) |

### Common fixes

- **"Authentication failed"** -- Run `open-grok logout` to clear cached credentials, then `open-grok login` to sign in again.
- **Token expires too quickly** -- Set `auth_token_ttl` or return `expires_in` in your auth provider's JSON output.
- **OIDC redirect fails** -- Ensure your IdP allows loopback redirect URIs (`http://127.0.0.1/callback`).
- **External auth provider not found** -- Check that the `auth_provider_command` path is correct and the binary is executable.
