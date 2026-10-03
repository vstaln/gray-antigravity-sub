# gray-antigravity-sub

> **Antigravity subscription model-provider sidecar plugin for the [gray](https://github.com/vstaln/gray) agent harness.**

`gray-antigravity-sub` drives the official `agy` CLI fully inert (staged HOME, `request-review` skeleton, `--disable-slash-commands`, `--json-schema` funnel — `agy` owns zero tools of gray's) as a request-scoped model provider. Gray owns the agent loop, tools, approvals, and compaction — Antigravity only answers.

## Why this is ToS-safe (no bans)

The Antigravity proxy ecosystem (`antigravity-proxy`, `opencode-antigravity-auth` and forks) extracts the Google OAuth token and replays Google's internal RPC protocols as an OpenAI-compatible proxy, with account rotation and quota dashboards. That pattern violates Google's Terms of Service — the opencode plugin's own README warns users have been **banned or shadow-banned** for it.

This connector does none of that:

* **Zero token handling**: uses your existing `agy` Google sign-in (OS keyring + browser). Token bytes are never opened, copied, logged, or forwarded — auth reaches the child through a staged-HOME symlink to your own `~/.gemini/antigravity-cli` dir.
* **Official CLI only**: every turn is one `agy --model <full-id> --disable-slash-commands --input-format stream-json --output-format stream-json --json-schema … -p ''` subprocess — the same binary Google ships, driven the way its own docs describe.
* **No rotation, no quota tooling, no internal endpoints**: one login, one upstream request per turn (admission relay), quota errors surface as clean `RateLimited` instead of failover schemes.
* **Fail-closed**: gateway/proxy env overrides (`AGY_LLM_GATEWAY_*`, `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, …) refuse to spawn rather than redirect the subscription bearer.

---

## Highlights

* **Zero Token / Credential Leaks**: Uses your existing `agy` sign-in. Never handles, stores, or logs API keys or tokens.
* **Single-Request Admission Relay**: Drives an internal request-scoped loopback admission relay that enforces exactly one upstream request per turn and absorbs redundant recovery attempts.
* **Finish Funnel**: Gray tools are described in the system text and answered through a single `finish(answer, calls[])` JSON-schema tool — `agy` never sees a real tool. A turn with no `finish` call is incomplete, never an answer.
* **Pinned Model Catalog**: Full `agy models` ids (`gemini-3.8-flash-low`, `claude-sonnet-5-5-low`, …) with windows, never guessed (`gpt-oss-120b-medium` reports `None`). Full ids already encode effort, so there is no effort knob.
* **Dual Integration**: Shipped as both a **standalone protocol-1.2 sidecar binary** (`antigravity-sub`) and an **in-process Rust provider library** (`antigravity_sub::direct_provider`).

---

## Requirements

| Requirement | Details |
|---|---|
| **Antigravity CLI** | Official `agy` binary installed and on `$PATH`. |
| **Authentication** | Signed in via `agy` once (Google sign-in) on your Antigravity subscription. |
| **Gray Harness** | [Gray](https://github.com/vstaln/gray) agent framework (Protocol v1.1 or v1.2 sidecar support). |

The plugin validates dependencies at every seam:
* If `agy` is not found, it reports an informative installation hint rather than a process crash.
* If gateway/proxy overrides or conflicting backend variables are set, it fails closed to prevent redirecting subscription authentication.

---

## Installation & Build

### Building from Source

```sh
git clone https://github.com/vstaln/gray-antigravity-sub.git
cd gray-antigravity-sub
cargo build --release
```

The resulting binary is located at `./target/release/antigravity-sub`.

### Installing in Gray

Point Gray to the binary or install it directly into `$GRAY_HOME/plugins` (default `~/.gray/plugins`):

```sh
# Copy binary to your Gray plugin directory
mkdir -p ~/.gray/plugins/antigravity-sub
cp target/release/antigravity-sub ~/.gray/plugins/antigravity-sub/antigravity-sub

# Or install via gray plugin management
gray install plugin antigravity-sub
```

Sign in with Antigravity if you haven't already:

```sh
agy
# complete the Google sign-in once, then exit
```

---

## Model Selection

Once registered, Antigravity subscription models appear under the `antigravity-sub/` prefix:

```sh
# Select model in interactive mode
/model antigravity-sub/flash
/model antigravity-sub/sonnet
/model antigravity-sub/opus
/model antigravity-sub/pro

# Or launch directly with Gray CLI
gray -m antigravity-sub/flash -p "Review this codebase"
```

### Pinned Model Catalog

| Model ID | Target Model | Context Window |
|---|---|---|
| `antigravity-sub/flash` | Gemini 3.8 Flash (Low) | 1,048,576 tokens |
| `antigravity-sub/pro` | Gemini 3.1 Pro (Low) | 1,048,576 tokens |
| `antigravity-sub/sonnet` | Claude Sonnet 5.5 (Low) | 200,000 tokens |
| `antigravity-sub/opus` | Claude Opus 5.5 (Low) | 1,000,000 tokens |

Plus every full `agy models` id verbatim (`gemini-3.8-flash-high`, `claude-opus-5-5-medium`, `gpt-oss-120b-medium`, …). Each effort tier is a separate model — there is no effort knob.

---

## Wire Protocol & Architecture

```
┌──────────────────┐               ┌──────────────────┐               ┌───────────────────┐
│                  │  stdio NDJSON │                  │  stdin/stdout │                   │
│   Gray Harness   │ ────────────> │ gray-antigravity │ ────────────> │  agy CLI          │
│   (owns tools &  │ <──────────── │ (admission relay │ <──────────── │  (inert funnel,   │
│   approvals)     │ Protocol 1.2  │ & funnel encoder)│  stream-json  │   finish only)    │
└──────────────────┘               └──────────────────┘               └───────────────────┘
```

The sidecar communicates over standard I/O using newline-delimited JSON (NDJSON):
* `plugin/manifest`: Declares provider capabilities and supported model IDs.
* `provider/models`: Returns the pinned catalog.
* `provider/chat`: Parks the request intent and initiates a relayed turn.
* `provider/auth/*`: Reports external CLI authentication state.

---

## Repository Structure

```text
├── src/
│   ├── main.rs              # Protocol-1.2 sidecar entry point
│   ├── chat.rs              # Funnel translation, streaming parser, and CLI runner
│   ├── relay.rs             # Single-admission loopback relay proxy
│   ├── catalog.rs           # Pinned model catalog definitions
│   ├── models.rs            # Provider model metadata
│   ├── manifest.rs          # Plugin protocol manifest
│   ├── setup.rs             # CLI dependency discovery and verification
│   └── direct_provider.rs   # In-process Provider trait implementation
```

---

## License

MIT License — Copyright (c) 2026 Vstalin Grady
