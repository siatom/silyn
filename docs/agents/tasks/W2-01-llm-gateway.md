---
id: W2-01
wave: 2
depends_on: [W1-01]
owns:
  - crates/silyn-llm/src/**
  - crates/silyn-llm/tests/**
reads:
  - crates/silyn-core/src/contracts.rs
  - schemas/run_event.schema.json
forbids:
  - crates/silyn-core/**
  - eval/suites/**
  - Cargo.toml
contracts: [ModelProvider v1, ModelRequest v1, ModelResponse v1]
acceptance_registry:
  - llm.openai_compatible_request_shape
  - llm.structured_output_fallback
  - llm.rate_limit_is_transient
commands:
  - cargo test -p silyn-llm
  - cargo clippy -p silyn-llm --all-targets -- -D warnings
---

# W2-01 OpenAI-compatible model gateway

## Goal

Implement `ModelProvider` for OpenAI-compatible endpoints, plus a deterministic
`MockProvider` and cassette replay for offline tests.

## Requirements

- Config: `base_url`, `api_key_env`, `model`, timeouts, retries, price table.
  Never read a hardcoded key.
- Probe and record: tools, JSON schema, parallel tool calls, stream, seed.
- Prefer `response_format=json_schema`; fall back to JSON instruction plus one
  repair attempt; then return `Permanent`.
- Map HTTP 429/5xx/timeouts to `Transient`; malformed output to `Permanent`.
- Record the model id reported by the provider and token usage.
- Redact credentials and prompt text in logs.

## Non-goals

- Streaming UI, embeddings, vendor-specific branches.

## Acceptance

- Request shape matches the OpenAI-compatible schema.
- 429 retries with backoff, 400 does not.
- Invalid JSON triggers exactly one repair attempt.
- Mock provider makes tests fully offline.
