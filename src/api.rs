use crate::Foremerge;
use crate::model::*;
use axum::Json;
use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Bounded wait for the Tokio runtime once `serve` has returned. Dropping the
/// runtime cancels pending request futures, which runs the validation
/// cancellation guards, but it cannot cancel a blocking task that is already
/// inside a synchronous child process. The caller uses this bound so a wedged
/// child cannot hold the process open forever.
pub const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How the HTTP server stopped, so the caller can decide how the process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Every in-flight request finished inside the grace period.
    Drained,
    /// The grace expired with requests still in flight; the remaining request
    /// futures were abandoned rather than awaited.
    GraceExpired,
}

impl ShutdownOutcome {
    /// The shutdown bound had to be enforced rather than observed.
    pub fn grace_expired(self) -> bool {
        matches!(self, Self::GraceExpired)
    }
}

/// The grace applied to in-flight HTTP work after SIGINT or SIGTERM.
pub fn shutdown_grace() -> Duration {
    SHUTDOWN_GRACE
}

#[derive(Clone)]
pub struct ApiState {
    pub service: Foremerge,
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
struct Success<T: Serialize> {
    ok: bool,
    data: T,
}

#[derive(Debug, Serialize)]
struct Failure {
    ok: bool,
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: String,
    message: String,
}

struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
}

struct ApiJson<T>(T);
struct ApiQuery<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|rejection| ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "INVALID_INPUT".to_string(),
                message: format!("INVALID_INPUT: {}", rejection.body_text()),
            })
    }
}

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection| ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "INVALID_INPUT".to_string(),
                message: format!("INVALID_INPUT: {}", rejection.body_text()),
            })
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let message = format!("{error:#}");
        let code = message
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .filter(|prefix| {
                prefix
                    .chars()
                    .all(|value| value.is_ascii_uppercase() || value == '_')
            })
            .unwrap_or("INTERNAL_ERROR")
            .to_string();
        let status = match code.as_str() {
            "INVALID_INPUT" => StatusCode::BAD_REQUEST,
            "NOT_FOUND" => StatusCode::NOT_FOUND,
            "FORBIDDEN" => StatusCode::FORBIDDEN,
            "STATE_RACE" => StatusCode::CONFLICT,
            "RESOURCE_LIMIT" => StatusCode::PAYLOAD_TOO_LARGE,
            "INVALID_TRANSITION"
            | "CHECK_FAILED"
            | "STALE_CHANGESET"
            | "BLOCKING_CONFLICT"
            | "UNSATISFIED_DEPENDENCY"
            | "TARGET_DIVERGED" => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            code,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(Failure {
                ok: false,
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<Json<Success<T>>, ApiError>;

fn success<T: Serialize>(data: T) -> Json<Success<T>> {
    Json(Success { ok: true, data })
}

pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/v1/agents/register", post(register_agent))
        .route("/v1/agents", get(list_agents))
        .route("/v1/agents/{id}/inbox", get(inbox))
        .route("/v1/intents", post(publish_intent))
        .route("/v1/intents/{id}", get(get_intent))
        .route("/v1/claims", post(claim_work))
        .route("/v1/assessments", post(record_assessment))
        .route("/v1/intents/{id}/assessments", get(list_assessments))
        .route("/v1/work", get(query_work))
        .route("/v1/work/{id}/start", post(start_work))
        .route("/v1/work/{id}/discard", post(discard_work))
        .route("/v1/conflicts", get(list_conflicts))
        .route("/v1/conflicts/check", post(check_conflicts))
        .route("/v1/conflicts/{id}/detections", get(conflict_detections))
        .route("/v1/conflicts/{id}/resolve", post(resolve_conflict))
        .route("/v1/changesets", post(publish_changeset))
        .route("/v1/changesets/{id}", get(get_changeset))
        .route(
            "/v1/changesets/{id}/validation-attempts",
            get(validation_attempts),
        )
        .route("/v1/changesets/{id}/validate", post(validate_changeset))
        .route("/v1/changesets/{id}/accept", post(accept_changeset))
        .route("/v1/changesets/{id}/commit", post(record_commit))
        .route("/v1/coordinate", post(coordinate))
        .route("/v1/events", get(events))
        .route("/v1/audit/event-chain", get(audit_event_chain))
        .route("/v1/graph", get(graph))
        .route("/v1/status", get(status))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .merge(protected)
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

async fn route_not_found() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "NOT_FOUND".to_string(),
        message: "route not found".to_string(),
    }
}

async fn method_not_allowed() -> ApiError {
    ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        code: "METHOD_NOT_ALLOWED".to_string(),
        message: "method not allowed".to_string(),
    }
}

