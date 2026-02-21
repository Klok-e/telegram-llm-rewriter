# brainrot_tg_llm_rewrite

A Telegram **userbot** that intercepts your outgoing messages in configured chats, rewrites them through the OpenAI Responses API, and edits the original message with the rewritten version. The rewrite prompt is fully configurable.

## How It Works

```
You type a message in chat X
        │
        ▼
Userbot detects outgoing message in a monitored chat
        │
        ▼
Original text is sent to OpenAI with the configured system prompt + context
        │
        ▼
Model returns rewritten text
        │
        ▼
Userbot edits the original message with the rewritten version
```

The user sees their message briefly in its original form, then it gets replaced with the rewritten version within ~1-2 seconds (depending on model speed/network).

## Setup

1. Get `api_id` and `api_hash` from https://my.telegram.org
2. Get an OpenAI API key
3. Create `config.toml` in the working directory (see [Config](#config))
4. `cargo run` — on first launch, the bot will prompt for phone number + login code

## Config

`config.toml` in the working directory:

```toml
[telegram]
api_id = 12345
api_hash = "your_api_hash"
session_file = "session.bin"

[openai]
api_key = "sk-..."
model = "gpt-4.1-mini"
timeout_seconds = 20

[rewrite]
# Chat IDs to monitor (negative for groups/supergroups).
chats = [-1001234567890]

# The system prompt that controls the rewrite style.
system_prompt = """
You are a message rewriter. Rewrite the following message in an excessively verbose,
formal, and grandiose style. Treat even the most mundane statements as matters of
great importance. Preserve the original meaning but make it sound like a royal decree
or academic paper. Reply with ONLY the rewritten message, nothing else.
"""
```

`api_id` and `api_hash` are obtained from https://my.telegram.org.

For `--list-chats` mode, only the `[telegram]` section is required.

## CLI

```text
brainrot_tg_llm_rewrite [--config <path>] [--list-chats [query]]
```

- `--config <path>`: override config path (default `config.toml`)
- `--list-chats [query]`: list visible chats as `<id>\t<name>`, optionally filtered by case-insensitive name contains

## Hot-Reload

The bot watches `config.toml` for changes at runtime using the `notify` crate. When the file is modified, the bot re-parses it and applies hot-reloadable fields without restarting.

### Hot-Reloadable Fields (no restart needed)

| Field | Section |
|-------|---------|
| `system_prompt` | `[rewrite]` |
| `chats` | `[rewrite]` |
| `context_messages` | `[rewrite]` |
| `model` | `[openai]` |
| `api_key` | `[openai]` |

### Restart-Required Fields

| Field | Section | Why |
|-------|---------|-----|
| `api_id` | `[telegram]` | Bound to the Telegram connection at startup |
| `api_hash` | `[telegram]` | Bound to the Telegram connection at startup |
| `session_file` | `[telegram]` | Session is opened once at startup |
| `timeout_seconds` | `[openai]` | Baked into the HTTP client at construction |
