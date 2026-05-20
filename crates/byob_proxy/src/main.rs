#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    env, fs,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use base64::prelude::{Engine as _, BASE64_URL_SAFE};
use futures_util::{Stream, StreamExt as _};
use http::Uri;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};
use uuid::Uuid;
use warp_multi_agent_api as api;

type WarpSseItem = Result<Event, Infallible>;
type WarpSseStream = Pin<Box<dyn Stream<Item = WarpSseItem> + Send>>;

#[derive(Clone, Debug)]
struct Config {
    listen_addr: SocketAddr,
    openai_base_url: String,
    backend_url_override: Option<String>,
    openai_api_key: Option<String>,
    model_override: Option<String>,
    wire_api: WireApi,
    extra_headers: BTreeMap<String, String>,
    system_prompt: String,
    request_timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireApi {
    ChatCompletions,
    Responses,
}

impl WireApi {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" | "chat_completions" | "chat-completions" | "openai_chat" => {
                Some(Self::ChatCompletions)
            }
            "responses" | "openai_responses" | "responses_api" => Some(Self::Responses),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::Responses => "responses",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct CodexBackendConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    wire_api: Option<WireApi>,
    http_headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ByobConfigFile {
    #[serde(alias = "base_url")]
    openai_base_url: Option<String>,
    #[serde(alias = "backend_url")]
    backend_url_override: Option<String>,
    #[serde(alias = "api_key")]
    openai_api_key: Option<String>,
    model: Option<String>,
    wire_api: Option<String>,
    #[serde(default, alias = "http_headers")]
    extra_headers: BTreeMap<String, String>,
}

const DEFAULT_OCA_BASE_URL: &str =
    "https://code-internal.aiservice.us-chicago-1.oci.oraclecloud.com/20250206/app/litellm";
const DEFAULT_MODEL: &str = "gpt-5.5";

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let byob_config = load_byob_config_file();
        let codex_config = load_codex_backend_config();
        let listen_addr = env::var("BYOB_PROXY_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:1337".to_string())
            .parse()?;
        let openai_base_url = env_var_nonempty("BYOB_OPENAI_BASE_URL")
            .or_else(|| nonempty_string(byob_config.openai_base_url.clone()))
            .or(codex_config.base_url)
            .unwrap_or_else(|| DEFAULT_OCA_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let backend_url_override = env_var_nonempty("BYOB_BACKEND_URL")
            .or_else(|| nonempty_string(byob_config.backend_url_override.clone()));
        let openai_api_key = env_var_nonempty("BYOB_OPENAI_API_KEY")
            .or_else(|| nonempty_string(byob_config.openai_api_key.clone()))
            .or(codex_config.api_key);
        let model_override = env_var_nonempty("BYOB_MODEL")
            .or_else(|| nonempty_string(byob_config.model.clone()))
            .or(codex_config.model)
            .or_else(|| Some(DEFAULT_MODEL.to_string()));
        let wire_api = match env_var_nonempty("BYOB_WIRE_API") {
            Some(value) => WireApi::parse(&value)
                .ok_or_else(|| anyhow::anyhow!("unsupported BYOB_WIRE_API `{value}`"))?,
            None => byob_config
                .wire_api
                .as_deref()
                .and_then(WireApi::parse)
                .or(codex_config.wire_api)
                .unwrap_or(WireApi::Responses),
        };
        let mut extra_headers = default_extra_headers();
        extra_headers.extend(codex_config.http_headers);
        extra_headers.extend(byob_config.extra_headers);
        if let Some(headers) = env_var_nonempty("BYOB_EXTRA_HEADERS_JSON") {
            extra_headers.extend(parse_extra_headers_json(&headers)?);
        }
        let system_prompt =
            env::var("BYOB_SYSTEM_PROMPT").unwrap_or_else(|_| default_agent_system_prompt().into());
        let request_timeout = env::var("BYOB_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(600));

        Ok(Self {
            listen_addr,
            openai_base_url,
            backend_url_override,
            openai_api_key,
            model_override,
            wire_api,
            extra_headers,
            system_prompt,
            request_timeout,
        })
    }
}

fn default_extra_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("client".to_string(), "codex-cli".to_string()),
        ("client-version".to_string(), "0".to_string()),
    ])
}

fn default_agent_system_prompt() -> &'static str {
    "You are WarpOCA, an agent running inside the Warp terminal UI. \
     Be concise, but behave like an active terminal operator: state what you are about to do, \
     call tools for terminal work, observe the returned output, adapt, and finish with a clear conclusion. \
     Do not emit only tool calls when a short user-facing status sentence would help. \
     Never claim a command ran, a file changed, or a remote host state was verified until Warp returns the tool result. \
     If a command is still running, monitor it with the long-running command tools and explain what you observe."
}

fn env_var_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .and_then(|value| nonempty_string(Some(value)))
}

fn nonempty_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn load_byob_config_file() -> ByobConfigFile {
    byob_config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str::<ByobConfigFile>(&contents).ok())
        .unwrap_or_default()
}

fn byob_config_path() -> Option<PathBuf> {
    env_var_nonempty("BYOB_CONFIG_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            byob_config_candidates()
                .into_iter()
                .find(|path| path.exists())
        })
}

fn load_codex_backend_config() -> CodexBackendConfig {
    let mut config = codex_config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| parse_codex_config(&contents).ok())
        .unwrap_or_default();

    if config.api_key.is_none() {
        config.api_key = codex_auth_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|contents| parse_codex_auth_api_key(&contents));
    }

    config
}

fn codex_config_path() -> Option<PathBuf> {
    env_var_nonempty("BYOB_CODEX_CONFIG_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env_var_nonempty("CODEX_HOME").map(|home| PathBuf::from(home).join("config.toml"))
        })
        .or_else(|| user_home_dir().map(|home| home.join(".codex").join("config.toml")))
}

fn codex_auth_path() -> Option<PathBuf> {
    env_var_nonempty("BYOB_CODEX_AUTH_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env_var_nonempty("CODEX_HOME").map(|home| PathBuf::from(home).join("auth.json"))
        })
        .or_else(|| user_home_dir().map(|home| home.join(".codex").join("auth.json")))
}

fn byob_config_candidates() -> Vec<PathBuf> {
    app_support_roots()
        .into_iter()
        .flat_map(|root| {
            ["WarpOCA", "WarpOCI", "WarpByob"]
                .into_iter()
                .map(move |name| root.join(name).join("config.json"))
        })
        .collect()
}

fn app_support_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    #[cfg(target_os = "macos")]
    if let Some(home) = user_home_dir() {
        roots.push(home.join("Library/Application Support"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = env_var_nonempty("APPDATA").map(PathBuf::from) {
            roots.push(appdata);
        }
        if let Some(home) = user_home_dir() {
            roots.push(home.join("AppData").join("Roaming"));
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(data_home) = env_var_nonempty("XDG_DATA_HOME").map(PathBuf::from) {
            roots.push(data_home);
        }
        if let Some(home) = user_home_dir() {
            roots.push(home.join(".local").join("share"));
        }
    }

    roots
}

fn user_home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        env_var_nonempty("USERPROFILE")
            .or_else(|| env_var_nonempty("HOME"))
            .map(PathBuf::from)
    }

    #[cfg(not(target_os = "windows"))]
    {
        env_var_nonempty("HOME").map(PathBuf::from)
    }
}

fn parse_codex_config(contents: &str) -> anyhow::Result<CodexBackendConfig> {
    let doc = contents.parse::<DocumentMut>()?;
    let provider_name = codex_provider_name(&doc);
    let provider = provider_name.as_deref().and_then(|name| {
        doc.get("model_providers")?
            .as_table()?
            .get(name)?
            .as_table()
    });
    let profile = doc
        .get("profile")
        .and_then(Item::as_str)
        .and_then(|name| doc.get("profiles")?.as_table()?.get(name)?.as_table());

    let model = profile
        .and_then(|table| table.get("model"))
        .and_then(Item::as_str)
        .or_else(|| doc.get("model").and_then(Item::as_str))
        .or_else(|| {
            provider
                .and_then(|table| table.get("model"))
                .and_then(Item::as_str)
        })
        .map(ToOwned::to_owned);

    let base_url = provider
        .and_then(|table| table.get("base_url"))
        .and_then(Item::as_str)
        .map(ToOwned::to_owned);

    let wire_api = provider
        .and_then(|table| table.get("wire_api"))
        .and_then(Item::as_str)
        .and_then(WireApi::parse);

    let http_headers = provider
        .and_then(|table| table.get("http_headers"))
        .map(parse_toml_headers)
        .unwrap_or_default();

    Ok(CodexBackendConfig {
        base_url,
        api_key: None,
        model,
        wire_api,
        http_headers,
    })
}

fn codex_provider_name(doc: &DocumentMut) -> Option<String> {
    let top_level = doc.get("model_provider").and_then(Item::as_str);
    let profile_provider = doc
        .get("profile")
        .and_then(Item::as_str)
        .and_then(|name| doc.get("profiles")?.as_table()?.get(name)?.as_table())
        .and_then(|profile| profile.get("model_provider"))
        .and_then(Item::as_str);

    profile_provider.or(top_level).map(ToOwned::to_owned)
}

fn parse_toml_headers(item: &Item) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();

    if let Some(table) = item.as_table() {
        for (name, value) in table.iter() {
            if let Some(value) = value.as_str() {
                headers.insert(name.to_string(), value.to_string());
            }
        }
    }

    if let Some(table) = item.as_inline_table() {
        for (name, value) in table.iter() {
            if let Some(value) = value.as_str() {
                headers.insert(name.to_string(), value.to_string());
            }
        }
    }

    headers
}