async fn authenticate(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    authorize(&state, request.headers())?;
    Ok(next.run(request).await)
}

/// Serve the loopback JSON API until SIGINT or SIGTERM, then drain.
///
/// The returned outcome reports whether the drain completed or the grace had to
/// be enforced. A `GraceExpired` outcome means abandoned request futures may
/// still be attached to the runtime, so the caller must shut the runtime down
/// under [`RUNTIME_SHUTDOWN_GRACE`] and then exit the process deliberately.
pub async fn serve(state: ApiState, bind: SocketAddr) -> anyhow::Result<ShutdownOutcome> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "Foremerge API listening");
    serve_with_shutdown(listener, router(state), SHUTDOWN_GRACE, shutdown_signal()).await
}

/// Shared shutdown mechanics for [`serve`], with the listener, router, grace,
/// and shutdown trigger injected so the bound itself is testable.
async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    router: Router,
    grace: Duration,
    shutdown: F,
) -> anyhow::Result<ShutdownOutcome>
where
    F: Future<Output = ()> + Send + 'static,
{
    let draining = Arc::new(tokio::sync::Notify::new());
    let notify = draining.clone();
    let graceful = async move {
        shutdown.await;
        tracing::info!("shutdown requested; draining in-flight requests");
        notify.notify_one();
    };
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(graceful)
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            result?;
            Ok(ShutdownOutcome::Drained)
        }
        () = draining.notified() => {
            match tokio::time::timeout(grace, &mut server).await {
                Ok(result) => {
                    result?;
                    Ok(ShutdownOutcome::Drained)
                }
                Err(_) => {
                    tracing::warn!(
                        grace_seconds = grace.as_secs(),
                        "shutdown grace expired; abandoning remaining in-flight requests"
                    );
                    Ok(ShutdownOutcome::GraceExpired)
                }
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "install Ctrl-C handler");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "install SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn health() -> ApiResult<Value> {
    Ok(success(json!({
        "name": "foremerge",
        "version": env!("CARGO_PKG_VERSION"),
        "status": "alive",
    })))
}

async fn ready(State(state): State<ApiState>) -> ApiResult<Value> {
    let service = state.service;
    let ready = api_blocking(move || service.store().readiness()).await?;
    if !ready {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "NOT_READY".to_string(),
            message: "coordinator store is busy".to_string(),
        });
    }
    Ok(success(json!({ "status": "ready" })))
}

#[derive(Debug, Deserialize)]
struct AuditParams {
    page_size: Option<usize>,
}

async fn audit_event_chain(
    State(state): State<ApiState>,
    ApiQuery(params): ApiQuery<AuditParams>,
) -> ApiResult<EventChainAudit> {
    let service = state.service;
    Ok(success(
        api_blocking(move || {
            service
                .store()
                .audit_event_chain(params.page_size.unwrap_or(1000))
        })
        .await?,
    ))
}

async fn api_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR".to_string(),
            message: format!("blocking coordinator operation failed: {error}"),
        })?
        .map_err(ApiError::from)
}

async fn register_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<RegisterAgentRequest>,
) -> ApiResult<RegisterAgentOutcome> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.register_agent(request)).await?,
    ))
}

async fn list_agents(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Vec<Agent>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(api_blocking(move || service.list_agents()).await?))
}

async fn publish_intent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<PublishIntentRequest>,
) -> ApiResult<PublishIntentOutcome> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.publish_intent(request)).await?,
    ))
}

async fn get_intent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<IntentDetail> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.show_intent(&id)).await?,
    ))
}

