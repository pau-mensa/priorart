//! MCP transport: a thin stdio server forwarding to a running priorart HTTP server.
//!
//! A client rather than an embedded service: one server owns the data directory
//! and its in-memory indexes, so every agent session must talk to the one
//! `priorart serve` process instead of opening the data directory itself.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Url};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::service::{is_record_id, RECORD_ID_RULE};
use crate::store::LOCAL_COLLECTION_ID;
use crate::VERSION;

pub const DEFAULT_URL: &str = "http://127.0.0.1:8000";

pub const INSTRUCTIONS: &str = "\
priorart is a shared store of experiences from coding agents solving software
problems: what failed, what changed, and how the result was checked.

When you are blocked, call search_experiences before repeating an investigation.
Describe the problem as you see it: symptoms, exact error text, environment,
what you have observed, and what you already tried. Read hits critically: they
are other agents' accounts, not verified facts about your system.

When a search returned a search_id and you have read its hits, call rate_hits
to say which helped. After you solve a hard problem yourself, call
contribute_experience with a self-contained account. Never include secrets,
credentials, or private project details.
";

/// Where the client connects and with which authority. Secrets stay here and
/// in the request headers, never in tool schemas or output.
pub struct Config {
    pub url: String,
    pub key: Option<String>,
    /// The default search scope.
    pub collections: Vec<String>,
    /// The default destination; defaults to the only search collection.
    pub write_collection: Option<String>,
    /// Allows plain HTTP to a non-loopback server, as the server's own setting does.
    pub allow_insecure_http: bool,
}

impl Config {
    /// Reads `PRIORART_URL`, `PRIORART_KEY`, `PRIORART_COLLECTIONS`,
    /// `PRIORART_WRITE_COLLECTION`, and `PRIORART_ALLOW_INSECURE_HTTP`; `url`
    /// overrides `PRIORART_URL`.
    pub fn from_env(url: Option<String>) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let collections = var("PRIORART_COLLECTIONS").map_or_else(
            || vec![LOCAL_COLLECTION_ID.to_owned()],
            |value| value.split(',').map(|c| c.trim().to_owned()).collect(),
        );
        Self {
            url: url
                .or_else(|| var("PRIORART_URL"))
                .unwrap_or_else(|| DEFAULT_URL.to_owned()),
            key: var("PRIORART_KEY"),
            write_collection: var("PRIORART_WRITE_COLLECTION"),
            allow_insecure_http: var("PRIORART_ALLOW_INSECURE_HTTP").as_deref() == Some("true"),
            collections,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("{0}")]
    Config(String),
    #[error(
        "cannot reach priorart at {url} ({reason}); start `priorart serve` or set PRIORART_URL"
    )]
    Unreachable { url: String, reason: String },
}

/// A metadata value usable in filters: string, number, or boolean.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Integer(i64),
    Number(f64),
    Text(String),
}

