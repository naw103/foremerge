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
use std::net::SocketAddr;

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
        .route("/v1/agents/{id}/inbox", get(inbox))
        .route("/v1/intents", post(publish_intent))
        .route("/v1/claims", post(claim_work))
        .route("/v1/work", get(query_work))
        .route("/v1/work/{id}/start", post(start_work))
        .route("/v1/work/{id}/discard", post(discard_work))
        .route("/v1/conflicts", get(list_conflicts))
        .route("/v1/conflicts/check", post(check_conflicts))
        .route("/v1/conflicts/{id}/resolve", post(resolve_conflict))
        .route("/v1/changesets", post(publish_changeset))
        .route("/v1/changesets/{id}/validate", post(validate_changeset))
        .route("/v1/changesets/{id}/accept", post(accept_changeset))
        .route("/v1/changesets/{id}/commit", post(record_commit))
        .route("/v1/coordinate", post(coordinate))
        .route("/v1/events", get(events))
        .route("/v1/graph", get(graph))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/healthz", get(health))
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

pub async fn serve(state: ApiState, bind: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "Foremerge API listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn health(State(state): State<ApiState>) -> ApiResult<Value> {
    let counts = state.service.store().counts().map_err(ApiError::from)?;
    let event_chain_ok = state
        .service
        .store()
        .verify_event_chain()
        .map_err(ApiError::from)?;
    Ok(success(json!({
        "name": "foremerge",
        "version": env!("CARGO_PKG_VERSION"),
        "status": if event_chain_ok { "ok" } else { "degraded" },
        "event_chain_ok": event_chain_ok,
        "counts": counts,
    })))
}

async fn register_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<RegisterAgentRequest>,
) -> ApiResult<Agent> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .register_agent(request)
            .map_err(ApiError::from)?,
    ))
}

async fn publish_intent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<PublishIntentRequest>,
) -> ApiResult<PublishIntentOutcome> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .publish_intent(request)
            .map_err(ApiError::from)?,
    ))
}

async fn claim_work(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ClaimWorkRequest>,
) -> ApiResult<ClaimOutcome> {
    authorize(&state, &headers)?;
    Ok(success(
        state.service.claim_work(request).map_err(ApiError::from)?,
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
    Ok(success(
        state
            .service
            .query_work(WorkQuery {
                agent_id: params.agent_id,
                status: params.status,
                scope,
                limit: params.limit.unwrap_or(50),
            })
            .map_err(ApiError::from)?,
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
    Ok(success(
        state
            .service
            .start_work(&request.agent_id, &id)
            .map_err(ApiError::from)?,
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
    Ok(success(
        state
            .service
            .discard_work(&request.agent_id, &id, &request.reason)
            .map_err(ApiError::from)?,
    ))
}

async fn check_conflicts(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ConflictCheckRequest>,
) -> ApiResult<ConflictReport> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .check_conflicts(request)
            .map_err(ApiError::from)?,
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
    Ok(success(
        state
            .service
            .list_conflicts(params.status.as_deref())
            .map_err(ApiError::from)?,
    ))
}

async fn resolve_conflict(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ResolveConflictRequest>,
) -> ApiResult<Conflict> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .resolve_conflict(&id, request)
            .map_err(ApiError::from)?,
    ))
}

async fn publish_changeset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<PublishChangeSetRequest>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .publish_changeset(request)
            .map_err(ApiError::from)?,
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
    Ok(success(
        state
            .service
            .accept_changeset(&id, request)
            .map_err(ApiError::from)?,
    ))
}

async fn record_commit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<RecordCommitRequest>,
) -> ApiResult<ChangeSet> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .record_commit(&id, &request.git_ref)
            .map_err(ApiError::from)?,
    ))
}

async fn coordinate(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CoordinateRequest>,
) -> ApiResult<CoordinationMessage> {
    authorize(&state, &headers)?;
    Ok(success(
        state
            .service
            .coordinate_with_agent(request)
            .map_err(ApiError::from)?,
    ))
}

async fn inbox(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Vec<CoordinationMessage>> {
    authorize(&state, &headers)?;
    Ok(success(state.service.inbox(&id).map_err(ApiError::from)?))
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
    Ok(success(
        state
            .service
            .events(params.after_seq.unwrap_or(0), params.limit.unwrap_or(100))
            .map_err(ApiError::from)?,
    ))
}

async fn graph(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Value> {
    authorize(&state, &headers)?;
    Ok(success(state.service.graph().map_err(ApiError::from)?))
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
    if supplied == Some(expected) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "UNAUTHORIZED".to_string(),
            message: "missing or invalid bearer token".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_is_live_json_and_does_not_require_token() {
        let app = router(ApiState {
            service: Foremerge::new(Store::in_memory().unwrap()),
            token: Some("secret".to_string()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["name"], "foremerge");
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