async fn record_assessment(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<RecordAssessmentRequest>,
) -> ApiResult<Assessment> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.record_assessment(request)).await?,
    ))
}

async fn list_assessments(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Vec<Assessment>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.list_assessments(&id)).await?,
    ))
}

async fn claim_work(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ClaimWorkRequest>,
) -> ApiResult<ClaimOutcome> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.claim_work(request)).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct WorkParams {
    agent_id: Option<String>,
    status: Option<String>,
    scope: Option<String>,
    limit: Option<usize>,
}

async fn query_work(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiQuery(params): ApiQuery<WorkParams>,
) -> ApiResult<Vec<WorkItem>> {
    authorize(&state, &headers)?;
    let scope = params
        .scope
        .as_deref()
        .map(Scope::parse)
        .transpose()
        .map_err(ApiError::from)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || {
            service.query_work(WorkQuery {
                agent_id: params.agent_id,
                status: params.status,
                scope,
                limit: params.limit.unwrap_or(50),
            })
        })
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct AgentAction {
    agent_id: String,
}

async fn start_work(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<AgentAction>,
) -> ApiResult<Intent> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.start_work(&request.agent_id, &id)).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct DiscardAction {
    agent_id: String,
    reason: String,
}

async fn discard_work(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<DiscardAction>,
) -> ApiResult<Intent> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.discard_work(&request.agent_id, &id, &request.reason)).await?,
    ))
}

async fn check_conflicts(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ConflictCheckRequest>,
) -> ApiResult<ConflictReport> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.check_conflicts(request)).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct ConflictParams {
    status: Option<String>,
}

async fn list_conflicts(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiQuery(params): ApiQuery<ConflictParams>,
) -> ApiResult<Vec<Conflict>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.list_conflicts(params.status.as_deref())).await?,
    ))
}

async fn conflict_detections(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Vec<ConflictDetection>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.conflict_detections(&id)).await?,
    ))
}

async fn resolve_conflict(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ResolveConflictRequest>,
) -> ApiResult<Conflict> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.resolve_conflict(&id, request)).await?,
    ))
}

async fn publish_changeset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<PublishChangeSetRequest>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.publish_changeset(request)).await?,
    ))
}

async fn get_changeset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.get_changeset(&id)).await?,
    ))
}

async fn validation_attempts(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Vec<ValidationAttempt>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.validation_attempts(&id)).await?,
    ))
}

async fn validate_changeset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ValidationRequest>,
) -> ApiResult<Validation> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .validate_changeset(&id, request)
            .await
            .map_err(ApiError::from)?,
    ))
}

async fn accept_changeset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<AcceptRequest>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.accept_changeset(&id, request)).await?,
    ))
}

async fn record_commit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<RecordCommitRequest>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.record_commit(&id, &request.git_ref)).await?,
    ))
}

async fn coordinate(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CoordinateRequest>,
) -> ApiResult<CoordinationMessage> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || service.coordinate_with_agent(request)).await?,
    ))
}

async fn inbox(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Vec<CoordinationMessage>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(api_blocking(move || service.inbox(&id)).await?))
}

#[derive(Debug, Deserialize)]
struct EventParams {
    after_seq: Option<i64>,
    limit: Option<usize>,
}

async fn events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiQuery(params): ApiQuery<EventParams>,
) -> ApiResult<Vec<Event>> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(
        api_blocking(move || {
            service.events(params.after_seq.unwrap_or(0), params.limit.unwrap_or(100))
        })
        .await?,
    ))
}

async fn graph(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Value> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(api_blocking(move || service.graph()).await?))
}

async fn status(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<StatusReport> {
    authorize(&state, &headers)?;
    let service = state.service;
    Ok(success(api_blocking(move || service.status()).await?))
}

fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(());
    };
    let supplied = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .and_then(|(scheme, credentials)| {
            let credentials = credentials.trim();
            (scheme.eq_ignore_ascii_case("bearer") && !credentials.is_empty())
                .then_some(credentials)
        });
    if supplied.is_some_and(|supplied| constant_time_token_eq(supplied, expected)) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "UNAUTHORIZED".to_string(),
            message: "missing or invalid bearer token".to_string(),
        })
    }
}