fn parse_codex_auth_api_key(contents: &str) -> Option<String> {
    serde_json::from_str::<Value>(contents)
        .ok()?
        .get("OPENAI_API_KEY")?
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_extra_headers_json(contents: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let Value::Object(headers) = serde_json::from_str::<Value>(contents)? else {
        anyhow::bail!("BYOB_EXTRA_HEADERS_JSON must be a JSON object");
    };

    let mut parsed = BTreeMap::new();
    for (name, value) in headers {
        let Some(value) = value.as_str() else {
            anyhow::bail!("BYOB_EXTRA_HEADERS_JSON value for `{name}` must be a string");
        };
        parsed.insert(name, value.to_string());
    }
    Ok(parsed)
}

#[derive(Clone)]
struct AppState {
    config: Config,
    client: reqwest::Client,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(config.request_timeout)
        .build()?;
    let state = Arc::new(AppState {
        config: config.clone(),
        client,
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/ai/generate_input_suggestions",
            post(proxy_generate_input_suggestions),
        )
        .route(
            "/ai/generate_am_query_suggestions",
            post(proxy_generate_am_query_suggestions),
        )
        .route("/ai/generate_block_title", post(proxy_generate_block_title))
        .route("/ai/predict_am_queries", post(proxy_predict_am_queries))
        .route("/ai/relevant_files", post(proxy_relevant_files))
        .route("/", post(proxy_multi_agent))
        .route("/{*path}", post(proxy_multi_agent))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    eprintln!("byob_proxy listening on http://{}", config.listen_addr);
    eprintln!(
        "BYOB backend: {} ({})",
        config.openai_base_url,
        config.wire_api.as_str()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "backend": state.config.openai_base_url,
        "backend_url_override": state.config.backend_url_override.as_deref(),
        "model_override": state.config.model_override,
        "wire_api": state.config.wire_api.as_str(),
        "auth_configured": state.config.openai_api_key.is_some(),
        "extra_header_names": state.config.extra_headers.keys().collect::<Vec<_>>(),
    }))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct GenerateAIInputSuggestionsRequest {
    #[serde(default)]
    context_messages: Vec<ContextMessagePayload>,
    #[serde(default)]
    history_context: String,
    system_context: Option<String>,
    #[serde(default)]
    rejected_suggestions: Vec<String>,
    prefix: Option<String>,
    block_context: Option<Value>,
    previous_result: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ContextMessagePayload {
    String(String),
    Object(Value),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct AgentModeSuggestionV2 {
    query: String,
    context_block_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct GenerateAIInputSuggestionsResponseV2 {
    commands: Vec<String>,
    ai_queries: Vec<AgentModeSuggestionV2>,
    most_likely_action: String,
}

async fn proxy_generate_input_suggestions(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GenerateAIInputSuggestionsRequest>,
) -> Result<Json<GenerateAIInputSuggestionsResponseV2>, (StatusCode, String)> {
    if let Some(response) = local_next_command_suggestion(&request) {
        return Ok(Json(response));
    }

    if state.config.openai_api_key.is_none() {
        return Ok(Json(empty_input_suggestion_response()));
    }

    let backend_request = build_input_suggestions_backend_request(&request, &state.config);
    let response = send_backend_request(&state, &backend_request)
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach BYOB backend for input suggestions: {err}"),
            )
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "BYOB backend returned HTTP {status} for input suggestions: {}",
                truncate(&body, 1200)
            ),
        ));
    }

    let text = backend_text_from_body(state.config.wire_api, &body).map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Unable to parse BYOB input suggestion response: {err}"),
        )
    })?;

    Ok(Json(parse_input_suggestion_text(&request, &text)))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct GenerateAMQuerySuggestionsRequest {
    #[serde(default)]
    context_messages: Vec<ContextMessagePayload>,
    system_context: Option<String>,
    exit_code: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct GenerateAMQuerySuggestionsResponse {
    id: String,
    suggestion: Option<AMSuggestion>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AMSuggestion {
    Simple(AMSimpleQuery),
    Coding(AMCodingQuery),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct AMSimpleQuery {
    query: String,
    should_plan_task: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct AMCodingQuery {
    files: Vec<AMGeneratedFileLocations>,
    query: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct AMGeneratedFileLocations {
    file_name: String,
    line_numbers: Option<Vec<usize>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PredictAMQueriesRequest {
    #[serde(default)]
    context_messages: Vec<ContextMessagePayload>,
    partial_query: String,
    system_context: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct PredictAMQueriesResponse {
    suggestion: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct GetRelevantFilesRequest {
    query: String,
    #[serde(default)]
    files: Vec<RelevantFileContext>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RelevantFileContext {
    path: String,
    symbols: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct GetRelevantFilesResponse {
    relevant_file_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct GenerateBlockTitleRequest {
    command: String,
    output: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct GenerateBlockTitleResponse {
    title: String,
}

async fn proxy_generate_am_query_suggestions(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GenerateAMQuerySuggestionsRequest>,
) -> Result<Json<GenerateAMQuerySuggestionsResponse>, (StatusCode, String)> {
    if state.config.openai_api_key.is_none() {
        return Ok(Json(empty_am_query_suggestion_response()));
    }

    let backend_request = build_am_query_suggestion_backend_request(&request, &state.config);
    let text = send_backend_text(&state, &backend_request, "agent mode query suggestions").await?;
    Ok(Json(parse_am_query_suggestion_text(&text)))
}

async fn proxy_predict_am_queries(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PredictAMQueriesRequest>,
) -> Result<Json<PredictAMQueriesResponse>, (StatusCode, String)> {
    if state.config.openai_api_key.is_none() {
        return Ok(Json(PredictAMQueriesResponse {
            suggestion: String::new(),
        }));
    }

    let backend_request = build_predict_am_queries_backend_request(&request, &state.config);
    let text =
        send_backend_text(&state, &backend_request, "partial agent query prediction").await?;
    Ok(Json(PredictAMQueriesResponse {
        suggestion: clean_query_suggestion(&text, request.partial_query.as_str()),
    }))
}

async fn proxy_generate_block_title(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GenerateBlockTitleRequest>,
) -> Result<Json<GenerateBlockTitleResponse>, (StatusCode, String)> {
    if let Some(title) = local_block_title(&request) {
        return Ok(Json(GenerateBlockTitleResponse { title }));
    }

    if state.config.openai_api_key.is_none() {
        return Ok(Json(GenerateBlockTitleResponse {
            title: fallback_block_title(&request),
        }));
    }

    let backend_request = build_block_title_backend_request(&request, &state.config);
    let text = send_backend_text(&state, &backend_request, "block title generation").await?;
    Ok(Json(GenerateBlockTitleResponse {
        title: clean_block_title(&text),
    }))
}

async fn proxy_relevant_files(
    Json(request): Json<GetRelevantFilesRequest>,
) -> Json<GetRelevantFilesResponse> {
    Json(GetRelevantFilesResponse {
        relevant_file_paths: rank_relevant_files(&request),
    })
}

async fn proxy_multi_agent(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    body: Bytes,
) -> Result<Sse<WarpSseStream>, (StatusCode, String)> {
    let request = api::Request::decode(body.as_ref()).map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to decode warp_multi_agent_api::Request: {err}"),
        )
    })?;

    let ids = WarpIds::from_request(&request);
    let backend_request = build_backend_request(&request, &state.config);
    let state_for_stream = state.clone();
    let stream = stream! {
        yield Ok(warp_sse_event(init_event(&ids)));

        if request.task_context.as_ref().is_none_or(|ctx| ctx.tasks.is_empty()) {
            yield Ok(warp_sse_event(create_root_task_event(&ids)));
        }

        let mut output_message_id: Option<String> = None;
        let mut tool_calls = ToolCallAccumulatorSet::default();
        let mut parsed_any_backend_event = false;
        let mut seen_backend_text_delta = false;
        let mut decoder = SseDecoder::default();

        if state_for_stream.config.openai_api_key.is_none() {
            let message = format!(
                "WarpOCA could not find Codex CLI authentication. Run `codex login`, confirm `{}` exists, then relaunch WarpOCA.",
                codex_auth_hint(),
            );
            if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                yield Ok(event);
            }
            yield Ok(warp_sse_event(finished_event_internal_error(message)));
            return;
        }

        let backend_result = send_backend_request(&state_for_stream, &backend_request).await;
        match backend_result {
            Ok(response) => {
                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let message = format!("BYOB backend returned HTTP {status}: {}", truncate(&body, 1200));
                    if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                        yield Ok(event);
                    }
                    yield Ok(warp_sse_event(finished_event_internal_error(message)));
                    return;
                }

                let mut bytes_stream = response.bytes_stream();
                while let Some(chunk) = bytes_stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            for data in decoder.push(&text) {
                                parsed_any_backend_event = true;
                                if data.trim() == "[DONE]" {
                                    continue;
                                }
                                match parse_backend_sse_data(
                                    state_for_stream.config.wire_api,
                                    &data,
                                    &mut tool_calls,
                                    !seen_backend_text_delta,
                                ) {
                                    Ok(text_deltas) => {
                                        for content in text_deltas {
                                            if !content.is_empty() {
                                                seen_backend_text_delta = true;
                                            }
                                            if let Some(event) = text_delta_event(&ids, &mut output_message_id, &content) {
                                                yield Ok(event);
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        let message = format!("Unable to parse backend SSE chunk as {} delta: {err}", state_for_stream.config.wire_api.as_str());
                                        if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                                            yield Ok(event);
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let message = format!("BYOB backend stream failed: {err}");
                            if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                                yield Ok(event);
                            }
                            yield Ok(warp_sse_event(finished_event_internal_error(message)));
                            return;
                        }
                    }
                }

                if !parsed_any_backend_event {
                    let remaining = decoder.into_remaining();
                    if !remaining.trim().is_empty() {
                        match parse_backend_full_response(
                            state_for_stream.config.wire_api,
                            &remaining,
                            &mut tool_calls,
                            !seen_backend_text_delta,
                        ) {
                            Ok(text_deltas) => {
                                for content in text_deltas {
                                    if let Some(event) = text_delta_event(&ids, &mut output_message_id, &content) {
                                        yield Ok(event);
                                    }
                                }
                            }
                            Err(err) => {
                                let message = format!(
                                    "Backend did not return {} SSE or JSON response: {err}. Raw body: {}",
                                    state_for_stream.config.wire_api.as_str(),
                                    truncate(&remaining, 1200)
                                );
                                if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                                    yield Ok(event);
                                }
                            }
                        }
                    }
                }

                let mut unsupported = Vec::new();
                let mut warp_tool_calls = Vec::new();
                let mut suggestions = api::Suggestions::default();
                for result in tool_calls.into_warp_client_outputs() {
                    match result {
                        Ok(WarpClientOutput::ToolCall(call)) => warp_tool_calls.push(call),
                        Ok(WarpClientOutput::Suggestions(new_suggestions)) => {
                            suggestions.rules.extend(new_suggestions.rules);
                            suggestions.workflows.extend(new_suggestions.workflows);
                        }
                        Err(err) => {
                            unsupported.push(err.to_string());
                        }
                    }
                }

                if !unsupported.is_empty() {
                    let message = format!("Unsupported or malformed backend tool call: {}", unsupported.join("; "));
                    if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                        yield Ok(event);
                    }
                }

                if !warp_tool_calls.is_empty() {
                    yield Ok(warp_sse_event(add_tool_calls_event(&ids, warp_tool_calls)));
                }

                if !suggestions.rules.is_empty() || !suggestions.workflows.is_empty() {
                    yield Ok(warp_sse_event(show_suggestions_event(suggestions)));
                }

                yield Ok(warp_sse_event(finished_event_done()));
            }
            Err(err) => {
                let message = format!("Failed to reach BYOB backend for {}: {err}", uri.path());
                if let Some(event) = text_delta_event(&ids, &mut output_message_id, &message) {
                    yield Ok(event);
                }
                yield Ok(warp_sse_event(finished_event_internal_error(message)));
            }
        }
    };

    Ok(Sse::new(Box::pin(stream) as WarpSseStream).keep_alive(KeepAlive::default()))
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum BackendRequest {
    Chat(OpenAIChatRequest),
    Responses(OpenAIResponsesRequest),
}

impl BackendRequest {
    fn stream(&self) -> bool {
        match self {
            Self::Chat(request) => request.stream,
            Self::Responses(request) => request.stream,
        }
    }
}

fn codex_auth_hint() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        r"%USERPROFILE%\.codex\auth.json"
    }

    #[cfg(not(target_os = "windows"))]
    {
        "~/.codex/auth.json"
    }
}

fn build_backend_request(request: &api::Request, config: &Config) -> BackendRequest {
    let chat_request = build_chat_request(request, config);
    match config.wire_api {
        WireApi::ChatCompletions => BackendRequest::Chat(chat_request),
        WireApi::Responses => {
            BackendRequest::Responses(OpenAIResponsesRequest::from_chat_request(chat_request))
        }
    }
}

fn build_input_suggestions_backend_request(
    request: &GenerateAIInputSuggestionsRequest,
    config: &Config,
) -> BackendRequest {
    let chat_request = OpenAIChatRequest {
        model: config
            .model_override
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        messages: vec![
            OpenAIChatMessage::system(next_command_system_prompt()),
            OpenAIChatMessage::user(format_input_suggestion_prompt(request)),
        ],
        tools: Vec::new(),
        stream: true,
    };

    match config.wire_api {
        WireApi::ChatCompletions => BackendRequest::Chat(chat_request),
        WireApi::Responses => {
            BackendRequest::Responses(OpenAIResponsesRequest::from_chat_request(chat_request))
        }
    }
}

fn build_am_query_suggestion_backend_request(
    request: &GenerateAMQuerySuggestionsRequest,
    config: &Config,
) -> BackendRequest {
    let chat_request = OpenAIChatRequest {
        model: config
            .model_override
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        messages: vec![
            OpenAIChatMessage::system(agent_mode_suggestion_system_prompt()),
            OpenAIChatMessage::user(format_am_query_suggestion_prompt(request)),
        ],
        tools: Vec::new(),
        stream: true,
    };

    match config.wire_api {
        WireApi::ChatCompletions => BackendRequest::Chat(chat_request),
        WireApi::Responses => {
            BackendRequest::Responses(OpenAIResponsesRequest::from_chat_request(chat_request))
        }
    }
}

fn build_predict_am_queries_backend_request(
    request: &PredictAMQueriesRequest,
    config: &Config,
) -> BackendRequest {
    let chat_request = OpenAIChatRequest {
        model: config
            .model_override
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        messages: vec![
            OpenAIChatMessage::system(
                "Complete the user's partial Agent Mode query for a terminal assistant. \
                 Return only the completed query text. No markdown, no JSON, no explanation.",
            ),
            OpenAIChatMessage::user(format_predict_am_queries_prompt(request)),
        ],
        tools: Vec::new(),
        stream: true,
    };

    match config.wire_api {
        WireApi::ChatCompletions => BackendRequest::Chat(chat_request),
        WireApi::Responses => {
            BackendRequest::Responses(OpenAIResponsesRequest::from_chat_request(chat_request))
        }
    }
}

fn build_block_title_backend_request(
    request: &GenerateBlockTitleRequest,
    config: &Config,
) -> BackendRequest {
    let chat_request = OpenAIChatRequest {
        model: config
            .model_override
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        messages: vec![
            OpenAIChatMessage::system(
                "Generate a concise terminal block title. Return only the title text. \
                 No quotes, no markdown, no trailing punctuation unless it is part of a command.",
            ),
            OpenAIChatMessage::user(format!(
                "Command:\n{}\n\nOutput:\n{}",
                truncate(&request.command, 1000),
                truncate(&request.output, 5000)
            )),
        ],
        tools: Vec::new(),
        stream: true,
    };

    match config.wire_api {
        WireApi::ChatCompletions => BackendRequest::Chat(chat_request),
        WireApi::Responses => {
            BackendRequest::Responses(OpenAIResponsesRequest::from_chat_request(chat_request))
        }
    }
}

fn next_command_system_prompt() -> &'static str {
    "You predict the next shell command for Warp terminal. Return JSON only, with this exact shape: \
     {\"commands\":[\"command\"],\"ai_queries\":[],\"most_likely_action\":\"command\"}. \
     The command must be a single shell input line, with no markdown, no leading prompt symbol, \
     and no explanation. If no useful next command is likely, return \
     {\"commands\":[],\"ai_queries\":[],\"most_likely_action\":\"\"}."
}

fn agent_mode_suggestion_system_prompt() -> &'static str {
    "You suggest a useful follow-up Agent Mode query after a terminal command finishes. \
     Return JSON only with this exact shape: \
     {\"query\":\"short user-facing request\",\"should_plan_task\":false}. \
     If there is no useful suggestion, return {\"query\":\"\",\"should_plan_task\":false}. \
     Do not include markdown or explanations."
}

fn format_input_suggestion_prompt(request: &GenerateAIInputSuggestionsRequest) -> String {
    let mut prompt = String::new();

    if let Some(prefix) = request.prefix.as_deref().filter(|value| !value.is_empty()) {
        prompt.push_str("User has already typed this prefix. Any command must start with it:\n");
        prompt.push_str(prefix);
        prompt.push_str("\n\n");
    }

    if !request.rejected_suggestions.is_empty() {
        prompt.push_str("Do not return these rejected suggestions:\n");
        for suggestion in &request.rejected_suggestions {
            prompt.push_str("- ");
            prompt.push_str(suggestion);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    if let Some(system_context) = request
        .system_context
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        prompt.push_str("System context:\n");
        prompt.push_str(&truncate(system_context, 3000));
        prompt.push_str("\n\n");
    }

    if !request.history_context.trim().is_empty() {
        prompt.push_str("Relevant shell history:\n");
        prompt.push_str(&truncate(&request.history_context, 5000));
        prompt.push_str("\n\n");
    }

    prompt.push_str("Most recent terminal blocks, oldest to newest:\n");
    for message in request.context_messages.iter().rev().take(8).rev() {
        prompt.push_str(&truncate(&context_message_payload_to_string(message), 2500));
        prompt.push('\n');
    }

    if let Some(block_context) = &request.block_context {
        prompt.push_str("\nCurrent block context:\n");
        prompt.push_str(&truncate(&block_context.to_string(), 2500));
    }

    if let Some(previous_result) = &request.previous_result {
        prompt.push_str("\nPrevious autosuggestion result:\n");
        prompt.push_str(&truncate(&previous_result.to_string(), 1500));
    }

    prompt
}

fn format_am_query_suggestion_prompt(request: &GenerateAMQuerySuggestionsRequest) -> String {
    let mut prompt = String::new();

    prompt.push_str("The last terminal command exit code was ");
    prompt.push_str(&request.exit_code.to_string());
    prompt.push_str(".\n\n");

    if let Some(system_context) = request
        .system_context
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        prompt.push_str("System context:\n");
        prompt.push_str(&truncate(system_context, 3000));
        prompt.push_str("\n\n");
    }

    prompt.push_str("Recent terminal blocks, oldest to newest:\n");
    for message in request.context_messages.iter().rev().take(8).rev() {
        prompt.push_str(&truncate(&context_message_payload_to_string(message), 2500));
        prompt.push('\n');
    }

    prompt
}

fn format_predict_am_queries_prompt(request: &PredictAMQueriesRequest) -> String {
    let mut prompt = String::new();

    prompt.push_str("Partial query:\n");
    prompt.push_str(&request.partial_query);
    prompt.push_str("\n\n");

    if let Some(system_context) = request
        .system_context
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        prompt.push_str("System context:\n");
        prompt.push_str(&truncate(system_context, 3000));
        prompt.push_str("\n\n");
    }

    prompt.push_str("Recent terminal context:\n");
    for message in request.context_messages.iter().rev().take(6).rev() {
        prompt.push_str(&truncate(&context_message_payload_to_string(message), 2000));
        prompt.push('\n');
    }

    prompt
}

async fn send_backend_text(
    state: &AppState,
    backend_request: &BackendRequest,
    feature_name: &str,
) -> Result<String, (StatusCode, String)> {
    let response = send_backend_request(state, backend_request)
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach BYOB backend for {feature_name}: {err}"),
            )
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "BYOB backend returned HTTP {status} for {feature_name}: {}",
                truncate(&body, 1200)
            ),
        ));
    }

    backend_text_from_body(state.config.wire_api, &body).map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Unable to parse BYOB {feature_name} response: {err}"),
        )
    })
}

fn backend_text_from_body(wire_api: WireApi, body: &str) -> anyhow::Result<String> {
    let mut tool_calls = ToolCallAccumulatorSet::default();
    let mut text = Vec::new();

    if looks_like_sse(body) {
        let mut decoder = SseDecoder::default();
        let mut seen_backend_text_delta = false;
        for data in decoder.push(body) {
            if data.trim() != "[DONE]" {
                let text_deltas = parse_backend_sse_data(
                    wire_api,
                    &data,
                    &mut tool_calls,
                    !seen_backend_text_delta,
                )?;
                if text_deltas.iter().any(|delta| !delta.is_empty()) {
                    seen_backend_text_delta = true;
                }
                text.extend(text_deltas);
            }
        }

        let remaining = decoder.into_remaining();
        if !remaining.trim().is_empty() && !remaining.trim_start().starts_with("data:") {
            text.extend(parse_backend_full_response(
                wire_api,
                &remaining,
                &mut tool_calls,
                !seen_backend_text_delta,
            )?);
        }
    } else {
        text.extend(parse_backend_full_response(
            wire_api,
            body,
            &mut tool_calls,
            true,
        )?);
    }

    Ok(text.join(""))
}

fn looks_like_sse(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with("data:") || trimmed.starts_with("event:")
}

fn context_message_payload_to_string(message: &ContextMessagePayload) -> String {
    match message {
        ContextMessagePayload::String(message) => message.clone(),
        ContextMessagePayload::Object(value) => value.to_string(),
    }
}

async fn send_backend_request(
    state: &AppState,
    backend_request: &BackendRequest,
) -> reqwest::Result<reqwest::Response> {
    let url = state
        .config
        .backend_url_override
        .clone()
        .unwrap_or_else(|| {
            join_endpoint(
                &state.config.openai_base_url,
                state.config.wire_api.endpoint(),
            )
        });
    let mut request = state.client.post(url).json(backend_request);
    if let Some(api_key) = &state.config.openai_api_key {
        request = request.bearer_auth(api_key);
    }
    if state.config.wire_api == WireApi::Responses && backend_request.stream() {
        request = request.header("Accept", "text/event-stream");
    }
    for (name, value) in &state.config.extra_headers {
        if !name.eq_ignore_ascii_case("authorization") {
            request = request.header(name, value);
        }
    }
    if is_oracle_code_assist_backend(&state.config.openai_base_url) {
        request = add_header_if_absent(
            request,
            &state.config.extra_headers,
            "opc-request-id",
            format!("byob-{}", Uuid::new_v4()),
        );
        request = add_header_if_absent(
            request,
            &state.config.extra_headers,
            "client-ide",
            "codex".to_string(),
        );
        request = add_header_if_absent(
            request,
            &state.config.extra_headers,
            "client-ide-version",
            "0".to_string(),
        );
    }
    request.send().await
}

fn join_endpoint(base_url: &str, endpoint: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with(endpoint) {
        base_url.to_string()
    } else {
        format!("{base_url}/{endpoint}")
    }
}

fn is_oracle_code_assist_backend(base_url: &str) -> bool {
    base_url.contains("oraclecloud.com") || base_url.contains("aiservice")
}

fn add_header_if_absent(
    request: reqwest::RequestBuilder,
    configured_headers: &BTreeMap<String, String>,
    name: &'static str,
    value: String,
) -> reqwest::RequestBuilder {
    if configured_headers
        .keys()
        .any(|configured| configured.eq_ignore_ascii_case(name))
    {
        request
    } else {
        request.header(name, value)
    }
}

fn text_delta_event(
    ids: &WarpIds,
    output_message_id: &mut Option<String>,
    content: &str,
) -> Option<Event> {
    if content.is_empty() {
        return None;
    }

    Some(match output_message_id {
        Some(message_id) => warp_sse_event(append_agent_output_event(ids, message_id, content)),
        None => {
            let message_id = format!("msg_{}", Uuid::new_v4());
            let event = warp_sse_event(add_agent_output_event(ids, &message_id, content));
            *output_message_id = Some(message_id);
            event
        }
    })
}

#[derive(Clone, Debug)]
struct WarpIds {
    conversation_id: String,
    request_id: String,
    run_id: String,
    task_id: String,
}

impl WarpIds {
    fn from_request(request: &api::Request) -> Self {
        let conversation_id = request
            .metadata
            .as_ref()
            .map(|metadata| metadata.conversation_id.as_str())
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("conv_{}", Uuid::new_v4()));
        let run_id = request
            .metadata
            .as_ref()
            .map(|metadata| metadata.ambient_agent_task_id.as_str())
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| conversation_id.clone());
        let task_id = request
            .task_context
            .as_ref()
            .and_then(|context| context.tasks.first())
            .map(|task| task.id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("task_{}", Uuid::new_v4()));

        Self {
            conversation_id,
            request_id: format!("req_{}", Uuid::new_v4()),
            run_id,
            task_id,
        }
    }
}