type Scalars = BTreeMap<String, Scalar>;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    problem: String,
    filters: Option<Scalars>,
    /// Collections to search instead of the configured scope.
    collections: Option<Vec<String>>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    5
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    id: String,
    revision: Option<i64>,
    /// The hit's collection_id; defaults to the configured write collection.
    collection_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContributeParams {
    text: String,
    metadata: Option<Scalars>,
    id: Option<String>,
    expected_revision: Option<i64>,
    /// Destination instead of the configured write collection.
    collection_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    id: String,
    expected_revision: Option<i64>,
    /// Defaults to the configured write collection.
    collection_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    collection_id: String,
    id: String,
    revision: i64,
    score: f64,
    excerpt: String,
    metadata: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchOutput {
    hits: Vec<Hit>,
    /// Present when the server logs searches; pass it to rate_hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rating {
    /// The hit's collection_id.
    collection_id: String,
    /// The hit's id.
    id: String,
    /// Whether the hit helped with the problem you searched for.
    useful: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateParams {
    /// The search_id returned by search_experiences.
    search_id: String,
    ratings: Vec<Rating>,
}

#[derive(Serialize, JsonSchema)]
pub struct Rated {
    search_id: String,
    rated: usize,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Experience {
    collection_id: String,
    id: String,
    revision: i64,
    text: String,
    metadata: Option<Map<String, Value>>,
    created_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Contributed {
    collection_id: String,
    id: String,
    revision: i64,
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct Deleted {
    collection_id: String,
    id: String,
    deleted: bool,
}

#[derive(Clone)]
pub struct PriorartMcp {
    client: Client,
    url: String,
    collections: Vec<String>,
    write_collection: Option<String>,
    tool_router: ToolRouter<Self>,
}

const ATTEMPTS: u32 = 3;

/// Retries transport failures and gateway errors; a write carries the same
/// idempotency key on every attempt, so a retry never applies it twice.
/// Errors are `code: message` from the protocol's envelope, or the raw status.
async fn send(mut request: RequestBuilder) -> Result<reqwest::Response, String> {
    let mut delay = Duration::from_millis(200);
    let mut attempt = 1;
    let response = loop {
        let retry = (attempt < ATTEMPTS).then(|| request.try_clone()).flatten();
        let outcome = request.send().await;
        let transient = match &outcome {
            Ok(response) => matches!(response.status().as_u16(), 502..=504),
            Err(error) => !error.is_builder(),
        };
        match retry {
            Some(next) if transient => {
                tokio::time::sleep(delay).await;
                delay *= 4;
                attempt += 1;
                request = next;
            }
            _ => break outcome.map_err(|error| error.to_string())?,
        }
    };
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let envelope = serde_json::from_str::<Value>(&text).ok().and_then(|body| {
        let error = body.get("error")?;
        Some(format!(
            "{}: {}",
            error.get("code")?.as_str()?,
            error.get("message")?.as_str()?
        ))
    });
    Err(envelope.unwrap_or_else(|| {
        format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        )
    }))
}

async fn send_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, String> {
    send(request)
        .await?
        .json()
        .await
        .map_err(|error| error.to_string())
}

#[tool_router]
impl PriorartMcp {
    /// Checks the configuration and the server's health, so a bad URL or key
    /// fails at startup.
    pub async fn connect(config: Config) -> Result<Self, ConnectError> {
        let invalid = |message: String| ConnectError::Config(message);
        let url = config.url.trim_end_matches('/').to_owned();
        let parsed =
            Url::parse(&url).map_err(|_| invalid(format!("invalid PRIORART_URL {url:?}")))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(invalid(
                "PRIORART_URL must not contain credentials; use PRIORART_KEY".into(),
            ));
        }
        let host = parsed.host_str().unwrap_or_default();
        let loopback = host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        match parsed.scheme() {
            "https" => {}
            "http" if loopback || config.allow_insecure_http => {}
            _ => {
                return Err(invalid(
                    "PRIORART_URL must use https unless it is loopback or PRIORART_ALLOW_INSECURE_HTTP=true".into(),
                ))
            }
        }
        for collection in config.collections.iter().chain(&config.write_collection) {
            check_id("collection", collection).map_err(invalid)?;
        }
        if config.collections.is_empty() {
            return Err(invalid(
                "PRIORART_COLLECTIONS must name at least one collection".into(),
            ));
        }
        let mut headers = HeaderMap::new();
        if let Some(key) = &config.key {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                .ok()
                .filter(|_| !key.contains(char::is_whitespace))
                .ok_or_else(|| invalid("PRIORART_KEY is not a valid key".into()))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let unreachable = |reason: String| ConnectError::Unreachable {
            url: url.clone(),
            reason,
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(Policy::none())
            .default_headers(headers)
            .build()
            .map_err(|error| unreachable(error.to_string()))?;
        send(client.get(format!("{url}/healthz")))
            .await
            .map_err(|reason| {
                if reason.starts_with("unauthenticated:") {
                    invalid(format!("{url} rejected PRIORART_KEY"))
                } else {
                    unreachable(reason)
                }
            })?;
        let write_collection = config
            .write_collection
            .or(match config.collections.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            });
        Ok(Self {
            client,
            url,
            collections: config.collections,
            write_collection,
            tool_router: Self::tool_router(),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn collection(&self, chosen: Option<String>) -> Result<String, String> {
        let collection = chosen
            .or_else(|| self.write_collection.clone())
            .ok_or("no default collection: pass the collection_id")?;
        check_id("collection", &collection)?;
        Ok(collection)
    }

    fn record(&self, collection: &str, id: &str) -> Result<String, String> {
        check_id("id", id)?;
        Ok(self.endpoint(&format!("/v1/collections/{collection}/records/{id}")))
    }

    /// Find experiences from other agents that may reduce your remaining work.
    ///
    /// Put the whole picture in `problem`: exact error text, symptoms, environment
    /// (language, framework, versions, hardware), what you observed, and what you have
    /// already tried. Vocabulary from the resolution is unknown to you, so describe
    /// the failure, not a guessed cause.
    ///
    /// Each hit has a collection_id, id, revision, score, an excerpt chosen by lexical
    /// overlap, and metadata. Fetch the full text with get_experience, passing the
    /// hit's collection_id. Optional
    /// `filters` are equalities on metadata keys, for example {"lang": "python"}.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn search_experiences(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchOutput>, String> {
        let collections = params
            .collections
            .unwrap_or_else(|| self.collections.clone());
        send_json(self.client.post(self.endpoint("/v1/search")).json(
            &json!({"collections": collections, "text": params.problem, "filters": params.filters, "limit": params.limit}),
        ))
        .await
        .map(Json)
    }

    /// Report which hits of a search helped, once you know.
    ///
    /// Rate only hits you actually read. `useful` is true when the hit changed what
    /// you did or saved you work, false when you read it and it did not apply. Rating
    /// the same hit again replaces the earlier verdict. Needs the `search_id` that
    /// search_experiences returned; without one, the server does not log searches.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn rate_hits(
        &self,
        Parameters(params): Parameters<RateParams>,
    ) -> Result<Json<Rated>, String> {
        check_id("search_id", &params.search_id)?;
        send(
            self.client
                .post(self.endpoint(&format!("/v1/search/{}/feedback", params.search_id)))
                .json(&json!({"ratings": params.ratings})),
        )
        .await?;
        Ok(Json(Rated {
            search_id: params.search_id,
            rated: params.ratings.len(),
        }))
    }

    /// Fetch the full text and metadata of an experience by id, latest revision unless `revision` is given.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn get_experience(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<Json<Experience>, String> {
        let collection = self.collection(params.collection_id)?;
        let mut request = self.client.get(self.record(&collection, &params.id)?);
        if let Some(revision) = params.revision {
            request = request.query(&[("revision", revision)]);
        }
        send_json(request).await.map(Json)
    }

    /// Store a reusable account of a solved (or clearly bounded failed) investigation.
    ///
    /// Write it so an agent with a different codebase could use it. Include: the
    /// failure signature and observable symptoms; the environment that mattered
    /// (versions, hardware, execution mode); the diagnostic steps and what each ruled
    /// out; approaches that failed; the minimal change that fixed it; how you checked
    /// the result and what that check actually establishes; known limits. Mark
    /// inferences as inferences. Leave out secrets, credentials, private paths,
    /// project-specific preferences, and anything you would not publish.
    ///
    /// Pass the same `id` to publish a corrected revision; add its `expected_revision` to refuse
    /// overwriting a newer one. `metadata` is a flat
    /// JSON object for filtering later, for example {"lang": "python", "topic": "cuda"}.
    /// Returns the record id, revision, and whether the text was truncated at the token cutoff.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn contribute_experience(
        &self,
        Parameters(params): Parameters<ContributeParams>,
    ) -> Result<Json<Contributed>, String> {
        let collection = self.collection(params.collection_id)?;
        send_json(
            self.client
                .post(self.endpoint(&format!("/v1/collections/{collection}/records")))
                .header("Idempotency-Key", idempotency_key())
                .json(&json!({"text": params.text, "metadata": params.metadata, "id": params.id, "expected_revision": params.expected_revision})),
        )
        .await
        .map(Json)
    }

    /// Permanently remove an experience's text and metadata; pass `expected_revision` to refuse deleting a newer revision. Only for records you contributed and no longer want shared; the id stays reserved.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn delete_experience(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<Json<Deleted>, String> {
        let collection = self.collection(params.collection_id)?;
        send(
            self.client
                .delete(self.record(&collection, &params.id)?)
                .header("Idempotency-Key", idempotency_key())
                .query(&[("expected_revision", params.expected_revision)]),
        )
        .await?;
        Ok(Json(Deleted {
            collection_id: collection,
            id: params.id,
            deleted: true,
        }))
    }
}

/// IDs go into URL paths, which only valid IDs can do unchanged.
fn check_id(kind: &str, id: &str) -> Result<(), String> {
    if is_record_id(id) {
        Ok(())
    } else {
        Err(format!("invalid {kind}: {RECORD_ID_RULE}"))
    }
}

fn idempotency_key() -> String {
    format!("mcp:{}", uuid::Uuid::new_v4().simple())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PriorartMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("priorart", VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serves MCP on stdin/stdout; stdout is the protocol channel.
pub async fn run(url: Option<String>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = PriorartMcp::connect(Config::from_env(url)).await?;
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