fn constant_time_token_eq(supplied: &str, expected: &str) -> bool {
    let supplied = Sha256::digest(supplied.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    supplied
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;
    use tower::ServiceExt;

    async fn loopback_listener() -> (tokio::net::TcpListener, SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let address = listener.local_addr().expect("resolve listener address");
        (listener, address)
    }

    #[tokio::test]
    async fn a_completed_drain_reports_a_clean_shutdown() {
        let (listener, _address) = loopback_listener().await;
        let (trigger, shutdown) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_shutdown(
            listener,
            Router::new().route("/quick", get(|| async { "ok" })),
            SHUTDOWN_GRACE,
            async move {
                let _ = shutdown.await;
            },
        ));
        trigger.send(()).expect("request shutdown");
        let outcome = server
            .await
            .expect("server task")
            .expect("server shutdown cleanly");
        assert_eq!(outcome, ShutdownOutcome::Drained);
        assert!(!outcome.grace_expired());
    }

    #[tokio::test]
    async fn an_expired_grace_is_reported_instead_of_waiting_for_the_request() {
        let (listener, address) = loopback_listener().await;
        // The handler blocks far longer than the grace, so the drain cannot
        // finish and the bound must be the thing that ends `serve`.
        let started = Arc::new(tokio::sync::Notify::new());
        let handler_started = started.clone();
        let router = Router::new().route(
            "/slow",
            get(move || {
                let started = handler_started.clone();
                async move {
                    started.notify_one();
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    "never"
                }
            }),
        );
        let (trigger, shutdown) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_shutdown(
            listener,
            router,
            Duration::from_millis(100),
            async move {
                let _ = shutdown.await;
            },
        ));
        // Hold the connection open for the whole test: dropping it early would
        // let the server finish the drain for the wrong reason.
        let mut connection = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect to the API");
        connection
            .write_all(b"GET /slow HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("send a request that never completes");
        started.notified().await;
        trigger.send(()).expect("request shutdown");
        let outcome = server
            .await
            .expect("server task")
            .expect("server shutdown without a transport error");
        assert_eq!(outcome, ShutdownOutcome::GraceExpired);
        assert!(outcome.grace_expired());
        drop(connection);
    }

    #[tokio::test]
    async fn health_is_live_json_and_does_not_require_token() {
        let store = Store::in_memory().unwrap();
        let service = Foremerge::new(store.clone());
        // If liveness accidentally touches SQLite this held mutex makes the
        // request hang, while readiness must fail fast instead of queueing.
        let (locked_sender, locked_receiver) = std::sync::mpsc::sync_channel(0);
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let lock_store = store.clone();
        let lock_thread = std::thread::spawn(move || {
            let _guard = lock_store.lock().unwrap();
            locked_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
        });
        locked_receiver.recv().unwrap();
        let app = router(ApiState {
            service: service.clone(),
            token: Some("secret".to_string()),
        });
        let response = tokio::time::timeout(
            Duration::from_millis(250),
            app.oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("liveness must not wait for the store")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["name"], "foremerge");

        let ready = router(ApiState {
            service,
            token: Some("secret".to_string()),
        })
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        release_sender.send(()).unwrap();
        lock_thread.join().unwrap();
    }

    #[tokio::test]
    async fn event_chain_audit_is_authenticated() {
        let state = ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        };
        let unauthorized = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/audit/event-chain?page_size=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/audit/event-chain?page_size=1")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        let body = authorized.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["data"]["valid"], true);
    }

    #[tokio::test]
    async fn protected_routes_require_token() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/work")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_auth_scheme_is_case_insensitive() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/work")
                    .header("authorization", "bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn authentication_runs_before_body_deserialization() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/agents/register")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":123}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn routing_failures_use_the_error_envelope() {
        let state = ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        };
        let wrong_method = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/work")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = wrong_method.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");

        let unknown = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let body = unknown.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn malformed_json_uses_the_error_envelope() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/agents/register")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":123}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "INVALID_INPUT");
    }

    #[tokio::test]
    async fn malformed_query_uses_the_error_envelope() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/work?limit=nope")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "INVALID_INPUT");
    }
}