fn init_event(ids: &WarpIds) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Init(
            api::response_event::StreamInit {
                conversation_id: ids.conversation_id.clone(),
                request_id: ids.request_id.clone(),
                run_id: ids.run_id.clone(),
            },
        )),
    }
}

fn create_root_task_event(ids: &WarpIds) -> api::ResponseEvent {
    client_actions_event(vec![api::ClientAction {
        action: Some(api::client_action::Action::CreateTask(
            api::client_action::CreateTask {
                task: Some(api::Task {
                    id: ids.task_id.clone(),
                    description: "WarpOCA agent".to_string(),
                    dependencies: None,
                    messages: vec![],
                    summary: String::new(),
                    server_data: String::new(),
                    ..Default::default()
                }),
            },
        )),
    }])
}

fn add_agent_output_event(ids: &WarpIds, message_id: &str, text: &str) -> api::ResponseEvent {
    client_actions_event(vec![api::ClientAction {
        action: Some(api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask {
                task_id: ids.task_id.clone(),
                messages: vec![agent_output_message(ids, message_id, text)],
            },
        )),
    }])
}

fn append_agent_output_event(ids: &WarpIds, message_id: &str, text: &str) -> api::ResponseEvent {
    client_actions_event(vec![api::ClientAction {
        action: Some(api::client_action::Action::AppendToMessageContent(
            api::client_action::AppendToMessageContent {
                task_id: ids.task_id.clone(),
                message: Some(agent_output_message(ids, message_id, text)),
                mask: Some(prost_types::FieldMask {
                    paths: vec!["agent_output.text".to_string()],
                }),
            },
        )),
    }])
}

fn add_tool_calls_event(
    ids: &WarpIds,
    tool_calls: Vec<api::message::ToolCall>,
) -> api::ResponseEvent {
    let messages = tool_calls
        .into_iter()
        .map(|tool_call| api::Message {
            id: format!("msg_{}", Uuid::new_v4()),
            task_id: ids.task_id.clone(),
            request_id: ids.request_id.clone(),
            timestamp: current_timestamp(),
            server_message_data: String::new(),
            citations: vec![],
            message: Some(api::message::Message::ToolCall(tool_call)),
            ..Default::default()
        })
        .collect();

    client_actions_event(vec![api::ClientAction {
        action: Some(api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask {
                task_id: ids.task_id.clone(),
                messages,
            },
        )),
    }])
}

fn show_suggestions_event(suggestions: api::Suggestions) -> api::ResponseEvent {
    client_actions_event(vec![api::ClientAction {
        action: Some(api::client_action::Action::ShowSuggestions(suggestions)),
    }])
}

fn agent_output_message(ids: &WarpIds, message_id: &str, text: &str) -> api::Message {
    api::Message {
        id: message_id.to_string(),
        task_id: ids.task_id.clone(),
        request_id: ids.request_id.clone(),
        timestamp: current_timestamp(),
        server_message_data: String::new(),
        citations: vec![],
        message: Some(api::message::Message::AgentOutput(
            api::message::AgentOutput {
                text: text.to_string(),
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

fn current_timestamp() -> Option<prost_types::Timestamp> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(prost_types::Timestamp {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    })
}

fn client_actions_event(actions: Vec<api::ClientAction>) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::ClientActions(
            api::response_event::ClientActions { actions },
        )),
    }
}

fn finished_event_done() -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Finished(
            api::response_event::StreamFinished {
                reason: Some(api::response_event::stream_finished::Reason::Done(
                    api::response_event::stream_finished::Done {},
                )),
                token_usage: vec![],
                should_refresh_model_config: false,
                request_cost: None,
                conversation_usage_metadata: None,
            },
        )),
    }
}

fn finished_event_internal_error(message: String) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Finished(
            api::response_event::StreamFinished {
                reason: Some(api::response_event::stream_finished::Reason::InternalError(
                    api::response_event::stream_finished::InternalError { message },
                )),
                token_usage: vec![],
                should_refresh_model_config: false,
                request_cost: None,
                conversation_usage_metadata: None,
            },
        )),
    }
}

fn warp_sse_event(event: api::ResponseEvent) -> Event {
    Event::default().data(BASE64_URL_SAFE.encode(event.encode_to_vec()))
}

#[derive(Clone, Debug, Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<OpenAIChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAITool>,
    stream: bool,
}

#[derive(Clone, Debug, Serialize)]
struct OpenAIResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAIResponsesTool>,
    stream: bool,
    store: bool,
}

impl OpenAIResponsesRequest {
    fn from_chat_request(chat: OpenAIChatRequest) -> Self {
        let input = responses_input_items_from_chat_messages(chat.messages);
        let tools = chat
            .tools
            .iter()
            .map(OpenAIResponsesTool::from_openai_tool)
            .collect();

        Self {
            model: chat.model,
            input,
            tools,
            stream: chat.stream,
            store: false,
        }
    }
}

fn responses_input_items_from_chat_messages(messages: Vec<OpenAIChatMessage>) -> Vec<Value> {
    let input = messages
        .into_iter()
        .flat_map(responses_input_items_from_chat_message)
        .collect::<Vec<_>>();
    retain_paired_function_io(input)
}

fn responses_input_items_from_chat_message(message: OpenAIChatMessage) -> Vec<Value> {
    let mut items = Vec::new();

    if let Some(content) = message.content.filter(|content| !content.is_empty()) {
        if message.role == "tool" {
            items.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.unwrap_or_else(|| format!("tool_{}", Uuid::new_v4())),
                "output": content,
            }));
        } else {
            items.push(json!({
                "role": message.role,
                "content": content,
            }));
        }
    }

    for tool_call in message.tool_calls.unwrap_or_default() {
        items.push(json!({
            "type": "function_call",
            "call_id": tool_call.id,
            "name": tool_call.function.name,
            "arguments": tool_call.function.arguments,
        }));
    }

    items
}

fn retain_paired_function_io(input: Vec<Value>) -> Vec<Value> {
    let function_calls = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(value_call_id)
        .collect::<BTreeSet<_>>();
    let function_outputs = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(value_call_id)
        .collect::<BTreeSet<_>>();

    input
        .into_iter()
        .filter(|item| match item.get("type").and_then(Value::as_str) {
            Some("function_call") => value_call_id(item)
                .as_deref()
                .is_some_and(|call_id| function_outputs.contains(call_id)),
            Some("function_call_output") => value_call_id(item)
                .as_deref()
                .is_some_and(|call_id| function_calls.contains(call_id)),
            _ => true,
        })
        .collect()
}

fn value_call_id(value: &Value) -> Option<String> {
    value.get("call_id")?.as_str().map(ToOwned::to_owned)
}

