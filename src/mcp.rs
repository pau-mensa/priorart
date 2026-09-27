//! MCP transport: a thin stdio server forwarding to a running priorart HTTP server.
//!
//! A client rather than an embedded service: the index write lock is
//! process-local and the encoder is expensive to load, so every agent session
//! must talk to the one `priorart serve` process instead of opening the data
//! directory itself. Set `PRIORART_URL` to that server.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Client, RequestBuilder};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::service::{is_record_id, RECORD_ID_RULE};
use crate::VERSION;

pub const DEFAULT_URL: &str = "http://127.0.0.1:8000";

pub const INSTRUCTIONS: &str = "\
priorart is a shared store of experiences from coding agents solving software
problems: what failed, what changed, and how the result was checked.

When you are blocked, call search_experiences before repeating an investigation.
Describe the problem as you see it: symptoms, exact error text, environment,
what you have observed, and what you already tried. Read hits critically: they
are other agents' accounts, not verified facts about your system.

After you reuse an experience, call report_outcome with its revision and search_id so the
outcome is linked to what surfaced it. After you solve a hard problem yourself,
call contribute_experience with a self-contained account. Never include
secrets, credentials, or private project details.
";

#[derive(Debug, thiserror::Error)]
#[error("cannot reach priorart at {url} ({reason}); start `priorart serve` or set PRIORART_URL")]
pub struct Unreachable {
    url: String,
    reason: String,
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
pub struct SearchParams {
    problem: String,
    filters: Option<Scalars>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    5
}

#[derive(Deserialize, JsonSchema)]
pub struct GetParams {
    id: String,
    revision: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ContributeParams {
    text: String,
    metadata: Option<Scalars>,
    id: Option<String>,
    expected_revision: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReportParams {
    record_id: String,
    outcome: String,
    revision: i64,
    search_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteParams {
    id: String,
    expected_revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    id: String,
    revision: i64,
    score: f64,
    score_semantics: String,
    excerpt: String,
    metadata: Option<Map<String, Value>>,
}

#[derive(Serialize, JsonSchema)]
pub struct SearchOutput {
    search_id: String,
    hits: Vec<Hit>,
    next_step: String,
}

#[derive(Deserialize)]
struct SearchBody {
    search_id: String,
    hits: Vec<Hit>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Experience {
    id: String,
    revision: i64,
    text: String,
    metadata: Option<Map<String, Value>>,
    created_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Contributed {
    id: String,
    revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Reported {
    id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Deleted {
    id: String,
    deleted: bool,
}

#[derive(Clone)]
pub struct PriorartMcp {
    client: Client,
    url: String,
    tool_router: ToolRouter<Self>,
}

/// `code: message` from the protocol's error envelope, or the raw status.
async fn send(request: RequestBuilder) -> Result<reqwest::Response, String> {
    let response = request.send().await.map_err(|error| error.to_string())?;
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
    /// Checks the server's health so a misconfigured URL fails at startup.
    pub async fn connect(url: Option<String>) -> Result<Self, Unreachable> {
        let url = url
            .or_else(|| std::env::var("PRIORART_URL").ok())
            .unwrap_or_else(|| DEFAULT_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let unreachable = |reason: String| Unreachable {
            url: url.clone(),
            reason,
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| unreachable(error.to_string()))?;
        send(client.get(format!("{url}/healthz")))
            .await
            .map_err(unreachable)?;
        Ok(Self {
            client,
            url,
            tool_router: Self::tool_router(),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// Find experiences from other agents that may reduce your remaining work.
    ///
    /// Put the whole picture in `problem`: exact error text, symptoms, environment
    /// (language, framework, versions, hardware), what you observed, and what you have
    /// already tried. Vocabulary from the resolution is unknown to you, so describe
    /// the failure, not a guessed cause.
    ///
    /// Each hit has an id, revision, score, an excerpt chosen by lexical overlap, and
    /// metadata. Fetch the full text with get_experience. Keep the returned
    /// `search_id` and pass it to report_outcome when you act on a hit. Optional
    /// `filters` are equalities on metadata keys, for example {"lang": "python"}.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn search_experiences(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchOutput>, String> {
        let body: SearchBody = send_json(self.client.post(self.endpoint("/v1/search")).json(
            &json!({"collections": ["local"], "text": params.problem, "filters": params.filters, "limit": params.limit}),
        ))
        .await?;
        let next_step = format!(
            "Use get_experience(id) for the full text. When you act on a hit, call \
             report_outcome with search_id={:?}.",
            body.search_id
        );
        Ok(Json(SearchOutput {
            search_id: body.search_id,
            hits: body.hits,
            next_step,
        }))
    }

    /// Fetch the full text and metadata of an experience by id, latest revision unless `revision` is given.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn get_experience(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<Json<Experience>, String> {
        check_record_id(&params.id)?;
        let mut request = self
            .client
            .get(self.endpoint(&format!("/v1/collections/local/records/{}", params.id)));
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
    /// Pass the same `id` and its `expected_revision` to publish a corrected revision. `metadata` is a flat
    /// JSON object for filtering later, for example {"lang": "python", "topic": "cuda"}.
    /// Returns the record id and revision.
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
        send_json(
            self.client
                .post(self.endpoint("/v1/collections/local/records"))
                .json(&json!({"text": params.text, "metadata": params.metadata, "id": params.id, "expected_revision": params.expected_revision})),
        )
        .await
        .map(Json)
    }

    /// Record what happened after reusing an experience. This is evidence about one
    /// application, not a vote.
    ///
    /// Say what environment received the change, what you actually applied (it may
    /// differ from the record), which observable check passed or failed, and any side
    /// effects or limits you noticed. A failure often marks an applicability boundary
    /// rather than a bad record; say why you think it did not apply. Pass the
    /// exact `revision` you used and the `search_id` from the search that surfaced it.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = false
    ))]
    async fn report_outcome(
        &self,
        Parameters(params): Parameters<ReportParams>,
    ) -> Result<Json<Reported>, String> {
        send_json(
            self.client
                .post(self.endpoint("/v1/collections/local/reports"))
                .json(&json!({
                    "record_id": params.record_id,
                    "text": params.outcome,
                    "revision": params.revision,
                    "search_id": params.search_id,
                })),
        )
        .await
        .map(Json)
    }

    /// Permanently remove an experience's text and metadata using its current `expected_revision`. Only for records you contributed and no longer want shared; the id stays reserved.
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
        check_record_id(&params.id)?;
        send(
            self.client
                .delete(self.endpoint(&format!("/v1/collections/local/records/{}", params.id)))
                .query(&[("expected_revision", params.expected_revision)]),
        )
        .await?;
        Ok(Json(Deleted {
            id: params.id,
            deleted: true,
        }))
    }
}

/// Record IDs go into URL paths, which only valid IDs can do unchanged.
fn check_record_id(id: &str) -> Result<(), String> {
    if is_record_id(id) {
        Ok(())
    } else {
        Err(format!("invalid id: {RECORD_ID_RULE}"))
    }
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
    let server = PriorartMcp::connect(url).await?;
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
