# brainrot_tg_llm_rewrite — Design Document

## Overview

A Telegram **userbot** (runs as your personal account) that intercepts your outgoing messages in configured chats, rewrites them through a local LLM (Ollama), and edits the original message with the rewritten version. Default style: verbose and formal. The rewrite prompt is fully configurable.

## How It Works

```
You type a message in chat X
        │
        ▼
Userbot detects outgoing message in a monitored chat
        │
        ▼
Original text is sent to Ollama with the configured system prompt
        │
        ▼
LLM returns rewritten text
        │
        ▼
Userbot edits the original message with the rewritten version
```

The user sees their message briefly in its original form, then it gets replaced with the rewritten version within ~1-2 seconds (depending on LLM speed).

## Architecture

### Components

1. **Telegram Client** — MTProto userbot using the `grammers` crate (pure Rust, no C dependencies unlike TDLib)
2. **LLM Client** — HTTP client talking to a local Ollama instance
3. **Config** — TOML file for chat IDs, Ollama endpoint, model name, and system prompt

### Crate Dependencies

| Crate | Purpose |
|-------|---------|
| `grammers-client` + `grammers-session` | Telegram MTProto client & session persistence |
| `reqwest` | HTTP client for Ollama API |
| `serde` / `serde_json` | JSON (de)serialization for Ollama API |
| `toml` | Config file parsing |
| `tokio` | Async runtime |
| `tracing` + `tracing-subscriber` | Logging |

### Project Structure

```
src/
├── main.rs          # Entry point, CLI auth flow, event loop, orchestration
├── config.rs        # Config loading & types
├── telegram.rs      # Telegram client wrapper (connect, listen, edit)
└── llm.rs           # Ollama API client
```

## Config

`config.toml` in the working directory:

```toml
[telegram]
api_id = 12345
api_hash = "your_api_hash"
session_file = "session.bin"

[ollama]
url = "http://localhost:11434"
model = "llama3"

[rewrite]
# Chat IDs to monitor (negative for groups/supergroups).
chat_ids = [-1001234567890]

# The system prompt that controls the rewrite style.
system_prompt = """
You are a message rewriter. Rewrite the following message in an excessively verbose,
formal, and grandiose style. Treat even the most mundane statements as matters of
great importance. Preserve the original meaning but make it sound like a royal decree
or academic paper. Reply with ONLY the rewritten message, nothing else.
"""
```

`api_id` and `api_hash` are obtained from https://my.telegram.org.

## Implementation Plan

### Phase 1: Project skeleton & config
- Define `Config` struct with serde deserialization
- Load and validate `config.toml`
- Add all dependencies to `Cargo.toml`
- Set up `tracing` logging

### Phase 2: Ollama client
- Implement `POST /api/generate` call to Ollama
- Send system prompt + user message, return generated text
- Handle errors and timeouts gracefully

### Phase 3: Telegram userbot
- Implement first-run interactive login (phone number + code + optional 2FA)
- Persist session to file so login is one-time
- Connect and listen for **outgoing** message updates
- Filter to only monitored chats (by ID or resolved username)

### Phase 4: Message rewrite loop
- On outgoing message in a monitored chat:
  1. Extract message text (skip media-only messages)
  2. Send to Ollama for rewriting
  3. Edit the original message with the rewritten text
- Log original → rewritten for debugging

### Phase 5: Polish
- Graceful shutdown on Ctrl+C
- Retry logic for Ollama (connection refused, model loading)
- Skip editing if the rewrite is identical or empty
- Optionally prefix rewritten messages (e.g., a subtle marker) so you know which ones were rewritten

## Key Design Decisions

1. **`grammers` over TDLib** — Pure Rust, no need to compile/link TDLib's C++ code. Lighter, easier to build and cross-compile.

2. **Edit-in-place, not delete+resend** — Editing preserves message ordering, reply chains, and doesn't trigger extra notifications. Downside: there's a brief window where the original text is visible.

3. **Ollama over cloud APIs** — No API keys, no costs, full privacy. Runs locally. The user can swap models freely.

4. **TOML config over CLI args** — The system prompt and chat list are too complex for CLI flags. A config file is more ergonomic.

## Outgoing Message Detection (Verified)

Detecting outgoing messages with `grammers` is confirmed to work. Evidence:

- **Protocol level:** Telegram's MTProto pushes updates for messages you send (this powers multi-device sync). The `Message` constructor carries an `out` flag.
- **`grammers` internals:** The session adaptor layer (`grammers-session/src/message_box/adaptor.rs`) explicitly preserves the `out` flag when converting `UpdateShortMessage` → `UpdateNewMessage`.
- **Public API:** `Message::outgoing()` exposes the flag directly:
  ```rust
  pub fn outgoing(&self) -> bool {
      match &self.raw {
          tl::enums::Message::Message(message) => message.out,
          // ...
      }
  }
  ```
- **Official example confirms it:** The `echo.rs` example guards with `!message.outgoing()` to skip outgoing messages — proving they arrive as updates.

### Usage pattern

```rust
match update {
    Update::NewMessage(message) if message.outgoing() => {
        if message.chat().id() == target_chat_id {
            // rewrite this message
        }
    }
    _ => {}
}
```

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| Telegram rate limits on edits | Add a small delay if needed; in practice, normal chatting won't hit limits |
| Ollama latency on slow hardware | User can pick a smaller/faster model; message stays as-is until rewrite completes |
| Session invalidation | Detect auth errors and prompt re-login |