#[derive(Clone, Debug, Serialize)]
struct OpenAIResponsesTool {
    r#type: &'static str,
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

impl OpenAIResponsesTool {
    fn from_openai_tool(tool: &OpenAITool) -> Self {
        Self {
            r#type: "function",
            name: tool.function.name,
            description: tool.function.description,
            parameters: tool.function.parameters.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OpenAIChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIMessageToolCall>>,
}

impl OpenAIChatMessage {
    fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_tool_call(tool_call: OpenAIMessageToolCall) -> Self {
        Self {
            role: "assistant".to_string(),
            content: None,
            tool_call_id: None,
            tool_calls: Some(vec![tool_call]),
        }
    }

    fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct OpenAITool {
    r#type: &'static str,
    function: OpenAIFunctionDefinition,
}

#[derive(Clone, Debug, Serialize)]
struct OpenAIFunctionDefinition {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OpenAIMessageToolCall {
    id: String,
    r#type: String,
    function: OpenAIFunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct SuggestionContextMessage {
    input: String,
    output: String,
}

fn local_next_command_suggestion(
    request: &GenerateAIInputSuggestionsRequest,
) -> Option<GenerateAIInputSuggestionsResponseV2> {
    brew_search_install_suggestion(request)
        .filter(|command| command_is_allowed_for_request(request, command))
        .map(suggestion_response)
}

fn brew_search_install_suggestion(request: &GenerateAIInputSuggestionsRequest) -> Option<String> {
    let context = latest_context_message(request)?;
    let input = context.input.trim();
    let mut parts = input.split_whitespace();

    if parts.next()? != "brew" || parts.next()? != "search" {
        return None;
    }

    let query = parts.find(|part| !part.starts_with('-'))?;
    if query.is_empty()
        || query.contains('/')
        || query.contains('\\')
        || !query
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+' | '@'))
    {
        return None;
    }

    let output = context.output.trim();
    if output.is_empty()
        || output.contains("No formula")
        || output.contains("No cask")
        || output.contains("Error:")
    {
        return None;
    }

    let query_matches_output = output
        .lines()
        .flat_map(|line| line.split_whitespace())
        .any(|token| token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric()) == query);

    query_matches_output.then(|| format!("brew install {query}"))
}

fn latest_context_message(
    request: &GenerateAIInputSuggestionsRequest,
) -> Option<SuggestionContextMessage> {
    request
        .context_messages
        .iter()
        .rev()
        .find_map(context_message_from_payload)
}

fn context_message_from_payload(
    message: &ContextMessagePayload,
) -> Option<SuggestionContextMessage> {
    match message {
        ContextMessagePayload::Object(value) => {
            serde_json::from_value::<SuggestionContextMessage>(value.clone()).ok()
        }
        ContextMessagePayload::String(message) => {
            serde_json::from_str::<SuggestionContextMessage>(message)
                .ok()
                .or_else(|| forgiving_context_message_parse(message))
        }
    }
}

fn forgiving_context_message_parse(message: &str) -> Option<SuggestionContextMessage> {
    Some(SuggestionContextMessage {
        input: extract_json_string_field(message, "input")?,
        output: extract_json_string_field(message, "output")?,
    })
}

fn extract_json_string_field(message: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\":\"");
    let start = message.find(&marker)? + marker.len();
    let mut value = String::new();
    let mut escaped = false;

    for ch in message[start..].chars() {
        if escaped {
            match ch {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                other => value.push(other),
            }
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some(value),
            other => value.push(other),
        }
    }

    None
}

fn command_is_allowed_for_request(
    request: &GenerateAIInputSuggestionsRequest,
    command: &str,
) -> bool {
    if command.trim().is_empty() || command.contains('\n') {
        return false;
    }

    if request
        .rejected_suggestions
        .iter()
        .any(|rejected| rejected.trim() == command)
    {
        return false;
    }

    request
        .prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty())
        .is_none_or(|prefix| command.starts_with(prefix))
}

fn parse_input_suggestion_text(
    request: &GenerateAIInputSuggestionsRequest,
    text: &str,
) -> GenerateAIInputSuggestionsResponseV2 {
    let cleaned = strip_code_fences(text.trim());
    let candidates = parse_suggestion_candidates(cleaned);

    candidates
        .into_iter()
        .map(clean_command_candidate)
        .find(|command| command_is_allowed_for_request(request, command))
        .map(suggestion_response)
        .unwrap_or_else(empty_input_suggestion_response)
}

fn parse_suggestion_candidates(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return suggestion_candidates_from_value(&value);
    }

    if let Some(value) = first_json_value(text) {
        return suggestion_candidates_from_value(&value);
    }

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn first_json_value(text: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
}

fn suggestion_candidates_from_value(value: &Value) -> Vec<String> {
    if let Some(command) = value.get("most_likely_action").and_then(Value::as_str) {
        if !command.trim().is_empty() {
            return vec![command.to_string()];
        }
    }

    if let Some(commands) = value.get("commands").and_then(Value::as_array) {
        return commands
            .iter()
            .filter_map(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }

    if let Some(commands) = value.as_array() {
        return commands
            .iter()
            .filter_map(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }

    value
        .as_str()
        .map(|command| vec![command.to_string()])
        .unwrap_or_default()
}

fn strip_code_fences(text: &str) -> &str {
    let text = text.trim();
    let Some(stripped) = text.strip_prefix("```") else {
        return text;
    };
    let stripped = stripped
        .strip_prefix("json")
        .or_else(|| stripped.strip_prefix("JSON"))
        .unwrap_or(stripped)
        .trim_start_matches(['\r', '\n']);
    stripped.strip_suffix("```").unwrap_or(stripped).trim()
}

fn clean_command_candidate(command: String) -> String {
    command
        .trim()
        .trim_start_matches("$ ")
        .trim_start_matches("% ")
        .trim()
        .to_string()
}

fn suggestion_response(command: String) -> GenerateAIInputSuggestionsResponseV2 {
    GenerateAIInputSuggestionsResponseV2 {
        commands: vec![command.clone()],
        ai_queries: Vec::new(),
        most_likely_action: command,
    }
}

fn empty_input_suggestion_response() -> GenerateAIInputSuggestionsResponseV2 {
    GenerateAIInputSuggestionsResponseV2 {
        commands: Vec::new(),
        ai_queries: Vec::new(),
        most_likely_action: String::new(),
    }
}

fn local_block_title(request: &GenerateBlockTitleRequest) -> Option<String> {
    let command = request.command.trim();
    if command.is_empty() {
        return None;
    }

    let mut parts = command.split_whitespace();
    let executable = parts.next().unwrap_or_default();
    let title = match executable {
        "brew" => match parts.next() {
            Some("search") => format!(
                "Search Homebrew for {}",
                parts.collect::<Vec<_>>().join(" ")
            ),
            Some("install") => format!("Install {}", parts.collect::<Vec<_>>().join(" ")),
            Some("upgrade") => "Upgrade Homebrew packages".to_string(),
            Some("update") => "Update Homebrew".to_string(),
            _ => fallback_block_title(request),
        },
        "git" => match parts.next() {
            Some("status") => "Check git status".to_string(),
            Some("diff") => "Inspect git diff".to_string(),
            Some("log") => "Inspect git history".to_string(),
            Some("pull") => "Pull git changes".to_string(),
            Some("push") => "Push git changes".to_string(),
            Some("commit") => "Commit git changes".to_string(),
            _ => fallback_block_title(request),
        },
        "cargo" => match parts.next() {
            Some("test") => "Run Rust tests".to_string(),
            Some("check") => "Check Rust project".to_string(),
            Some("build") => "Build Rust project".to_string(),
            _ => fallback_block_title(request),
        },
        "npm" | "pnpm" | "yarn" => match parts.next() {
            Some("test") => "Run JavaScript tests".to_string(),
            Some("build") => "Build JavaScript project".to_string(),
            Some("install") => "Install JavaScript dependencies".to_string(),
            _ => fallback_block_title(request),
        },
        _ => return None,
    };

    Some(clean_block_title(&title))
}

fn fallback_block_title(request: &GenerateBlockTitleRequest) -> String {
    clean_block_title(
        request
            .command
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .unwrap_or("Terminal command"),
    )
}

fn clean_block_title(text: &str) -> String {
    let mut title = strip_code_fences(text)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Terminal command")
        .to_string();

    title = title.trim_end_matches('.').trim().to_string();
    if title.chars().count() > 80 {
        title = title.chars().take(77).collect::<String>();
        title.push_str("...");
    }
    if title.is_empty() {
        "Terminal command".to_string()
    } else {
        title
    }
}

fn parse_am_query_suggestion_text(text: &str) -> GenerateAMQuerySuggestionsResponse {
    let cleaned = strip_code_fences(text.trim());
    let query = if let Ok(value) = serde_json::from_str::<Value>(cleaned) {
        query_from_json_value(&value).map(ToOwned::to_owned)
    } else if let Some(value) = first_json_value(cleaned) {
        query_from_json_value(&value).map(ToOwned::to_owned)
    } else {
        cleaned
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(ToOwned::to_owned)
    }
    .map(clean_query_line)
    .filter(|query| !query.is_empty());

    GenerateAMQuerySuggestionsResponse {
        id: format!("byob-{}", Uuid::new_v4()),
        suggestion: query.map(|query| {
            AMSuggestion::Simple(AMSimpleQuery {
                query,
                should_plan_task: false,
            })
        }),
    }
}

fn query_from_json_value(value: &Value) -> Option<&str> {
    value
        .get("query")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("simple")
                .and_then(|simple| simple.get("query"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("suggestion")
                .and_then(|suggestion| suggestion.get("query"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("suggestion")
                .and_then(|suggestion| suggestion.get("simple"))
                .and_then(|simple| simple.get("query"))
                .and_then(Value::as_str)
        })
}

fn empty_am_query_suggestion_response() -> GenerateAMQuerySuggestionsResponse {
    GenerateAMQuerySuggestionsResponse {
        id: format!("byob-{}", Uuid::new_v4()),
        suggestion: None,
    }
}

fn clean_query_suggestion(text: &str, partial_query: &str) -> String {
    let cleaned = strip_code_fences(text.trim());
    let suggestion = if let Ok(value) = serde_json::from_str::<Value>(cleaned) {
        query_from_json_value(&value)
            .or_else(|| value.get("suggestion").and_then(Value::as_str))
            .or_else(|| value.as_str())
            .unwrap_or_default()
            .to_string()
    } else if let Some(value) = first_json_value(cleaned) {
        query_from_json_value(&value)
            .or_else(|| value.get("suggestion").and_then(Value::as_str))
            .or_else(|| value.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        cleaned.to_string()
    };

    let suggestion = clean_query_line(suggestion);
    if suggestion.is_empty() {
        return String::new();
    }
    if suggestion.starts_with(partial_query) {
        suggestion
    } else {
        format!("{partial_query}{suggestion}")
    }
}

fn clean_query_line(query: impl AsRef<str>) -> String {
    query
        .as_ref()
        .trim()
        .trim_matches('"')
        .trim_start_matches("- ")
        .trim()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn rank_relevant_files(request: &GetRelevantFilesRequest) -> Vec<String> {
    let query_terms = tokenize_for_ranking(&request.query);
    if query_terms.is_empty() {
        return request
            .files
            .iter()
            .take(8)
            .map(|file| file.path.clone())
            .collect();
    }

    let mut scored = request
        .files
        .iter()
        .filter_map(|file| {
            let path_lower = file.path.to_ascii_lowercase();
            let symbols_lower = file.symbols.to_ascii_lowercase();
            let file_name = path_lower.rsplit('/').next().unwrap_or(path_lower.as_str());
            let mut score = 0usize;

            for term in &query_terms {
                if file_name.contains(term) {
                    score += 8;
                }
                if path_lower.contains(term) {
                    score += 4;
                }
                if symbols_lower.contains(term) {
                    score += 2;
                }
            }

            (score > 0).then_some((score, file.path.clone()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(left_score, left_path), (right_score, right_path)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_path.len().cmp(&right_path.len()))
            .then_with(|| left_path.cmp(right_path))
    });
    scored.into_iter().take(12).map(|(_, path)| path).collect()
}

fn tokenize_for_ranking(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|term| term.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn build_chat_request(request: &api::Request, config: &Config) -> OpenAIChatRequest {
    let model = selected_model(request)
        .or_else(|| config.model_override.clone())
        .unwrap_or_else(|| "local-model".to_string());

    let mut messages = vec![OpenAIChatMessage::system(format!(
        "{}\n\n{}",
        config.system_prompt,
        warp_tool_instructions()
    ))];

    if let Some(context_summary) = request
        .input
        .as_ref()
        .and_then(|input| input.context.as_ref())
        .map(format_input_context)
        .filter(|summary| !summary.is_empty())
    {
        messages.push(OpenAIChatMessage::system(context_summary));
    }

    if let Some(task_context) = &request.task_context {
        for task in &task_context.tasks {
            for message in &task.messages {
                append_history_message(&mut messages, message);
            }
        }
    }

    if let Some(input) = &request.input {
        append_request_input_messages(&mut messages, input, request);
    }

    if messages.len() == 1 {
        messages.push(OpenAIChatMessage::user("Continue."));
    }

    OpenAIChatRequest {
        model,
        messages,
        tools: build_tool_definitions(request),
        stream: true,
    }
}

fn selected_model(request: &api::Request) -> Option<String> {
    let model_config = request.settings.as_ref()?.model_config.as_ref()?;
    [
        model_config.coding.as_str(),
        model_config.base.as_str(),
        model_config.cli_agent.as_str(),
        model_config.computer_use_agent.as_str(),
    ]
    .into_iter()
    .find(|model| !model.is_empty())
    .map(ToOwned::to_owned)
}

fn warp_tool_instructions() -> &'static str {
    "Tool mapping:\n\
     - Use run_shell_command for terminal commands. Prefer read-only commands unless mutation is required.\n\
     - When run_shell_command returns a long-running snapshot, use read_shell_command_output with the returned command_id to monitor it. Use delay_seconds for periodic checks and wait_until_complete when you need the final result.\n\
     - Use write_to_lrc only when a running command needs terminal input. Use transfer_shell_command_control when the command requires secret, interactive, or human-only input.\n\
     - Use read_files, grep, file_glob_v2, and search_codebase to gather local project context through Warp's native file/search UI.\n\
     - Use read_skill when the request references a Warp/Codex skill and Warp advertises that tool.\n\
     - Use ask_user_question when progress depends on a user choice. Keep questions concrete and options short.\n\
     - Use apply_file_diffs for file edits. Prefer v4a_updates when possible; otherwise use exact search/replace diffs.\n\
     - Use suggest_prompt for passive prompt chips or inline query banners when Warp asks for passive suggestions.\n\
     - Use suggest_rule only for durable rules the user may want to save for future agent runs.\n\
     - For SSH checks, prefer absolute key paths when the working directory is known, IdentitiesOnly=yes, BatchMode=yes, StrictHostKeyChecking=accept-new, and a short ConnectTimeout. If SSH says to log in as another user, retry once with that user and summarize both attempts.\n\
     - Do not claim a command ran or a file changed until Warp returns the tool result."
}

fn build_tool_definitions(request: &api::Request) -> Vec<OpenAITool> {
    let mut tools = Vec::new();
    if supports_tool(request, api::ToolType::RunShellCommand) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "run_shell_command",
                description: "Ask Warp to run a shell command in the user's terminal.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["command"],
                    "properties": {
                        "command": { "type": "string" },
                        "is_read_only": { "type": "boolean", "default": false },
                        "uses_pager": { "type": "boolean", "default": false },
                        "is_risky": { "type": "boolean", "default": false },
                        "wait_until_complete": { "type": "boolean", "default": true }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::ReadShellCommandOutput) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "read_shell_command_output",
                description: "Read output from a long-running shell command that Warp is monitoring.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["command_id"],
                    "properties": {
                        "command_id": {
                            "type": "string",
                            "description": "The command_id returned by a long-running command snapshot."
                        },
                        "delay_seconds": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Optional delay before reading another snapshot."
                        },
                        "wait_until_complete": {
                            "type": "boolean",
                            "default": false,
                            "description": "Wait for command completion instead of taking a timed snapshot."
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::WriteToLongRunningShellCommand) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "write_to_lrc",
                description: "Write input to a long-running foreground command in Warp's terminal.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["command_id", "input"],
                    "properties": {
                        "command_id": {
                            "type": "string",
                            "description": "The command_id returned by a long-running command snapshot."
                        },
                        "input": {
                            "type": "string",
                            "description": "The exact text or bytes to send to the terminal command."
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["raw", "line", "block"],
                            "default": "raw"
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::TransferShellCommandControlToUser) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "transfer_shell_command_control",
                description: "Transfer control of a running terminal command back to the user.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["reason"],
                    "properties": {
                        "reason": {
                            "type": "string",
                            "description": "A concise explanation of why the user must take over."
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::ReadFiles) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "read_files",
                description:
                    "Ask Warp to read one or more local files, optionally limited to line ranges.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["files"],
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["path"],
                                "properties": {
                                    "path": { "type": "string" },
                                    "line_ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["start", "end"],
                                            "properties": {
                                                "start": { "type": "integer", "minimum": 1 },
                                                "end": { "type": "integer", "minimum": 1 }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::SearchCodebase) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "search_codebase",
                description:
                    "Ask Warp to search the current indexed codebase for relevant file snippets.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "path_filters": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "codebase_path": {
                            "type": "string",
                            "description": "Optional absolute path to the codebase to search."
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::Grep) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "grep",
                description: "Ask Warp to grep local files for one or more literal strings or regex patterns.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": { "type": "string" },
                        "queries": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "path": {
                            "type": "string",
                            "description": "Relative file or directory to search. Empty means current project."
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::FileGlobV2) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "file_glob_v2",
                description: "Ask Warp to find files by filename glob patterns.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["patterns"],
                    "properties": {
                        "patterns": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "search_dir": {
                            "type": "string",
                            "description": "Relative directory to search. Empty means current project."
                        },
                        "max_matches": { "type": "integer", "minimum": 0 },
                        "max_depth": { "type": "integer", "minimum": 0 },
                        "min_depth": { "type": "integer", "minimum": 0 }
                    }
                }),
            },
        });
    } else if supports_tool(request, api::ToolType::FileGlob) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "file_glob",
                description: "Ask Warp to find files by filename glob patterns.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["patterns"],
                    "properties": {
                        "patterns": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "path": {
                            "type": "string",
                            "description": "Relative directory to search. Empty means current project."
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::ReadSkill) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "read_skill",
                description: "Ask Warp to read a skill by local SKILL.md path or bundled skill id.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "skill_path": { "type": "string" },
                        "bundled_skill_id": { "type": "string" },
                        "name": { "type": "string" }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::AskUserQuestion) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "ask_user_question",
                description: "Ask Warp to show one or more multiple-choice questions to the user.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["questions"],
                    "properties": {
                        "questions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["question", "options"],
                                "properties": {
                                    "question_id": { "type": "string" },
                                    "question": { "type": "string" },
                                    "options": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "recommended_option_index": {
                                        "type": "integer",
                                        "description": "Zero-based index. Use -1 for no recommendation."
                                    },
                                    "is_multiselect": { "type": "boolean", "default": false },
                                    "supports_other": { "type": "boolean", "default": false }
                                }
                            }
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::ApplyFileDiffs) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "apply_file_diffs",
                description: "Ask Warp to apply file edits, creates, deletes, or V4A hunks.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "summary": { "type": "string" },
                        "diffs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["file_path", "search", "replace"],
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "search": { "type": "string" },
                                    "replace": { "type": "string" }
                                }
                            }
                        },
                        "new_files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["file_path", "content"],
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "content": { "type": "string" }
                                }
                            }
                        },
                        "deleted_files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["file_path"],
                                "properties": {
                                    "file_path": { "type": "string" }
                                }
                            }
                        },
                        "v4a_updates": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["file_path", "hunks"],
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "move_to": { "type": "string" },
                                    "hunks": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "change_context": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                },
                                                "pre_context": { "type": "string" },
                                                "old": { "type": "string" },
                                                "new": { "type": "string" },
                                                "post_context": { "type": "string" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }),
            },
        });
    }
    if supports_tool(request, api::ToolType::SuggestPrompt) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "suggest_prompt",
                description: "Ask Warp to show a passive Agent Mode prompt suggestion.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["prompt"],
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The Agent Mode query to run if the user accepts the suggestion."
                        },
                        "label": {
                            "type": "string",
                            "description": "Short optional chip label shown to the user."
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["prompt_chip", "inline_query_banner"],
                            "default": "prompt_chip"
                        },
                        "title": {
                            "type": "string",
                            "description": "Inline banner title. Only used for inline_query_banner mode."
                        },
                        "description": {
                            "type": "string",
                            "description": "Inline banner description. Only used for inline_query_banner mode."
                        },
                        "is_trigger_irrelevant": {
                            "type": "boolean",
                            "default": false
                        }
                    }
                }),
            },
        });
    }
    if should_offer_suggest_rule(request) {
        tools.push(OpenAITool {
            r#type: "function",
            function: OpenAIFunctionDefinition {
                name: "suggest_rule",
                description: "Suggest a durable local WarpOCA rule for future agent runs. Use sparingly, only when a stable preference or project guideline was learned.",
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "content"],
                    "properties": {
                        "name": { "type": "string" },
                        "content": { "type": "string" },
                        "logging_id": {
                            "type": "string",
                            "description": "Optional stable unique id for this suggestion."
                        }
                    }
                }),
            },
        });
    }
    tools
}

fn supports_tool(request: &api::Request, tool: api::ToolType) -> bool {
    let Some(settings) = &request.settings else {
        return true;
    };
    settings.supported_tools.is_empty() || settings.supported_tools.contains(&(tool as i32))
}

fn should_offer_suggest_rule(request: &api::Request) -> bool {
    !request.input.as_ref().is_some_and(|input| {
        matches!(
            input.r#type,
            Some(api::request::input::Type::GeneratePassiveSuggestions(_))
        )
    })
}

fn append_history_message(messages: &mut Vec<OpenAIChatMessage>, message: &api::Message) {
    let Some(message_type) = &message.message else {
        return;
    };

    match message_type {
        api::message::Message::UserQuery(query) => {
            messages.push(OpenAIChatMessage::user(format_user_query(
                &query.query,
                query.context.as_ref(),
            )));
        }
        api::message::Message::SystemQuery(query) => {
            messages.push(OpenAIChatMessage::system(format_system_query(query)));
        }
        api::message::Message::AgentOutput(output) => {
            if !output.text.is_empty() {
                messages.push(OpenAIChatMessage::assistant(output.text.clone()));
            }
        }
        api::message::Message::AgentReasoning(reasoning) => {
            if !reasoning.reasoning.is_empty() {
                messages.push(OpenAIChatMessage::assistant(format!(
                    "[reasoning]\n{}",
                    reasoning.reasoning
                )));
            }
        }
        api::message::Message::ToolCall(tool_call) => {
            if let Some(openai_tool_call) = warp_tool_call_to_openai(tool_call) {
                messages.push(OpenAIChatMessage::assistant_tool_call(openai_tool_call));
            }
        }
        api::message::Message::ToolCallResult(result) => {
            messages.push(OpenAIChatMessage::tool(
                result.tool_call_id.clone(),
                format_tool_call_result(result),
            ));
        }
        api::message::Message::Summarization(summary) => {
            messages.push(OpenAIChatMessage::system(format!(
                "Conversation summary: {}",
                format_summarization(summary)
            )));
        }
        _ => {}
    }
}

#[allow(deprecated)]
fn append_request_input_messages(
    messages: &mut Vec<OpenAIChatMessage>,
    input: &api::request::Input,
    request: &api::Request,
) {
    let Some(input_type) = &input.r#type else {
        return;
    };
    match input_type {
        api::request::input::Type::UserInputs(user_inputs) => {
            for input in &user_inputs.inputs {
                let Some(user_input) = &input.input else {
                    continue;
                };
                match user_input {
                    api::request::input::user_inputs::user_input::Input::UserQuery(query) => {
                        messages.push(OpenAIChatMessage::user(format_user_query(
                            &query.query,
                            input_context_from_request_input(input),
                        )));
                    }
                    api::request::input::user_inputs::user_input::Input::CliAgentUserQuery(query) => {
                        let user_query = query
                            .user_query
                            .as_ref()
                            .map(|q| q.query.as_str())
                            .unwrap_or_default();
                        let running = query
                            .running_command
                            .as_ref()
                            .map(format_running_command)
                            .unwrap_or_default();
                        messages.push(OpenAIChatMessage::user(format!(
                            "{user_query}\n\nActive command context:\n{running}"
                        )));
                    }
                    api::request::input::user_inputs::user_input::Input::ToolCallResult(result) => {
                        messages.push(OpenAIChatMessage::tool(
                            result.tool_call_id.clone(),
                            format_request_tool_call_result(result),
                        ));
                    }
                    api::request::input::user_inputs::user_input::Input::MessagesReceivedFromAgents(received) => {
                        messages.push(OpenAIChatMessage::system(format!(
                            "Messages received from agents:\n{}",
                            received
                                .messages
                                .iter()
                                .map(|message| format!(
                                    "- from {} to {:?}: {}\n{}",
                                    message.sender_agent_id,
                                    message.addresses,
                                    message.subject,
                                    message.message_body
                                ))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )));
                    }
                    api::request::input::user_inputs::user_input::Input::EventsFromAgents(events) => {
                        messages.push(OpenAIChatMessage::system(format!(
                            "Received {} agent lifecycle event(s).",
                            events.agent_events.len()
                        )));
                    }
                    api::request::input::user_inputs::user_input::Input::PassiveSuggestionResult(_) => {
                        messages.push(OpenAIChatMessage::system(
                            "The user accepted or rejected a passive suggestion."
                        ));
                    }
                    _ => {
                        messages.push(OpenAIChatMessage::system(
                            "Received a Warp input type that this BYOB proxy does not yet translate explicitly.",
                        ));
                    }
                }
            }
        }
        api::request::input::Type::QueryWithCannedResponse(query) => {
            messages.push(OpenAIChatMessage::user(query.query.clone()));
        }
        api::request::input::Type::AutoCodeDiffQuery(query) => {
            messages.push(OpenAIChatMessage::user(query.query.clone()));
        }
        api::request::input::Type::ResumeConversation(_) => {
            messages.push(OpenAIChatMessage::user("Resume the conversation."));
        }
        api::request::input::Type::InitProjectRules(_) => {
            messages.push(OpenAIChatMessage::user(
                "Generate or update local project rules for this repository. \
                 Use apply_file_diffs to create or edit WARP.md/AGENTS.md rather than only describing the rule text.",
            ));
        }
        api::request::input::Type::GeneratePassiveSuggestions(_) => {
            let mut instruction = String::from(
                "Warp is asking for a passive suggestion based on recent terminal context. \
                 If no suggestion is genuinely useful, return no text and call no tools.",
            );
            if supports_tool(request, api::ToolType::SuggestPrompt) {
                instruction.push_str(
                    " If a follow-up Agent Mode prompt would help, call suggest_prompt exactly once. \
                     Prefer prompt_chip mode with a concise label.",
                );
            }
            if supports_tool(request, api::ToolType::ApplyFileDiffs) {
                instruction.push_str(
                    " If a small code-edit banner would be clearly useful, call apply_file_diffs exactly once.",
                );
            }
            instruction.push_str(" Do not answer with explanatory prose for passive suggestions.");
            messages.push(OpenAIChatMessage::user(instruction));
        }
        api::request::input::Type::CreateNewProject(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Create a new project: {}",
                query.query
            )));
        }
        api::request::input::Type::CloneRepository(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Clone and set up this repository: {}",
                query.url
            )));
        }
        api::request::input::Type::CodeReview(_) => {
            messages.push(OpenAIChatMessage::user("Review the provided code changes."));
        }
        api::request::input::Type::SummarizeConversation(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Summarize this conversation. {}",
                query.prompt
            )));
        }
        api::request::input::Type::CreateEnvironment(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Create a development environment for: {}",
                query.repo_paths.join(", ")
            )));
        }
        api::request::input::Type::FetchReviewComments(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Fetch review comments for {}.",
                query.repo_path
            )));
        }
        api::request::input::Type::StartFromAmbientRunPrompt(query) => {
            messages.push(OpenAIChatMessage::user(format!(
                "Start ambient run {}.",
                query.ambient_run_id
            )));
        }
        api::request::input::Type::InvokeSkill(query) => {
            let user_query = query
                .user_query
                .as_ref()
                .map(|q| q.query.as_str())
                .unwrap_or("Invoke the provided skill.");
            messages.push(OpenAIChatMessage::user(user_query.to_string()));
        }
        api::request::input::Type::UserQuery(query) => {
            messages.push(OpenAIChatMessage::user(query.query.clone()));
        }
        api::request::input::Type::ToolCallResult(result) => {
            messages.push(OpenAIChatMessage::tool(
                result.tool_call_id.clone(),
                format_request_tool_call_result(result),
            ));
        }
    }
}

