# WarpOCA Proxy

Local Warp multi-agent translation proxy.

It accepts Warp's protobuf `warp_multi_agent_api::Request` body and streams back
SSE events whose `data:` field is a base64 URL-safe encoded
`warp_multi_agent_api::ResponseEvent`.

## Run

For team installs, the proxy defaults to Oracle Code Assist/Codex Enterprise:

- base URL: `https://code-internal.aiservice.us-chicago-1.oci.oraclecloud.com/20250206/app/litellm`
- wire API: OpenAI Responses API
- default model: `gpt-5.5`

The default team path is Codex CLI auth. Run `codex login` once and the proxy
will read the existing key from `~/.codex/auth.json`. WarpOCA does not prompt
users to paste an Oracle Code Assist API key.

```bash
cargo run -p byob_proxy
```

The repo-level launcher starts the proxy, exports `WARP_CUSTOM_AI_PROXY`, and
then launches the Warp macOS app workflow:

```bash
./script/build_and_run.sh
```

To force a different OpenAI-compatible Chat Completions backend:

```bash
BYOB_WIRE_API=chat_completions \
BYOB_OPENAI_BASE_URL=http://127.0.0.1:8000/v1 \
BYOB_OPENAI_API_KEY=... \
BYOB_MODEL=... \
cargo run -p byob_proxy
```

To force Oracle Code Assist explicitly:

```bash
BYOB_WIRE_API=responses \
BYOB_OPENAI_BASE_URL=https://code-internal.aiservice.us-chicago-1.oci.oraclecloud.com/20250206/app/litellm \
BYOB_MODEL=gpt-5.5 \
cargo run -p byob_proxy
```

## Environment

- `BYOB_PROXY_LISTEN`: bind address, default `127.0.0.1:1337`
- `BYOB_OPENAI_BASE_URL`: OpenAI-compatible base URL, default Oracle Code Assist
- `BYOB_BACKEND_URL`: optional full URL override, bypassing automatic endpoint suffixing
- `BYOB_OPENAI_API_KEY`: optional developer override for the backend bearer token
- `BYOB_MODEL`: optional model override, default `gpt-5.5`
- `BYOB_WIRE_API`: `chat_completions` or `responses`; defaults to `responses`
- `BYOB_EXTRA_HEADERS_JSON`: optional JSON object of extra backend headers
- `BYOB_CONFIG_PATH`: optional override for the WarpOCA JSON config path
- `BYOB_CODEX_CONFIG_PATH`: optional override for Codex provider config path
- `BYOB_CODEX_AUTH_PATH`: optional override for Codex auth path
- `BYOB_SYSTEM_PROMPT`: optional system prompt prefix
- `BYOB_REQUEST_TIMEOUT_SECS`: backend timeout, default `600`

## Tool Mapping

- OpenAI `run_shell_command` function calls become Warp `RunShellCommand` tool calls.
- OpenAI `apply_file_diffs` function calls become Warp `ApplyFileDiffs` tool calls.
- Tool results from Warp are translated back into OpenAI `tool` messages on the next request.

## Supported WarpOCA Endpoints

- `/ai/multi-agent`: Agent Mode protobuf/SSE translation.
- `/ai/passive-suggestions`: passive Agent Mode suggestion translation.
- `/ai/generate_input_suggestions`: Next Command JSON prediction, including local package-manager fallbacks.
- `/ai/generate_am_query_suggestions`: terminal prompt suggestion generation.
- `/ai/predict_am_queries`: partial Agent Mode query completion.
- `/ai/relevant_files`: local relevant-file ranking from Warp's repo outline payload.
