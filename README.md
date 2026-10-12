<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
  <img src="assets/googlegemini.svg" alt="googlegemini" width="96">
</p>
<h1 align="center">gray-antigravity-sub</h1>
<p align="center">Use your Antigravity subscription as a model provider — the official `agy` CLI, driven fully inert.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-antigravity-sub/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

`gray-antigravity-sub` drives the official `agy` CLI fully inert (staged HOME, `request-review` skeleton, `--disable-slash-commands`, `--json-schema` funnel — `agy` owns zero tools of gray's) as a request-scoped model provider. Gray owns the agent loop, tools, approvals, and compaction — Antigravity only answers.

## How the plugin uses your login

* **No token reads by the plugin**: it never reads, copies, logs, or forwards agy's OAuth token. It checks that agy's token file exists and symlinks it into a staged HOME, so the official `agy` CLI can sign in with your own login.
* **Official CLI only**: every turn is one `agy --model <full-id> --disable-slash-commands --input-format stream-json --output-format stream-json --json-schema … -p ''` subprocess, the same binary Google ships, driven the way its own docs describe.
* **No internal endpoints, no quota API, no account rotation**: the plugin makes no direct calls to Google's services. The usage card shows a quota row only after agy reports a quota wall in its output.
* **Fail-closed**: gateway/proxy env overrides (`AGY_LLM_GATEWAY_*`, `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, …) refuse to spawn rather than redirect the subscription bearer.

---

## Highlights

* **No Token Handling**: Uses your existing `agy` sign-in. The plugin never reads, stores, or logs API keys or tokens.
* **Single-Request Admission Relay**: Drives an internal request-scoped loopback admission relay that enforces exactly one provider request per turn and absorbs redundant recovery attempts.
* **Finish Funnel**: Gray tools are described in the system text and answered through a single `finish(answer, calls[])` JSON-schema tool — `agy` never sees a real tool. A turn with no `finish` call is incomplete, never an answer.
* **Pinned Model Catalog**: Full `agy models` ids (`gemini-3.8-flash-low`, `claude-sonnet-5-5-low`, …) with windows, never guessed (`gpt-oss-120b-medium` reports `None`). Full ids already encode effort, so there is no effort knob.

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

Register the binary with Gray, either by pointing `GRAY_PLUGIN_PATH` at the executable or by putting it on `PATH` as `gray-antigravity-sub`:

```sh
GRAY_PLUGIN_PATH="$PWD/target/release/antigravity-sub" gray install plugin antigravity-sub

# or
cp target/release/antigravity-sub ~/.local/bin/gray-antigravity-sub
gray install plugin antigravity-sub
```

Run the install from an interactive terminal and approve the `provider.credentials` capability.

Sign in with Antigravity if you haven't already:

```sh
agy
# complete the Google sign-in once, then exit
```

---

## Model Selection

Once registered, run `/connect` in Gray and pick **Antigravity subscription**. Then choose a model:

```sh
# Select model in interactive mode
/model antigravity-sub/flash
/model antigravity-sub/sonnet
/model antigravity-sub/opus
/model antigravity-sub/pro

# Or launch directly with Gray CLI (after /connect)
gray --model antigravity-sub/flash -p "Review this codebase"
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
```

---

## License

MIT License — Copyright (c) 2026 Vstalin Grady

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