fn input_context_from_request_input(
    input: &api::request::input::user_inputs::UserInput,
) -> Option<&api::InputContext> {
    let _ = input;
    None
}

fn format_user_query(query: &str, context: Option<&api::InputContext>) -> String {
    let context = context.map(format_input_context).unwrap_or_default();
    if context.is_empty() {
        query.to_string()
    } else {
        format!("{query}\n\nContext:\n{context}")
    }
}

#[allow(deprecated)]
fn format_input_context(context: &api::InputContext) -> String {
    let mut lines = Vec::new();
    if let Some(directory) = &context.directory {
        if !directory.pwd.is_empty() {
            lines.push(format!("pwd: {}", directory.pwd));
        }
        if !directory.home.is_empty() {
            lines.push(format!("home: {}", directory.home));
        }
    }
    if let Some(os) = &context.operating_system {
        if !os.platform.is_empty() || !os.distribution.is_empty() {
            lines.push(
                format!("os: {} {}", os.platform, os.distribution)
                    .trim()
                    .to_string(),
            );
        }
    }
    if let Some(shell) = &context.shell {
        if !shell.name.is_empty() {
            lines.push(
                format!("shell: {} {}", shell.name, shell.version)
                    .trim()
                    .to_string(),
            );
        }
    }
    if let Some(git) = &context.git {
        if !git.head.is_empty() || !git.branch.is_empty() {
            lines.push(format!("git: head={} branch={}", git.head, git.branch));
        }
    }
    for codebase in &context.codebases {
        lines.push(format!("codebase: {} ({})", codebase.name, codebase.path));
    }
    for command in context.executed_shell_commands.iter().rev().take(5).rev() {
        lines.push(format!(
            "recent command [{}]: {}\nexit: {}\n{}",
            command.command_id,
            command.command,
            command.exit_code,
            truncate(&command.output, 4000)
        ));
    }
    for selected in &context.selected_text {
        lines.push(format!(
            "selected text:\n{}",
            truncate(&selected.text, 2000)
        ));
    }
    for file in &context.files {
        if let Some(content) = &file.content {
            lines.push(format!(
                "file: {}\n{}",
                content.file_path,
                truncate(&content.content, 6000)
            ));
        }
    }
    for rules in &context.project_rules {
        for file in &rules.active_rule_files {
            lines.push(format!(
                "project rule {}:\n{}",
                file.file_path,
                truncate(&file.content, 4000)
            ));
        }
    }
    lines.join("\n\n")
}

fn format_system_query(query: &api::message::SystemQuery) -> String {
    match &query.r#type {
        Some(api::message::system_query::Type::AutoCodeDiff(query)) => query.query.clone(),
        Some(api::message::system_query::Type::ResumeConversation(_)) => {
            "Resume the conversation.".to_string()
        }
        Some(api::message::system_query::Type::GeneratePassiveSuggestions(_)) => {
            "Generate passive suggestions.".to_string()
        }
        Some(api::message::system_query::Type::CreateNewProject(query)) => query.query.clone(),
        Some(api::message::system_query::Type::CloneRepository(query)) => {
            format!("Clone repository: {}", query.url)
        }
        Some(api::message::system_query::Type::SummarizeConversation(query)) => {
            format!("Summarize conversation: {}", query.prompt)
        }
        Some(api::message::system_query::Type::FetchReviewComments(query)) => {
            format!("Fetch review comments: {}", query.repo_path)
        }
        Some(_) | None => "System query.".to_string(),
    }
}

fn format_summarization(summary: &api::message::Summarization) -> String {
    match &summary.summary_type {
        Some(api::message::summarization::SummaryType::ConversationSummary(summary)) => {
            summary.summary.clone()
        }
        Some(api::message::summarization::SummaryType::ToolCallResultSummary(_)) => {
            "A prior tool call result was summarized.".to_string()
        }
        None => String::new(),
    }
}

fn format_running_command(command: &api::RunningShellCommand) -> String {
    let snapshot = command
        .snapshot
        .as_ref()
        .map(|snapshot| {
            format!(
                "command_id: {}\noutput:\n{}",
                snapshot.command_id,
                truncate(&snapshot.output, 4000)
            )
        })
        .unwrap_or_default();
    format!("command: {}\n{snapshot}", command.command)
}

