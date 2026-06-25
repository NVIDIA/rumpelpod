# LLM Response Cache

This directory contains cached LLM API responses used by integration tests for
`rumpel claude`, `rumpel codex`, and `rumpel grok`.

## Purpose

The cache enables **cheap and deterministic** LLM API calls in tests while keeping
them **realistic** (actual API responses, not hand-crafted mocks).

## How it works

The cache is served by an HTTP proxy on the host-side git server. Requests from
inside the pod are routed to the proxy via the existing exec tunnel. Each
request is hashed (method, path, headers, body) to produce a cache key; on hit
the stored response is replayed, on miss the request is forwarded upstream and
the response is stored.

The cache key excludes the API key so cached responses work regardless of
whether `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `XAI_API_KEY` is set.

## Workflow for tests

1. **First run (populate cache):** Run tests with the relevant API key set.
   Cache misses will hit the real API and store responses.
2. **Check in cache files:** Commit new/modified files in `llm-cache/` to the repo.
3. **Subsequent runs:** Tests use cached responses. No API key needed.

Do not hand-edit, rename, or synthesize cache response files. If a response is
wrong or its key changes, regenerate it by running the affected test with
`RUMPELPOD_TEST_LLM_OFFLINE=0` and a live API key, then commit the cache files
that the proxy writes.

## Detecting problems

If **new files appear** in `llm-cache/` on every test run, something is wrong:
- Non-deterministic request content (timestamps, random IDs, etc.)
- Test logic changed but cache wasn't updated

Run `git status llm-cache/` after tests to check for unexpected new files.

## Directory structure

```
llm-cache/
 README.md         # This file
 claude-cli/       # Cached responses from the Claude CLI proxy
    response/     # Binary: metadata JSON + newline + body
    request/      # Pretty-printed request JSON (gitignored)
 codex/            # Cached responses from the Codex CLI proxy
    response/
    request/
 grok/             # Cached responses from the Grok CLI proxy
    response/
    request/
```

## Implementation

See [`src/llm/cache_proxy.rs`](../rumpelpod/src/llm/cache_proxy.rs).