#[allow(deprecated)]
fn warp_tool_call_to_openai(tool_call: &api::message::ToolCall) -> Option<OpenAIMessageToolCall> {
    let (name, arguments) = match tool_call.tool.as_ref()? {
        api::message::tool_call::Tool::RunShellCommand(command) => (
            "run_shell_command",
            json!({
                "command": command.command,
                "is_read_only": command.is_read_only,
                "uses_pager": command.uses_pager,
                "is_risky": command.is_risky,
                "wait_until_complete": command.wait_until_complete_value.as_ref().is_none_or(
                    |api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(value)| *value
                ),
            }),
        ),
        api::message::tool_call::Tool::ReadShellCommandOutput(command) => (
            "read_shell_command_output",
            json!({
                "command_id": command.command_id,
                "delay_seconds": read_shell_delay_seconds(command),
                "wait_until_complete": read_shell_waits_until_complete(command),
            }),
        ),
        api::message::tool_call::Tool::WriteToLongRunningShellCommand(command) => (
            "write_to_lrc",
            json!({
                "command_id": command.command_id,
                "input": String::from_utf8_lossy(&command.input).to_string(),
                "mode": write_to_lrc_mode_name(command),
            }),
        ),
        api::message::tool_call::Tool::TransferShellCommandControlToUser(transfer) => (
            "transfer_shell_command_control",
            json!({
                "reason": transfer.reason,
            }),
        ),
        api::message::tool_call::Tool::ReadFiles(read_files) => (
            "read_files",
            json!({
                "files": read_files.files.iter().map(|file| json!({
                    "path": file.name,
                    "line_ranges": file.line_ranges.iter().map(|range| json!({
                        "start": range.start,
                        "end": range.end,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }),
        ),
        api::message::tool_call::Tool::SearchCodebase(search) => (
            "search_codebase",
            json!({
                "query": search.query,
                "path_filters": search.path_filters,
                "codebase_path": search.codebase_path,
            }),
        ),
        api::message::tool_call::Tool::Grep(grep) => (
            "grep",
            json!({
                "queries": grep.queries,
                "path": grep.path,
            }),
        ),
        api::message::tool_call::Tool::FileGlob(glob) => (
            "file_glob",
            json!({
                "patterns": glob.patterns,
                "path": glob.path,
            }),
        ),
        api::message::tool_call::Tool::FileGlobV2(glob) => (
            "file_glob_v2",
            json!({
                "patterns": glob.patterns,
                "search_dir": glob.search_dir,
                "max_matches": glob.max_matches,
                "max_depth": glob.max_depth,
                "min_depth": glob.min_depth,
            }),
        ),
        api::message::tool_call::Tool::ReadSkill(read_skill) => {
            let (skill_path, bundled_skill_id) = match read_skill.skill_reference.as_ref() {
                Some(api::message::tool_call::read_skill::SkillReference::SkillPath(path)) => {
                    (path.clone(), String::new())
                }
                Some(api::message::tool_call::read_skill::SkillReference::BundledSkillId(id)) => {
                    (String::new(), id.clone())
                }
                None => (String::new(), String::new()),
            };
            (
                "read_skill",
                json!({
                    "skill_path": skill_path,
                    "bundled_skill_id": bundled_skill_id,
                    "name": read_skill.name,
                }),
            )
        }
        api::message::tool_call::Tool::AskUserQuestion(ask) => (
            "ask_user_question",
            json!({
                "questions": ask.questions.iter().map(|question| {
                    let multiple_choice = match question.question_type.as_ref() {
                        Some(api::ask_user_question::question::QuestionType::MultipleChoice(choice)) => Some(choice),
                        None => None,
                    };
                    json!({
                        "question_id": question.question_id,
                        "question": question.question,
                        "options": multiple_choice
                            .map(|choice| choice.options.iter().map(|option| option.label.clone()).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        "recommended_option_index": multiple_choice.map(|choice| choice.recommended_option_index).unwrap_or(-1),
                        "is_multiselect": multiple_choice.map(|choice| choice.is_multiselect).unwrap_or(false),
                        "supports_other": multiple_choice.map(|choice| choice.supports_other).unwrap_or(false),
                    })
                }).collect::<Vec<_>>(),
            }),
        ),
        api::message::tool_call::Tool::ApplyFileDiffs(diffs) => (
            "apply_file_diffs",
            json!({
                "summary": diffs.summary,
                "diffs": diffs.diffs.iter().map(|diff| json!({
                    "file_path": diff.file_path,
                    "search": diff.search,
                    "replace": diff.replace,
                })).collect::<Vec<_>>(),
                "new_files": diffs.new_files.iter().map(|file| json!({
                    "file_path": file.file_path,
                    "content": file.content,
                })).collect::<Vec<_>>(),
                "deleted_files": diffs.deleted_files.iter().map(|file| json!({
                    "file_path": file.file_path,
                })).collect::<Vec<_>>(),
            }),
        ),
        _ => return None,
    };

    Some(OpenAIMessageToolCall {
        id: tool_call.tool_call_id.clone(),
        r#type: "function".to_string(),
        function: OpenAIFunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    })
}

fn read_shell_delay_seconds(
    command: &api::message::tool_call::ReadShellCommandOutput,
) -> Option<i64> {
    match &command.delay {
        Some(api::message::tool_call::read_shell_command_output::Delay::Duration(duration)) => {
            Some(duration.seconds)
        }
        _ => None,
    }
}

fn read_shell_waits_until_complete(
    command: &api::message::tool_call::ReadShellCommandOutput,
) -> bool {
    matches!(
        command.delay,
        Some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(_))
    )
}

fn write_to_lrc_mode_name(
    command: &api::message::tool_call::WriteToLongRunningShellCommand,
) -> &'static str {
    use api::message::tool_call::write_to_long_running_shell_command::mode::Mode;

    match command.mode.as_ref().and_then(|mode| mode.mode.as_ref()) {
        Some(Mode::Line(_)) => "line",
        Some(Mode::Block(_)) => "block",
        Some(Mode::Raw(_)) | None => "raw",
    }
}

#[allow(deprecated)]
fn format_tool_call_result(result: &api::message::ToolCallResult) -> String {
    match &result.result {
        Some(api::message::tool_call_result::Result::RunShellCommand(result)) => {
            format_run_shell_result(result)
        }
        Some(api::message::tool_call_result::Result::ReadShellCommandOutput(result)) => {
            format_read_shell_output_result(result)
        }
        Some(api::message::tool_call_result::Result::WriteToLongRunningShellCommand(result)) => {
            format_write_to_lrc_result(result)
        }
        Some(api::message::tool_call_result::Result::TransferShellCommandControlToUser(result)) => {
            format_transfer_shell_control_result(result)
        }
        Some(api::message::tool_call_result::Result::ReadFiles(result)) => {
            format_read_files_result(result)
        }
        Some(api::message::tool_call_result::Result::SearchCodebase(result)) => {
            format_search_codebase_result(result)
        }
        Some(api::message::tool_call_result::Result::Grep(result)) => format_grep_result(result),
        Some(api::message::tool_call_result::Result::FileGlob(result)) => {
            format_file_glob_result(result)
        }
        Some(api::message::tool_call_result::Result::FileGlobV2(result)) => {
            format_file_glob_v2_result(result)
        }
        Some(api::message::tool_call_result::Result::ReadSkill(result)) => {
            format_read_skill_result(result)
        }
        Some(api::message::tool_call_result::Result::AskUserQuestion(result)) => {
            format_ask_user_question_result(result)
        }
        Some(api::message::tool_call_result::Result::ApplyFileDiffs(result)) => {
            format_apply_file_diffs_result(result)
        }
        Some(other) => format!("Tool result: {other:?}"),
        None => "Tool returned no result.".to_string(),
    }
}

fn format_request_tool_call_result(result: &api::request::input::ToolCallResult) -> String {
    match &result.result {
        Some(api::request::input::tool_call_result::Result::RunShellCommand(result)) => {
            format_run_shell_result(result)
        }
        Some(api::request::input::tool_call_result::Result::ReadShellCommandOutput(result)) => {
            format_read_shell_output_result(result)
        }
        Some(api::request::input::tool_call_result::Result::WriteToLongRunningShellCommand(
            result,
        )) => format_write_to_lrc_result(result),
        Some(api::request::input::tool_call_result::Result::TransferShellCommandControlToUser(
            result,
        )) => format_transfer_shell_control_result(result),
        Some(api::request::input::tool_call_result::Result::ReadFiles(result)) => {
            format_read_files_result(result)
        }
        Some(api::request::input::tool_call_result::Result::SearchCodebase(result)) => {
            format_search_codebase_result(result)
        }
        Some(api::request::input::tool_call_result::Result::Grep(result)) => {
            format_grep_result(result)
        }
        Some(api::request::input::tool_call_result::Result::FileGlob(result)) => {
            format_file_glob_result(result)
        }
        Some(api::request::input::tool_call_result::Result::FileGlobV2(result)) => {
            format_file_glob_v2_result(result)
        }
        Some(api::request::input::tool_call_result::Result::ReadSkill(result)) => {
            format_read_skill_result(result)
        }
        Some(api::request::input::tool_call_result::Result::AskUserQuestion(result)) => {
            format_ask_user_question_result(result)
        }
        Some(api::request::input::tool_call_result::Result::ApplyFileDiffs(result)) => {
            format_apply_file_diffs_result(result)
        }
        Some(other) => format!("Tool result: {other:?}"),
        None => "Tool returned no result.".to_string(),
    }
}

#[allow(deprecated)]
fn format_run_shell_result(result: &api::RunShellCommandResult) -> String {
    match &result.result {
        Some(api::run_shell_command_result::Result::CommandFinished(finished)) => format!(
            "command: {}\ncommand_id: {}\nexit_code: {}\noutput:\n{}",
            result.command,
            finished.command_id,
            finished.exit_code,
            truncate(&finished.output, 12000)
        ),
        Some(api::run_shell_command_result::Result::LongRunningCommandSnapshot(snapshot)) => {
            format!(
                "command: {}\n{}",
                result.command,
                format_long_running_snapshot(snapshot)
            )
        }
        Some(api::run_shell_command_result::Result::PermissionDenied(_)) => {
            format!("command denied by user or policy: {}", result.command)
        }
        None => format!(
            "command: {}\nexit_code: {}\noutput:\n{}",
            result.command,
            result.exit_code,
            truncate(&result.output, 12000)
        ),
    }
}

fn format_read_shell_output_result(result: &api::ReadShellCommandOutputResult) -> String {
    match &result.result {
        Some(api::read_shell_command_output_result::Result::CommandFinished(finished)) => {
            format_shell_finished(Some(&result.command), finished)
        }
        Some(api::read_shell_command_output_result::Result::LongRunningCommandSnapshot(
            snapshot,
        )) => {
            format!(
                "command: {}\n{}",
                result.command,
                format_long_running_snapshot(snapshot)
            )
        }
        Some(api::read_shell_command_output_result::Result::Error(error)) => {
            format_shell_error("read_shell_command_output", error)
        }
        None => format!(
            "command: {}\nread_shell_command_output returned no result.",
            result.command
        ),
    }
}

fn format_write_to_lrc_result(result: &api::WriteToLongRunningShellCommandResult) -> String {
    match &result.result {
        Some(api::write_to_long_running_shell_command_result::Result::CommandFinished(
            finished,
        )) => format_shell_finished(None, finished),
        Some(
            api::write_to_long_running_shell_command_result::Result::LongRunningCommandSnapshot(
                snapshot,
            ),
        ) => format_long_running_snapshot(snapshot),
        Some(api::write_to_long_running_shell_command_result::Result::Error(error)) => {
            format_shell_error("write_to_lrc", error)
        }
        None => "write_to_lrc returned no result.".to_string(),
    }
}

fn format_transfer_shell_control_result(
    result: &api::TransferShellCommandControlToUserResult,
) -> String {
    match &result.result {
        Some(api::transfer_shell_command_control_to_user_result::Result::CommandFinished(
            finished,
        )) => format_shell_finished(None, finished),
        Some(
            api::transfer_shell_command_control_to_user_result::Result::LongRunningCommandSnapshot(
                snapshot,
            ),
        ) => format_long_running_snapshot(snapshot),
        Some(api::transfer_shell_command_control_to_user_result::Result::Error(error)) => {
            format_shell_error("transfer_shell_command_control", error)
        }
        None => "transfer_shell_command_control returned no result.".to_string(),
    }
}

fn format_read_files_result(result: &api::ReadFilesResult) -> String {
    match &result.result {
        Some(api::read_files_result::Result::TextFilesSuccess(success)) => {
            format_file_contents("read_files", success.files.iter())
        }
        Some(api::read_files_result::Result::AnyFilesSuccess(success)) => {
            let files = success
                .files
                .iter()
                .map(format_any_file_content)
                .collect::<Vec<_>>()
                .join("\n\n");
            if files.is_empty() {
                "read_files returned no files.".to_string()
            } else {
                format!("read_files result:\n{files}")
            }
        }
        Some(api::read_files_result::Result::Error(error)) => {
            format!("read_files failed: {}", error.message)
        }
        None => "read_files returned no result.".to_string(),
    }
}

fn format_search_codebase_result(result: &api::SearchCodebaseResult) -> String {
    match &result.result {
        Some(api::search_codebase_result::Result::Success(success)) => {
            format_file_contents("search_codebase", success.files.iter())
        }
        Some(api::search_codebase_result::Result::Error(error)) => {
            format!("search_codebase failed: {}", error.message)
        }
        None => "search_codebase returned no result.".to_string(),
    }
}

fn format_grep_result(result: &api::GrepResult) -> String {
    match &result.result {
        Some(api::grep_result::Result::Success(success)) => {
            let files = success
                .matched_files
                .iter()
                .map(|file| {
                    let lines = file
                        .matched_lines
                        .iter()
                        .map(|line| line.line_number.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{}: lines [{}]", file.file_path, lines)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if files.is_empty() {
                "grep found no matches.".to_string()
            } else {
                format!("grep matches:\n{files}")
            }
        }
        Some(api::grep_result::Result::Error(error)) => {
            format!("grep failed: {}", error.message)
        }
        None => "grep returned no result.".to_string(),
    }
}

fn format_file_glob_result(result: &api::FileGlobResult) -> String {
    match &result.result {
        Some(api::file_glob_result::Result::Success(success)) => {
            format!("file_glob matches:\n{}", success.matched_files)
        }
        Some(api::file_glob_result::Result::Error(error)) => {
            format!("file_glob failed: {}", error.message)
        }
        None => "file_glob returned no result.".to_string(),
    }
}

fn format_file_glob_v2_result(result: &api::FileGlobV2Result) -> String {
    match &result.result {
        Some(api::file_glob_v2_result::Result::Success(success)) => {
            let files = success
                .matched_files
                .iter()
                .map(|file| file.file_path.clone())
                .collect::<Vec<_>>()
                .join("\n");
            let warnings = if success.warnings.trim().is_empty() {
                String::new()
            } else {
                format!("\nwarnings:\n{}", success.warnings)
            };
            if files.is_empty() {
                format!("file_glob_v2 found no matches.{warnings}")
            } else {
                format!("file_glob_v2 matches:\n{files}{warnings}")
            }
        }
        Some(api::file_glob_v2_result::Result::Error(error)) => {
            format!("file_glob_v2 failed: {}", error.message)
        }
        None => "file_glob_v2 returned no result.".to_string(),
    }
}

fn format_read_skill_result(result: &api::ReadSkillResult) -> String {
    match &result.result {
        Some(api::read_skill_result::Result::Success(success)) => success
            .content
            .as_ref()
            .map(|content| format!("read_skill result:\n{}", format_file_content(content)))
            .unwrap_or_else(|| "read_skill returned success without content.".to_string()),
        Some(api::read_skill_result::Result::Error(error)) => {
            format!("read_skill failed: {}", error.message)
        }
        None => "read_skill returned no result.".to_string(),
    }
}

fn format_ask_user_question_result(result: &api::AskUserQuestionResult) -> String {
    match &result.result {
        Some(api::ask_user_question_result::Result::Success(success)) => {
            let answers = success
                .answers
                .iter()
                .map(format_answer_item)
                .collect::<Vec<_>>()
                .join("\n");
            if answers.is_empty() {
                "ask_user_question completed with no answers.".to_string()
            } else {
                format!("ask_user_question answers:\n{answers}")
            }
        }
        Some(api::ask_user_question_result::Result::Error(error)) => {
            format!("ask_user_question failed: {}", error.message)
        }
        None => "ask_user_question returned no result.".to_string(),
    }
}

fn format_file_contents<'a>(
    tool_name: &str,
    files: impl Iterator<Item = &'a api::FileContent>,
) -> String {
    let files = files
        .map(format_file_content)
        .collect::<Vec<_>>()
        .join("\n\n");
    if files.is_empty() {
        format!("{tool_name} returned no files.")
    } else {
        format!("{tool_name} result:\n{files}")
    }
}

fn format_any_file_content(content: &api::AnyFileContent) -> String {
    match content.content.as_ref() {
        Some(api::any_file_content::Content::TextContent(file)) => format_file_content(file),
        Some(api::any_file_content::Content::BinaryContent(file)) => {
            format!(
                "binary file: {}\nbytes: {}",
                file.file_path,
                file.data.len()
            )
        }
        None => "empty file content".to_string(),
    }
}

fn format_file_content(file: &api::FileContent) -> String {
    let range = file
        .line_range
        .as_ref()
        .map(|range| format!(" lines {}-{}", range.start, range.end))
        .unwrap_or_default();
    format!(
        "file: {}{}\n{}",
        file.file_path,
        range,
        truncate(&file.content, 12000)
    )
}

fn format_answer_item(answer: &api::ask_user_question_result::AnswerItem) -> String {
    match answer.answer.as_ref() {
        Some(api::ask_user_question_result::answer_item::Answer::MultipleChoice(choice)) => {
            let selected = choice.selected_options.join(", ");
            if choice.other_text.trim().is_empty() {
                format!("{}: {}", answer.question_id, selected)
            } else {
                format!(
                    "{}: {} other={}",
                    answer.question_id, selected, choice.other_text
                )
            }
        }
        Some(api::ask_user_question_result::answer_item::Answer::Skipped(_)) => {
            format!("{}: skipped", answer.question_id)
        }
        None => format!("{}: no answer", answer.question_id),
    }
}

fn format_shell_finished(command: Option<&str>, finished: &api::ShellCommandFinished) -> String {
    let command = command
        .filter(|command| !command.is_empty())
        .map(|command| format!("command: {command}\n"))
        .unwrap_or_default();
    format!(
        "{command}command_id: {}\nexit_code: {}\noutput:\n{}",
        finished.command_id,
        finished.exit_code,
        truncate(&finished.output, 12000)
    )
}

fn format_long_running_snapshot(snapshot: &api::LongRunningShellCommandSnapshot) -> String {
    format!(
        "command_id: {}\nstatus: still running\nalt_screen: {}\npreempted: {}\noutput:\n{}",
        snapshot.command_id,
        snapshot.is_alt_screen_active,
        snapshot.is_preempted,
        truncate(&snapshot.output, 12000)
    )
}

fn format_shell_error(tool_name: &str, error: &api::ShellCommandError) -> String {
    format!("{tool_name} failed: {error:?}")
}

fn format_apply_file_diffs_result(result: &api::ApplyFileDiffsResult) -> String {
    match &result.result {
        Some(api::apply_file_diffs_result::Result::Success(success)) => {
            let updated = success
                .updated_files_v2
                .iter()
                .filter_map(|file| file.file.as_ref())
                .map(|file| file.file_path.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let deleted = success
                .deleted_files
                .iter()
                .map(|file| file.file_path.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("file edits applied. updated: [{updated}] deleted: [{deleted}]")
        }
        Some(api::apply_file_diffs_result::Result::Error(error)) => {
            format!("file edits failed: {}", error.message)
        }
        None => "file edit result was empty".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIChatChunk {
    #[serde(default)]
    choices: Vec<OpenAIChatChunkChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatChunkChoice {
    #[serde(default)]
    delta: OpenAIChatDelta,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAIChatDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCallDelta {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    r#type: Option<String>,
    function: Option<OpenAIFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponse {
    #[serde(default)]
    choices: Vec<OpenAIChatResponseChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponseChoice {
    message: OpenAIChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIMessageToolCall>>,
}

fn parse_backend_sse_data(
    wire_api: WireApi,
    data: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
    include_completed_text: bool,
) -> anyhow::Result<Vec<String>> {
    match wire_api {
        WireApi::ChatCompletions => parse_chat_sse_data(data, tool_calls),
        WireApi::Responses => parse_responses_sse_data(data, tool_calls, include_completed_text),
    }
}

fn parse_backend_full_response(
    wire_api: WireApi,
    body: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
    include_completed_text: bool,
) -> anyhow::Result<Vec<String>> {
    match wire_api {
        WireApi::ChatCompletions => parse_chat_full_response(body, tool_calls),
        WireApi::Responses => {
            parse_responses_full_response(body, tool_calls, include_completed_text)
        }
    }
}

fn parse_chat_sse_data(
    data: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
) -> anyhow::Result<Vec<String>> {
    let chunk = serde_json::from_str::<OpenAIChatChunk>(data)?;
    let mut text_deltas = Vec::new();
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            text_deltas.push(content);
        }
        if let Some(reasoning) = choice.delta.reasoning_content {
            text_deltas.push(format_reasoning_delta(&reasoning));
        }
        if let Some(delta_tool_calls) = choice.delta.tool_calls {
            tool_calls.apply_deltas(delta_tool_calls);
        }
    }
    Ok(text_deltas)
}

fn parse_chat_full_response(
    body: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
) -> anyhow::Result<Vec<String>> {
    let full_response = serde_json::from_str::<OpenAIChatResponse>(body)?;
    let mut text_deltas = Vec::new();
    for choice in full_response.choices {
        if let Some(content) = choice.message.content {
            text_deltas.push(content);
        }
        if let Some(full_tool_calls) = choice.message.tool_calls {
            tool_calls.apply_full_calls(full_tool_calls);
        }
    }
    Ok(text_deltas)
}

fn parse_responses_sse_data(
    data: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
    include_completed_text: bool,
) -> anyhow::Result<Vec<String>> {
    let event = serde_json::from_str::<Value>(data)?;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "response.output_text.delta" | "response.reasoning_summary_text.delta" => Ok(event
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| vec![delta.to_string()])
            .unwrap_or_default()),
        "response.function_call_arguments.delta" => {
            tool_calls.apply_response_arguments_delta(
                response_output_index(&event),
                event.get("item_id").and_then(Value::as_str),
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            Ok(Vec::new())
        }
        "response.function_call_arguments.done" => {
            if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                tool_calls.apply_response_arguments_done(
                    response_output_index(&event),
                    event.get("item_id").and_then(Value::as_str),
                    arguments,
                );
            }
            Ok(Vec::new())
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                tool_calls.apply_response_output_item(response_output_index(&event), item);
            }
            Ok(Vec::new())
        }
        "response.completed" => {
            if let Some(response) = event.get("response") {
                tool_calls.apply_responses_output(response);
                if include_completed_text {
                    return Ok(responses_output_text(response));
                }
            }
            Ok(Vec::new())
        }
        "response.failed" => {
            let message = event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(error_message_from_value)
                .unwrap_or_else(|| "Responses backend reported failure".to_string());
            anyhow::bail!("{message}");
        }
        "error" => {
            let message = error_message_from_value(&event)
                .unwrap_or_else(|| "Responses backend returned an error event".to_string());
            anyhow::bail!("{message}");
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_responses_full_response(
    body: &str,
    tool_calls: &mut ToolCallAccumulatorSet,
    include_completed_text: bool,
) -> anyhow::Result<Vec<String>> {
    let response = serde_json::from_str::<Value>(body)?;
    tool_calls.apply_responses_output(&response);
    if include_completed_text {
        Ok(responses_output_text(&response))
    } else {
        Ok(Vec::new())
    }
}

fn response_output_index(event: &Value) -> Option<usize> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
}

fn responses_output_text(response: &Value) -> Vec<String> {
    if let Some(text) = response.get("output_text").and_then(Value::as_str) {
        if !text.is_empty() {
            return vec![text.to_string()];
        }
    }

    let mut texts = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content_items) = item.get("content").and_then(Value::as_array) {
                for content in content_items {
                    if matches!(
                        content.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) {
                        if let Some(text) = content
                            .get("text")
                            .or_else(|| content.get("output_text"))
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
        }
    }
    texts
}

fn error_message_from_value(value: &Value) -> Option<String> {
    value
        .get("message")
        .or_else(|| value.get("error").and_then(|error| error.get("message")))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[derive(Default)]
struct ToolCallAccumulatorSet {
    calls: BTreeMap<usize, ToolCallAccumulator>,
}

enum WarpClientOutput {
    ToolCall(api::message::ToolCall),
    Suggestions(api::Suggestions),
}

impl ToolCallAccumulatorSet {
    fn apply_deltas(&mut self, deltas: Vec<OpenAIToolCallDelta>) {
        for delta in deltas {
            let entry = self.calls.entry(delta.index).or_default();
            if let Some(id) = delta.id {
                entry.id = Some(id);
            }
            if let Some(r#type) = delta.r#type {
                entry.r#type = Some(r#type);
            }
            if let Some(function) = delta.function {
                if let Some(name) = function.name {
                    entry.name = Some(name);
                }
                if let Some(arguments) = function.arguments {
                    entry.arguments.push_str(&arguments);
                }
            }
        }
    }

    fn apply_full_calls(&mut self, calls: Vec<OpenAIMessageToolCall>) {
        for (index, call) in calls.into_iter().enumerate() {
            self.calls.insert(
                index,
                ToolCallAccumulator {
                    id: Some(call.id),
                    r#type: Some(call.r#type),
                    name: Some(call.function.name),
                    arguments: call.function.arguments,
                    response_item_id: None,
                },
            );
        }
    }

    fn apply_response_arguments_delta(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        delta: &str,
    ) {
        if delta.is_empty() {
            return;
        }
        self.response_call_mut(output_index, item_id)
            .arguments
            .push_str(delta);
    }

    fn apply_response_arguments_done(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        arguments: &str,
    ) {
        self.response_call_mut(output_index, item_id).arguments = arguments.to_string();
    }

    fn apply_response_output_item(&mut self, output_index: Option<usize>, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }

        let item_id = item.get("id").and_then(Value::as_str);
        let entry = self.response_call_mut(output_index, item_id);
        entry.r#type = Some("function".to_string());
        if let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            entry.id = Some(call_id.to_string());
        }
        if let Some(item_id) = item_id {
            entry.response_item_id = Some(item_id.to_string());
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            entry.name = Some(name.to_string());
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            entry.arguments = arguments.to_string();
        }
    }

    fn apply_responses_output(&mut self, response: &Value) {
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            for (index, item) in output.iter().enumerate() {
                self.apply_response_output_item(Some(index), item);
            }
        }
    }

    fn response_call_mut(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) -> &mut ToolCallAccumulator {
        if let Some(index) = output_index {
            let entry = self.calls.entry(index).or_default();
            if let Some(item_id) = item_id {
                entry.response_item_id = Some(item_id.to_string());
            }
            return entry;
        }

        if let Some(item_id) = item_id {
            if let Some(index) = self.calls.iter().find_map(|(index, call)| {
                (call.response_item_id.as_deref() == Some(item_id)).then_some(*index)
            }) {
                return self.calls.entry(index).or_default();
            }
        }

        let index = self
            .calls
            .keys()
            .next_back()
            .map(|index| index + 1)
            .unwrap_or(0);
        let entry = self.calls.entry(index).or_default();
        if let Some(item_id) = item_id {
            entry.response_item_id = Some(item_id.to_string());
        }
        entry
    }

    #[cfg(test)]
    fn into_warp_tool_calls(self) -> Vec<anyhow::Result<api::message::ToolCall>> {
        self.into_warp_client_outputs()
            .into_iter()
            .filter_map(|result| match result {
                Ok(WarpClientOutput::ToolCall(call)) => Some(Ok(call)),
                Ok(WarpClientOutput::Suggestions(_)) => None,
                Err(err) => Some(Err(err)),
            })
            .collect()
    }

    fn into_warp_client_outputs(self) -> Vec<anyhow::Result<WarpClientOutput>> {
        self.calls
            .into_values()
            .filter(|call| call.name.is_some() || !call.arguments.is_empty())
            .map(ToolCallAccumulator::into_warp_client_output)
            .collect()
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    r#type: Option<String>,
    name: Option<String>,
    arguments: String,
    response_item_id: Option<String>,
}

impl ToolCallAccumulator {
    #[allow(deprecated)]
    fn into_warp_client_output(self) -> anyhow::Result<WarpClientOutput> {
        let name = self
            .name
            .ok_or_else(|| anyhow::anyhow!("missing function name"))?;
        let tool_call_id = self
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("tool_{}", Uuid::new_v4()));

        let tool = match name.as_str() {
            "run_shell_command" => {
                let args: RunShellCommandArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::RunShellCommand(
                    api::message::tool_call::RunShellCommand {
                        command: args.command,
                        is_read_only: args.is_read_only,
                        uses_pager: args.uses_pager,
                        citations: vec![],
                        is_risky: args.is_risky,
                        wait_until_complete_value: Some(
                            api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                                args.wait_until_complete.unwrap_or(true),
                            ),
                        ),
                        ..Default::default()
                    },
                )
            }
            "read_shell_command_output" => {
                let args: ReadShellCommandOutputArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::ReadShellCommandOutput(
                    api::message::tool_call::ReadShellCommandOutput {
                        delay: args.to_delay(),
                        command_id: args.command_id,
                    },
                )
            }
            "write_to_lrc" | "write_to_long_running_shell_command" => {
                let args: WriteToLongRunningShellCommandArgs =
                    serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::WriteToLongRunningShellCommand(
                    api::message::tool_call::WriteToLongRunningShellCommand {
                        mode: Some(args.to_mode()),
                        input: args.input.into_bytes(),
                        command_id: args.command_id,
                    },
                )
            }
            "transfer_shell_command_control" | "transfer_shell_command_control_to_user" => {
                let args: TransferShellCommandControlArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::TransferShellCommandControlToUser(
                    api::message::tool_call::TransferShellCommandControlToUser {
                        reason: args.reason,
                    },
                )
            }
            "read_files" => {
                let args: ReadFilesArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::ReadFiles(api::message::tool_call::ReadFiles {
                    files: args
                        .files
                        .into_iter()
                        .map(|file| {
                            let ReadFileArg { name, line_ranges } = file;
                            api::message::tool_call::read_files::File {
                                name: name.unwrap_or_default(),
                                line_ranges: line_ranges.into_iter().map(Into::into).collect(),
                            }
                        })
                        .collect(),
                })
            }
            "search_codebase" => {
                let args: SearchCodebaseArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::SearchCodebase(
                    api::message::tool_call::SearchCodebase {
                        query: args.query,
                        path_filters: args.path_filters,
                        codebase_path: args.codebase_path.unwrap_or_default(),
                    },
                )
            }
            "grep" => {
                let args: GrepArgs = serde_json::from_str(&self.arguments)?;
                let path = args.path.clone().unwrap_or_default();
                api::message::tool_call::Tool::Grep(api::message::tool_call::Grep {
                    queries: args.queries(),
                    path,
                })
            }
            "file_glob" => {
                let args: FileGlobArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::FileGlob(api::message::tool_call::FileGlob {
                    patterns: args.patterns,
                    path: args.path.unwrap_or_default(),
                })
            }
            "file_glob_v2" => {
                let args: FileGlobV2Args = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
                    patterns: args.patterns,
                    search_dir: args.search_dir.unwrap_or_default(),
                    max_matches: args.max_matches.unwrap_or_default(),
                    max_depth: args.max_depth.unwrap_or_default(),
                    min_depth: args.min_depth.unwrap_or_default(),
                })
            }
            "read_skill" => {
                let args: ReadSkillArgs = serde_json::from_str(&self.arguments)?;
                let name = args.name.clone().unwrap_or_default();
                api::message::tool_call::Tool::ReadSkill(api::message::tool_call::ReadSkill {
                    name,
                    skill_reference: Some(args.skill_reference()?),
                })
            }
            "ask_user_question" => {
                let args: AskUserQuestionArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::AskUserQuestion(api::AskUserQuestion {
                    questions: args.questions.into_iter().map(Into::into).collect(),
                })
            }
            "apply_file_diffs" => {
                let args: ApplyFileDiffsArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::ApplyFileDiffs(
                    api::message::tool_call::ApplyFileDiffs {
                        summary: args.summary.unwrap_or_else(|| "Apply file edits".to_string()),
                        diffs: args
                            .diffs
                            .into_iter()
                            .map(|diff| api::message::tool_call::apply_file_diffs::FileDiff {
                                file_path: diff.file_path,
                                search: diff.search,
                                replace: diff.replace,
                            })
                            .collect(),
                        new_files: args
                            .new_files
                            .into_iter()
                            .map(|file| api::message::tool_call::apply_file_diffs::NewFile {
                                file_path: file.file_path,
                                content: file.content,
                            })
                            .collect(),
                        deleted_files: args
                            .deleted_files
                            .into_iter()
                            .map(|file| api::message::tool_call::apply_file_diffs::DeleteFile {
                                file_path: file.file_path,
                            })
                            .collect(),
                        v4a_updates: args
                            .v4a_updates
                            .into_iter()
                            .map(|update| api::message::tool_call::apply_file_diffs::V4aFileUpdate {
                                file_path: update.file_path,
                                move_to: update.move_to.unwrap_or_default(),
                                hunks: update
                                    .hunks
                                    .into_iter()
                                    .map(|hunk| {
                                        api::message::tool_call::apply_file_diffs::v4a_file_update::Hunk {
                                            change_context: hunk.change_context,
                                            pre_context: hunk.pre_context.unwrap_or_default(),
                                            old: hunk.old.unwrap_or_default(),
                                            new: hunk.new.unwrap_or_default(),
                                            post_context: hunk.post_context.unwrap_or_default(),
                                        }
                                    })
                                    .collect(),
                            })
                            .collect(),
                        ..Default::default()
                    },
                )
            }
            "suggest_prompt" => {
                let args: SuggestPromptArgs = serde_json::from_str(&self.arguments)?;
                api::message::tool_call::Tool::SuggestPrompt(args.into_api())
            }
            "suggest_rule" => {
                let args: SuggestRuleArgs = serde_json::from_str(&self.arguments)?;
                return Ok(WarpClientOutput::Suggestions(api::Suggestions {
                    rules: vec![api::SuggestedRule {
                        name: args.name,
                        content: args.content,
                        logging_id: args
                            .logging_id
                            .filter(|id| !id.trim().is_empty())
                            .unwrap_or_else(|| format!("byob-rule-{}", Uuid::new_v4())),
                    }],
                    workflows: vec![],
                }));
            }
            other => anyhow::bail!("unknown tool `{other}`"),
        };

        Ok(WarpClientOutput::ToolCall(api::message::ToolCall {
            tool_call_id,
            tool: Some(tool),
            ..Default::default()
        }))
    }
}

#[derive(Debug, Deserialize)]
struct RunShellCommandArgs {
    command: String,
    #[serde(default)]
    is_read_only: bool,
    #[serde(default)]
    uses_pager: bool,
    #[serde(default)]
    is_risky: bool,
    wait_until_complete: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReadShellCommandOutputArgs {
    command_id: String,
    delay_seconds: Option<i64>,
    #[serde(default)]
    wait_until_complete: bool,
}

impl ReadShellCommandOutputArgs {
    fn to_delay(&self) -> Option<api::message::tool_call::read_shell_command_output::Delay> {
        if self.wait_until_complete {
            return Some(
                api::message::tool_call::read_shell_command_output::Delay::OnCompletion(()),
            );
        }

        self.delay_seconds.map(|seconds| {
            api::message::tool_call::read_shell_command_output::Delay::Duration(
                prost_types::Duration {
                    seconds: seconds.max(0),
                    nanos: 0,
                },
            )
        })
    }
}

#[derive(Debug, Deserialize)]
struct WriteToLongRunningShellCommandArgs {
    command_id: String,
    input: String,
    mode: Option<String>,
}

impl WriteToLongRunningShellCommandArgs {
    fn to_mode(&self) -> api::message::tool_call::write_to_long_running_shell_command::Mode {
        use api::message::tool_call::write_to_long_running_shell_command::mode::Mode;

        let mode = match self
            .mode
            .as_deref()
            .unwrap_or("raw")
            .to_ascii_lowercase()
            .as_str()
        {
            "line" => Mode::Line(()),
            "block" => Mode::Block(()),
            _ => Mode::Raw(()),
        };

        api::message::tool_call::write_to_long_running_shell_command::Mode { mode: Some(mode) }
    }
}

#[derive(Debug, Deserialize)]
struct TransferShellCommandControlArgs {
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ReadFilesArgs {
    files: Vec<ReadFileArg>,
}

#[derive(Debug, Deserialize)]
struct ReadFileArg {
    #[serde(alias = "path", alias = "file_path")]
    name: Option<String>,
    #[serde(default)]
    line_ranges: Vec<LineRangeArg>,
}

#[derive(Debug, Deserialize)]
struct LineRangeArg {
    start: u32,
    end: u32,
}

impl From<LineRangeArg> for api::FileContentLineRange {
    fn from(value: LineRangeArg) -> Self {
        Self {
            start: value.start,
            end: value.end,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchCodebaseArgs {
    query: String,
    #[serde(default)]
    path_filters: Vec<String>,
    codebase_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    query: Option<String>,
    #[serde(default)]
    queries: Vec<String>,
    path: Option<String>,
}

impl GrepArgs {
    fn queries(self) -> Vec<String> {
        let mut queries = self.queries;
        if let Some(query) = self.query.filter(|query| !query.trim().is_empty()) {
            queries.push(query);
        }
        queries
    }
}

#[derive(Debug, Deserialize)]
struct FileGlobArgs {
    patterns: Vec<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileGlobV2Args {
    patterns: Vec<String>,
    search_dir: Option<String>,
    max_matches: Option<i32>,
    max_depth: Option<i32>,
    min_depth: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ReadSkillArgs {
    skill_path: Option<String>,
    bundled_skill_id: Option<String>,
    name: Option<String>,
}

impl ReadSkillArgs {
    fn skill_reference(
        self,
    ) -> anyhow::Result<api::message::tool_call::read_skill::SkillReference> {
        if let Some(path) = self.skill_path.filter(|path| !path.trim().is_empty()) {
            return Ok(api::message::tool_call::read_skill::SkillReference::SkillPath(path));
        }

        if let Some(id) = self.bundled_skill_id.filter(|id| !id.trim().is_empty()) {
            return Ok(api::message::tool_call::read_skill::SkillReference::BundledSkillId(id));
        }

        anyhow::bail!("read_skill requires skill_path or bundled_skill_id")
    }
}

#[derive(Debug, Deserialize)]
struct AskUserQuestionArgs {
    questions: Vec<AskUserQuestionArg>,
}

#[derive(Debug, Deserialize)]
struct AskUserQuestionArg {
    question_id: Option<String>,
    question: String,
    options: Vec<String>,
    recommended_option_index: Option<i32>,
    #[serde(default)]
    is_multiselect: bool,
    #[serde(default)]
    supports_other: bool,
}

impl From<AskUserQuestionArg> for api::ask_user_question::Question {
    fn from(value: AskUserQuestionArg) -> Self {
        let options = value
            .options
            .into_iter()
            .map(|label| api::ask_user_question::Option { label })
            .collect();

        api::ask_user_question::Question {
            question_id: value
                .question_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| format!("question_{}", Uuid::new_v4())),
            question: value.question,
            question_type: Some(
                api::ask_user_question::question::QuestionType::MultipleChoice(
                    api::ask_user_question::MultipleChoice {
                        options,
                        recommended_option_index: value.recommended_option_index.unwrap_or(-1),
                        is_multiselect: value.is_multiselect,
                        supports_other: value.supports_other,
                    },
                ),
            ),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ApplyFileDiffsArgs {
    summary: Option<String>,
    #[serde(default)]
    diffs: Vec<FileDiffArgs>,
    #[serde(default)]
    new_files: Vec<NewFileArgs>,
    #[serde(default)]
    deleted_files: Vec<DeletedFileArgs>,
    #[serde(default)]
    v4a_updates: Vec<V4AUpdateArgs>,
}

#[derive(Debug, Deserialize)]
struct FileDiffArgs {
    file_path: String,
    search: String,
    replace: String,
}

#[derive(Debug, Deserialize)]
struct NewFileArgs {
    file_path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct DeletedFileArgs {
    file_path: String,
}

#[derive(Debug, Deserialize)]
struct V4AUpdateArgs {
    file_path: String,
    move_to: Option<String>,
    #[serde(default)]
    hunks: Vec<V4AHunkArgs>,
}

#[derive(Debug, Deserialize)]
struct V4AHunkArgs {
    #[serde(default)]
    change_context: Vec<String>,
    pre_context: Option<String>,
    old: Option<String>,
    new: Option<String>,
    post_context: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SuggestPromptArgs {
    prompt: String,
    label: Option<String>,
    mode: Option<String>,
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    is_trigger_irrelevant: bool,
}

impl SuggestPromptArgs {
    fn into_api(self) -> api::message::tool_call::SuggestPrompt {
        use api::message::tool_call::suggest_prompt::{DisplayMode, InlineQueryBanner, PromptChip};

        let mode = self
            .mode
            .as_deref()
            .unwrap_or("prompt_chip")
            .to_ascii_lowercase();
        let wants_inline_banner = mode == "inline_query_banner"
            || self
                .title
                .as_ref()
                .is_some_and(|title| !title.trim().is_empty())
            || self
                .description
                .as_ref()
                .is_some_and(|description| !description.trim().is_empty());

        let display_mode = if wants_inline_banner {
            DisplayMode::InlineQueryBanner(InlineQueryBanner {
                title: self
                    .title
                    .or_else(|| self.label.clone())
                    .unwrap_or_else(|| "Suggested prompt".to_string()),
                description: self.description.unwrap_or_default(),
                query: self.prompt,
            })
        } else {
            DisplayMode::PromptChip(PromptChip {
                prompt: self.prompt,
                label: self.label.unwrap_or_default(),
            })
        };

        api::message::tool_call::SuggestPrompt {
            is_trigger_irrelevant: self.is_trigger_irrelevant,
            display_mode: Some(display_mode),
        }
    }
}

#[derive(Debug, Deserialize)]
struct SuggestRuleArgs {
    name: String,
    content: String,
    logging_id: Option<String>,
}

#[derive(Default)]
struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    fn push(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        while let Some((index, len)) = find_sse_boundary(&self.buffer) {
            let raw_event = self.buffer[..index].to_string();
            self.buffer.drain(..index + len);
            let data = raw_event
                .lines()
                .filter_map(|line| {
                    let line = line.trim_end_matches('\r');
                    line.strip_prefix("data:").map(str::trim_start)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                events.push(data);
            }
        }
        events
    }

    fn into_remaining(self) -> String {
        self.buffer
    }
}

fn find_sse_boundary(buffer: &str) -> Option<(usize, usize)> {
    match (buffer.find("\r\n\r\n"), buffer.find("\n\n")) {
        (Some(crlf), Some(lf)) if crlf < lf => Some((crlf, 4)),
        (Some(_), Some(lf)) => Some((lf, 2)),
        (Some(crlf), None) => Some((crlf, 4)),
        (None, Some(lf)) => Some((lf, 2)),
        (None, None) => None,
    }
}

fn format_reasoning_delta(reasoning: &str) -> String {
    if reasoning.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}", reasoning)
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut iter = text.chars();
    let truncated = iter.by_ref().take(max_chars).collect::<String>();
    if iter.next().is_some() {
        format!("{truncated}\n...[truncated]")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            listen_addr: "127.0.0.1:1337".parse().unwrap(),
            openai_base_url: "http://127.0.0.1:8000/v1".to_string(),
            backend_url_override: None,
            openai_api_key: None,
            model_override: Some("test-model".to_string()),
            wire_api: WireApi::ChatCompletions,
            extra_headers: BTreeMap::new(),
            system_prompt: "system".to_string(),
            request_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn converts_user_query_to_openai_message() {
        let request = api::Request {
            task_context: Some(api::request::TaskContext { tasks: vec![] }),
            input: Some(api::request::Input {
                context: None,
                r#type: Some(api::request::input::Type::UserInputs(
                    api::request::input::UserInputs {
                        inputs: vec![api::request::input::user_inputs::UserInput {
                            input: Some(
                                api::request::input::user_inputs::user_input::Input::UserQuery(
                                    api::request::input::UserQuery {
                                        query: "list files".to_string(),
                                        referenced_attachments: Default::default(),
                                        mode: None,
                                        intended_agent: 0,
                                        ..Default::default()
                                    },
                                ),
                            ),
                        }],
                    },
                )),
                ..Default::default()
            }),
            settings: Some(api::request::Settings {
                model_config: Some(api::request::settings::ModelConfig {
                    base: "from-request".to_string(),
                    ..Default::default()
                }),
                supported_tools: vec![api::ToolType::RunShellCommand as i32],
                ..Default::default()
            }),
            metadata: None,
            existing_suggestions: None,
            mcp_context: None,
            ..Default::default()
        };

        let chat = build_chat_request(&request, &test_config());

        assert_eq!(chat.model, "from-request");
        assert!(chat
            .messages
            .iter()
            .any(|message| message.role == "user"
                && message.content.as_deref() == Some("list files")));
        assert!(chat
            .tools
            .iter()
            .any(|tool| tool.function.name == "run_shell_command"));
    }

    #[test]
    fn default_prompt_describes_agentic_terminal_loop() {
        let prompt = default_agent_system_prompt();

        assert!(prompt.contains("state what you are about to do"));
        assert!(prompt.contains("monitor it with the long-running command tools"));
    }

    #[test]
    fn exposes_long_running_terminal_tools() {
        let request = api::Request {
            settings: Some(api::request::Settings {
                supported_tools: vec![
                    api::ToolType::RunShellCommand as i32,
                    api::ToolType::ReadShellCommandOutput as i32,
                    api::ToolType::WriteToLongRunningShellCommand as i32,
                    api::ToolType::TransferShellCommandControlToUser as i32,
                ],
                ..Default::default()
            }),
            ..Default::default()
        };

        let tools = build_tool_definitions(&request);
        let names = tools
            .iter()
            .map(|tool| tool.function.name)
            .collect::<BTreeSet<_>>();

        assert!(names.contains("run_shell_command"));
        assert!(names.contains("read_shell_command_output"));
        assert!(names.contains("write_to_lrc"));
        assert!(names.contains("transfer_shell_command_control"));
    }

    #[test]
    fn exposes_native_context_and_question_tools() {
        let request = api::Request {
            settings: Some(api::request::Settings {
                supported_tools: vec![
                    api::ToolType::ReadFiles as i32,
                    api::ToolType::SearchCodebase as i32,
                    api::ToolType::Grep as i32,
                    api::ToolType::FileGlobV2 as i32,
                    api::ToolType::ReadSkill as i32,
                    api::ToolType::AskUserQuestion as i32,
                ],
                ..Default::default()
            }),
            ..Default::default()
        };

        let tools = build_tool_definitions(&request);
        let names = tools
            .iter()
            .map(|tool| tool.function.name)
            .collect::<BTreeSet<_>>();

        assert!(names.contains("read_files"));
        assert!(names.contains("search_codebase"));
        assert!(names.contains("grep"));
        assert!(names.contains("file_glob_v2"));
        assert!(names.contains("read_skill"));
        assert!(names.contains("ask_user_question"));
    }

    #[test]
    fn encodes_warp_sse_data_as_decodable_response_event() {
        let event = finished_event_done();
        let encoded = BASE64_URL_SAFE.encode(event.encode_to_vec());
        let decoded = BASE64_URL_SAFE.decode(encoded).unwrap();
        let round_trip = api::ResponseEvent::decode(decoded.as_slice()).unwrap();

        assert!(matches!(
            round_trip.r#type,
            Some(api::response_event::Type::Finished(_))
        ));
    }

    #[test]
    fn generated_messages_use_current_timestamps() {
        let ids = WarpIds {
            conversation_id: "conv".to_string(),
            request_id: "req".to_string(),
            run_id: "run".to_string(),
            task_id: "task".to_string(),
        };

        let message = agent_output_message(&ids, "msg", "hello");

        assert!(message
            .timestamp
            .is_some_and(|timestamp| timestamp.seconds > 0));
    }

    #[test]
    fn responses_input_drops_orphaned_function_calls() {
        let input = responses_input_items_from_chat_messages(vec![
            OpenAIChatMessage::assistant_tool_call(OpenAIMessageToolCall {
                id: "call_missing_output".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "run_shell_command".to_string(),
                    arguments: r#"{"command":"date"}"#.to_string(),
                },
            }),
            OpenAIChatMessage::user("continue"),
        ]);

        assert!(input.iter().all(|item| {
            item.get("call_id").and_then(Value::as_str) != Some("call_missing_output")
        }));
        assert!(input
            .iter()
            .any(|item| item.get("role").and_then(Value::as_str) == Some("user")));
    }

    #[test]
    fn responses_input_keeps_paired_function_calls_and_outputs() {
        let input = responses_input_items_from_chat_messages(vec![
            OpenAIChatMessage::assistant_tool_call(OpenAIMessageToolCall {
                id: "call_with_output".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "run_shell_command".to_string(),
                    arguments: r#"{"command":"date"}"#.to_string(),
                },
            }),
            OpenAIChatMessage::tool("call_with_output", "Tue May 19"),
        ]);

        assert!(input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item.get("call_id").and_then(Value::as_str) == Some("call_with_output")
        }));
        assert!(input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("call_with_output")
        }));
    }

    #[test]
    fn accumulates_openai_tool_call_delta_to_warp_tool_call() {
        let mut calls = ToolCallAccumulatorSet::default();
        calls.apply_deltas(vec![OpenAIToolCallDelta {
            index: 0,
            id: Some("call_1".to_string()),
            r#type: Some("function".to_string()),
            function: Some(OpenAIFunctionDelta {
                name: Some("run_shell_command".to_string()),
                arguments: Some("{\"command\":\"pwd\",\"is_read_only\":true}".to_string()),
            }),
        }]);

        let call = calls.into_warp_tool_calls().remove(0).unwrap();
        assert_eq!(call.tool_call_id, "call_1");
        assert!(matches!(
            call.tool,
            Some(api::message::tool_call::Tool::RunShellCommand(_))
        ));
    }

    #[test]
    fn accumulates_long_running_terminal_tool_calls() {
        let mut calls = ToolCallAccumulatorSet::default();
        calls.apply_full_calls(vec![
            OpenAIMessageToolCall {
                id: "call_read".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "read_shell_command_output".to_string(),
                    arguments: r#"{"command_id":"block_1","delay_seconds":1}"#.to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_write".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "write_to_lrc".to_string(),
                    arguments: r#"{"command_id":"block_1","input":"y\n","mode":"raw"}"#.to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_transfer".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "transfer_shell_command_control".to_string(),
                    arguments: r#"{"reason":"Password prompt requires the user."}"#.to_string(),
                },
            },
        ]);

        let calls = calls
            .into_warp_tool_calls()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(matches!(
            calls[0].tool.as_ref(),
            Some(api::message::tool_call::Tool::ReadShellCommandOutput(_))
        ));
        assert!(matches!(
            calls[1].tool.as_ref(),
            Some(api::message::tool_call::Tool::WriteToLongRunningShellCommand(_))
        ));
        assert!(matches!(
            calls[2].tool.as_ref(),
            Some(api::message::tool_call::Tool::TransferShellCommandControlToUser(_))
        ));
    }

    #[test]
    fn accumulates_context_and_question_tool_calls() {
        let mut calls = ToolCallAccumulatorSet::default();
        calls.apply_full_calls(vec![
            OpenAIMessageToolCall {
                id: "call_read_files".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "read_files".to_string(),
                    arguments: r#"{"files":[{"path":"src/main.rs","line_ranges":[{"start":1,"end":20}]}]}"#.to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_search".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "search_codebase".to_string(),
                    arguments: r#"{"query":"agent action conversion","path_filters":["crates/ai"]}"#
                        .to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_grep".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "grep".to_string(),
                    arguments: r#"{"query":"ToolCallAccumulator","path":"crates/byob_proxy"}"#
                        .to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_glob".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "file_glob_v2".to_string(),
                    arguments: r#"{"patterns":["*.rs"],"search_dir":"crates/byob_proxy","max_matches":10}"#
                        .to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_skill".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "read_skill".to_string(),
                    arguments: r#"{"skill_path":"/tmp/SKILL.md","name":"tmp"}"#.to_string(),
                },
            },
            OpenAIMessageToolCall {
                id: "call_question".to_string(),
                r#type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: "ask_user_question".to_string(),
                    arguments: r#"{"questions":[{"question":"Which host should I inspect?","options":["dev","prod"],"recommended_option_index":0}]}"#
                        .to_string(),
                },
            },
        ]);

        let calls = calls
            .into_warp_tool_calls()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(matches!(
            calls[0].tool.as_ref(),
            Some(api::message::tool_call::Tool::ReadFiles(_))
        ));
        assert!(matches!(
            calls[1].tool.as_ref(),
            Some(api::message::tool_call::Tool::SearchCodebase(_))
        ));
        assert!(matches!(
            calls[2].tool.as_ref(),
            Some(api::message::tool_call::Tool::Grep(_))
        ));
        assert!(matches!(
            calls[3].tool.as_ref(),
            Some(api::message::tool_call::Tool::FileGlobV2(_))
        ));
        assert!(matches!(
            calls[4].tool.as_ref(),
            Some(api::message::tool_call::Tool::ReadSkill(_))
        ));
        assert!(matches!(
            calls[5].tool.as_ref(),
            Some(api::message::tool_call::Tool::AskUserQuestion(_))
        ));
    }

    #[test]
    fn parses_crlf_sse_events() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push("data: {\"a\":1}\r\n\r\ndata: [DONE]\r\n\r\n");
        assert_eq!(events, vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn parses_codex_oca_provider_config() {
        let parsed = parse_codex_config(
            r#"
            model_provider = "oca"
            profile = "codex-enterprise"
            model = "top-level"

            [model_providers.oca]
            base_url = "https://code.example.com/20250206/app/litellm"
            http_headers = { "client" = "codex-cli", "client-version" = "0" }
            model = "provider-model"
            wire_api = "responses"

            [profiles.codex-enterprise]
            model = "profile-model"
            model_provider = "oca"
            "#,
        )
        .unwrap();

        assert_eq!(
            parsed.base_url.as_deref(),
            Some("https://code.example.com/20250206/app/litellm")
        );
        assert_eq!(parsed.model.as_deref(), Some("profile-model"));
        assert_eq!(parsed.wire_api, Some(WireApi::Responses));
        assert_eq!(
            parsed.http_headers.get("client").map(String::as_str),
            Some("codex-cli")
        );
    }

    #[test]
    fn suggests_brew_install_after_brew_search() {
        let request = GenerateAIInputSuggestionsRequest {
            context_messages: vec![ContextMessagePayload::String(
                serde_json::json!({
                    "input": "brew search nvtop",
                    "output": "==> Formulae\nnvtop",
                    "context": {"exit_code": 0}
                })
                .to_string(),
            )],
            ..Default::default()
        };

        let response = local_next_command_suggestion(&request).unwrap();

        assert_eq!(response.most_likely_action, "brew install nvtop");
        assert_eq!(response.commands, vec!["brew install nvtop"]);
    }

    #[test]
    fn brew_install_suggestion_respects_prefix() {
        let request = GenerateAIInputSuggestionsRequest {
            context_messages: vec![ContextMessagePayload::String(
                serde_json::json!({
                    "input": "brew search nvtop",
                    "output": "nvtop",
                    "context": {"exit_code": 0}
                })
                .to_string(),
            )],
            prefix: Some("git".to_string()),
            ..Default::default()
        };

        assert!(local_next_command_suggestion(&request).is_none());
    }

    #[test]
    fn accepts_object_context_message_for_brew_fallback() {
        let request = GenerateAIInputSuggestionsRequest {
            context_messages: vec![ContextMessagePayload::Object(serde_json::json!({
                "input": "brew search nvtop",
                "output": "==> Formulae\nnvtop",
                "context": {"exit_code": 0}
            }))],
            ..Default::default()
        };

        let response = local_next_command_suggestion(&request).unwrap();

        assert_eq!(response.most_likely_action, "brew install nvtop");
    }

    #[test]
    fn accepts_literal_newline_context_message_for_brew_fallback() {
        let request = GenerateAIInputSuggestionsRequest {
            context_messages: vec![ContextMessagePayload::String(
                "{\"input\":\"brew search nvtop\",\"output\":\"==> Formulae\nnvtop\",\"context\":{\"exit_code\":0}}".to_string(),
            )],
            ..Default::default()
        };

        let response = local_next_command_suggestion(&request).unwrap();

        assert_eq!(response.most_likely_action, "brew install nvtop");
    }

    #[test]
    fn generates_local_block_title_for_common_commands() {
        let request = GenerateBlockTitleRequest {
            command: "brew install nvtop".to_string(),
            output: String::new(),
        };

        assert_eq!(
            local_block_title(&request),
            Some("Install nvtop".to_string())
        );
    }

    #[test]
    fn parses_agent_mode_query_suggestion_json() {
        let response = parse_am_query_suggestion_text(
            r#"```json
            {"query":"Explain why this command failed","should_plan_task":false}
            ```"#,
        );

        assert!(matches!(
            response.suggestion,
            Some(AMSuggestion::Simple(AMSimpleQuery { query, .. }))
                if query == "Explain why this command failed"
        ));
    }

    #[test]
    fn parses_concatenated_agent_mode_query_json() {
        let response = parse_am_query_suggestion_text(
            r#"{"query":"Install nvtop with Homebrew and verify it runs","should_plan_task":false}{"query":"Install nvtop with Homebrew and verify it runs","should_plan_task":false}"#,
        );

        assert!(matches!(
            response.suggestion,
            Some(AMSuggestion::Simple(AMSimpleQuery { query, .. }))
                if query == "Install nvtop with Homebrew and verify it runs"
        ));
    }

    #[test]
    fn cleans_concatenated_predict_am_query_json() {
        let suggestion = clean_query_suggestion(
            r#"{"query":"Install nvtop with Homebrew and verify it runs","should_plan_task":false}{"query":"Install nvtop with Homebrew and verify it runs","should_plan_task":false}"#,
            "",
        );

        assert_eq!(suggestion, "Install nvtop with Homebrew and verify it runs");
    }

    #[test]
    fn parses_concatenated_input_suggestion_json() {
        let request = GenerateAIInputSuggestionsRequest::default();
        let response = parse_input_suggestion_text(
            &request,
            r#"{"commands":["brew install nvtop"],"ai_queries":[],"most_likely_action":"brew install nvtop"}{"commands":["brew install nvtop"],"ai_queries":[],"most_likely_action":"brew install nvtop"}"#,
        );

        assert_eq!(response.most_likely_action, "brew install nvtop");
    }

    #[test]
    fn ranks_relevant_files_from_query_terms() {
        let request = GetRelevantFilesRequest {
            query: "auth login token".to_string(),
            files: vec![
                RelevantFileContext {
                    path: "src/ui/view.rs".to_string(),
                    symbols: "render settings".to_string(),
                },
                RelevantFileContext {
                    path: "src/auth/login.rs".to_string(),
                    symbols: "login token refresh".to_string(),
                },
            ],
        };

        let paths = rank_relevant_files(&request);

        assert_eq!(paths.first().map(String::as_str), Some("src/auth/login.rs"));
    }

    #[test]
    fn parses_responses_text_delta() {
        let mut calls = ToolCallAccumulatorSet::default();
        let text = parse_responses_sse_data(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            &mut calls,
            true,
        )
        .unwrap();

        assert_eq!(text, vec!["hello"]);
    }

    #[test]
    fn responses_sse_text_does_not_duplicate_completed_output() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"query\\\":\\\"Install nvtop\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" with Homebrew and verify it runs\\\",\\\"should_plan_task\\\":false}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"{\\\"query\\\":\\\"Install nvtop with Homebrew and verify it runs\\\",\\\"should_plan_task\\\":false}\"}}\n\n",
            "data: [DONE]\n\n",
        );

        let text = backend_text_from_body(WireApi::Responses, body).unwrap();

        assert_eq!(
            text,
            r#"{"query":"Install nvtop with Homebrew and verify it runs","should_plan_task":false}"#
        );
    }

    #[test]
    fn parses_responses_function_call_to_warp_tool_call() {
        let mut calls = ToolCallAccumulatorSet::default();
        parse_responses_sse_data(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"run_shell_command","arguments":""}}"#,
            &mut calls,
            true,
        )
        .unwrap();
        parse_responses_sse_data(
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"command\":\"pwd\",\"is_read_only\":true}"}"#,
            &mut calls,
            true,
        )
        .unwrap();

        let call = calls.into_warp_tool_calls().remove(0).unwrap();
        assert_eq!(call.tool_call_id, "call_1");
        assert!(matches!(
            call.tool,
            Some(api::message::tool_call::Tool::RunShellCommand(_))
        ));
    }

    #[test]
    fn parses_suggest_prompt_tool_call_to_warp_tool_call() {
        let mut calls = ToolCallAccumulatorSet::default();
        calls.apply_full_calls(vec![OpenAIMessageToolCall {
            id: "call_prompt".to_string(),
            r#type: "function".to_string(),
            function: OpenAIFunctionCall {
                name: "suggest_prompt".to_string(),
                arguments: r#"{"prompt":"Explain this failure","label":"Explain failure"}"#
                    .to_string(),
            },
        }]);

        let call = calls.into_warp_tool_calls().remove(0).unwrap();
        assert_eq!(call.tool_call_id, "call_prompt");
        assert!(matches!(
            call.tool,
            Some(api::message::tool_call::Tool::SuggestPrompt(_))
        ));
    }

    #[test]
    fn parses_suggest_rule_tool_call_to_show_suggestions_payload() {
        let mut calls = ToolCallAccumulatorSet::default();
        calls.apply_full_calls(vec![OpenAIMessageToolCall {
            id: "call_rule".to_string(),
            r#type: "function".to_string(),
            function: OpenAIFunctionCall {
                name: "suggest_rule".to_string(),
                arguments: r#"{"name":"Use OKE","content":"Prefer OKE for Kubernetes examples.","logging_id":"rule_1"}"#
                    .to_string(),
            },
        }]);

        let outputs = calls.into_warp_client_outputs();
        assert!(matches!(
            &outputs[0],
            Ok(WarpClientOutput::Suggestions(suggestions))
                if suggestions.rules.first().is_some_and(|rule| rule.logging_id == "rule_1")
        ));
    }
}
