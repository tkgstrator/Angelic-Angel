//! HTTP server: local-only JSON API plus static WebUI assets.

mod error;
pub mod following;
pub mod service;

use crate::config::ConfigLock;
use crate::error::{AngelicAngelError, Result};
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::{FromRequest, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use error::ApiError;
use serde::de::DeserializeOwned;
use service::{Backend, LiveBackend, Service, SettingsUpdate};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower_http::services::{ServeDir, ServeFile};

const MAX_BODY: usize = 16 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

type Shared<B> = Arc<Service<B>>;
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

/// JSON body extractor that reports failures in the API error shape.
struct ApiJson<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, ApiError> {
        let is_json = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.trim()
                    .to_ascii_lowercase()
                    .starts_with("application/json")
            });
        if !is_json {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Requests must be application/json.",
            ));
        }
        let bytes = Bytes::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                payload_too_large()
            } else {
                ApiError::invalid("invalid_body", "The request body could not be read.")
            }
        })?;
        serde_json::from_slice(&bytes)
            .map(ApiJson)
            .map_err(|_| ApiError::invalid("invalid_body", "The request body is not valid."))
    }
}

fn payload_too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "The request body is too large.",
    )
}

// ---- handlers ---------------------------------------------------------------

async fn health() -> Response {
    let mut response = Json(serde_json::json!({ "status": "ok" })).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn get_settings<B: Backend>(State(s): State<Shared<B>>) -> ApiResult<service::SettingsDto> {
    Ok(Json(s.settings()?))
}

async fn put_settings<B: Backend>(
    State(s): State<Shared<B>>,
    ApiJson(update): ApiJson<SettingsUpdate>,
) -> ApiResult<service::SettingsDto> {
    Ok(Json(s.update_settings(update).await?))
}

async fn validate_settings<B: Backend>(
    State(s): State<Shared<B>>,
) -> ApiResult<service::CredentialCheckDto> {
    Ok(Json(s.validate_credentials().await?))
}

async fn get_registration<B: Backend>(
    State(s): State<Shared<B>>,
) -> ApiResult<service::RegistrationDto> {
    Ok(Json(s.registration()?))
}

async fn register<B: Backend>(State(s): State<Shared<B>>) -> ApiResult<service::RegistrationDto> {
    Ok(Json(s.register().await?))
}

async fn unregister<B: Backend>(State(s): State<Shared<B>>) -> ApiResult<service::RegistrationDto> {
    Ok(Json(s.unregister().await?))
}

async fn get_status<B: Backend>(State(s): State<Shared<B>>) -> Json<service::StatusDto> {
    Json(s.status())
}

async fn get_following_notifications<B: Backend>(
    State(s): State<Shared<B>>,
) -> Json<following::FollowingNotificationsDto> {
    Json(s.following_notifications())
}

async fn start_following_notifications<B: Backend>(
    State(s): State<Shared<B>>,
) -> std::result::Result<(StatusCode, Json<following::FollowingNotificationsDto>), ApiError> {
    Ok((
        StatusCode::ACCEPTED,
        Json(s.start_following_notifications()?),
    ))
}

async fn cancel_following_notifications<B: Backend>(
    State(s): State<Shared<B>>,
) -> (StatusCode, Json<following::FollowingNotificationsDto>) {
    let (requested, snapshot) = s.cancel_following_notifications();
    let status = if requested {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (status, Json(snapshot))
}

const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 100;
const MAX_CURSOR_LEN: usize = 512;

struct PageQuery {
    before: Option<String>,
    limit: usize,
}

fn invalid_query(message: &'static str) -> ApiError {
    ApiError::invalid("invalid_query", message)
}

/// Strict `before` / `limit` parsing: unknown or repeated parameters are errors.
fn parse_page_query(raw: Option<&str>) -> std::result::Result<PageQuery, ApiError> {
    let mut before = None;
    let mut limit = None;
    if let Some(raw) = raw.filter(|q| !q.is_empty()) {
        let mut url = reqwest::Url::parse("http://localhost/")
            .map_err(|_| invalid_query("The query string is not valid."))?;
        url.set_query(Some(raw));
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "before" if before.is_none() => {
                    if value.is_empty()
                        || value.len() > MAX_CURSOR_LEN
                        || value.chars().any(|c| c.is_control())
                    {
                        return Err(invalid_query("before is not a valid cursor."));
                    }
                    before = Some(value.into_owned());
                },
                "limit" if limit.is_none() => {
                    let parsed = value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=MAX_PAGE_LIMIT).contains(n))
                        .ok_or_else(|| {
                            invalid_query("limit must be an integer between 1 and 100.")
                        })?;
                    limit = Some(parsed);
                },
                _ => return Err(invalid_query("Unknown or repeated query parameter.")),
            }
        }
    }
    Ok(PageQuery {
        before,
        limit: limit.unwrap_or(DEFAULT_PAGE_LIMIT),
    })
}

async fn get_notifications<B: Backend>(
    State(s): State<Shared<B>>,
    req: Request,
) -> ApiResult<crate::notifications::NotificationPage> {
    let query = parse_page_query(req.uri().query())?;
    Ok(Json(s.notifications(query.before, query.limit).await?))
}

async fn api_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "No such API endpoint.")
}

// ---- middleware -------------------------------------------------------------

/// Cross-origin rejection, size cap and `no-store`.
async fn api_guard(req: Request, next: Next) -> Response {
    let mut response = match check_request(&req) {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    };
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn is_cross_origin(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"cross-site"))
    {
        return true;
    }
    let Some(origin) = headers.get(ORIGIN) else {
        return false;
    };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://").map(|(_, rest)| rest))
        .map(|rest| rest.trim_end_matches('/'));
    let host = headers.get(HOST).and_then(|h| h.to_str().ok());
    !matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

fn check_request(req: &Request) -> std::result::Result<(), ApiError> {
    let headers = req.headers();
    if is_cross_origin(headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "Cross-origin requests are not allowed.",
        ));
    }

    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|len| len > MAX_BODY) {
        return Err(payload_too_large());
    }
    Ok(())
}

async fn static_headers(req: Request, next: Next) -> Response {
    let hashed = req.uri().path().starts_with("/assets/");
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if response.status().is_success() {
        let value = if hashed {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static(value));
    }
    response
}

async fn missing_assets() -> Response {
    let body = Html(
        "<!doctype html><meta charset=\"utf-8\"><title>Angelic Angel</title>\
         <h1>Web assets not found</h1>\
         <p>The WebUI files could not be located. Build the frontend or set \
         <code>WEBUI_ASSETS_DIR</code> to the directory containing <code>index.html</code>. \
         The API remains available.</p>",
    );
    (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
}

// ---- router -----------------------------------------------------------------

pub fn router<B: Backend>(service: Shared<B>, assets_dir: &Path) -> Router {
    let api = Router::new()
        .route(
            "/api/settings",
            get(get_settings::<B>).put(put_settings::<B>),
        )
        .route("/api/settings/validate", post(validate_settings::<B>))
        .route(
            "/api/registration",
            get(get_registration::<B>).delete(unregister::<B>),
        )
        .route("/api/register", post(register::<B>))
        .route("/api/status", get(get_status::<B>))
        .route("/api/notifications", get(get_notifications::<B>))
        .route(
            "/api/following-notifications",
            get(get_following_notifications::<B>)
                .post(start_following_notifications::<B>)
                .delete(cancel_following_notifications::<B>),
        )
        .route("/api", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .with_state(service)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(api_guard));

    let index = assets_dir.join("index.html");
    let assets = if index.is_file() {
        let spa = ServeFile::new(&index)
            .precompressed_gzip()
            .precompressed_br();
        let dir = ServeDir::new(assets_dir)
            .precompressed_gzip()
            .precompressed_br()
            .fallback(spa);
        Router::new().fallback_service(dir)
    } else {
        tracing::warn!("web assets not found; serving the API only");
        Router::new().fallback(missing_assets)
    }
    .layer(middleware::from_fn(static_headers));

    Router::new()
        .route("/health", get(health))
        .merge(api)
        .fallback_service(assets)
}

// ---- entry points -----------------------------------------------------------

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = term.recv() => {},
                }
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            },
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Runs the WebUI server until SIGINT/SIGTERM, then stops the owned receiver.
pub async fn serve(config_path: PathBuf, bind: String, assets_dir: PathBuf) -> Result<()> {
    let _lock = ConfigLock::acquire(&config_path)?;

    let service = Arc::new(Service::new(LiveBackend::default(), config_path));
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| AngelicAngelError::Config(format!("failed to bind {bind}: {e}")))?;
    let app = router(Arc::clone(&service), &assets_dir);

    service.startup().await;
    eprintln!("listening on http://{}", listener.local_addr()?);

    let stop = Arc::new(tokio::sync::Notify::new());
    let trigger = Arc::clone(&stop);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        trigger.notify_one();
    });
    let result = tokio::select! {
        result = server => result,
        _ = async {
            stop.notified().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => Ok(()),
    };

    service.shutdown().await;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::service::fake::FakeBackend;
    use super::*;
    use crate::config::{self, Config};
    use serde_json::{Value, json};

    struct Harness {
        base: String,
        client: reqwest::Client,
        config_path: PathBuf,
        assets: PathBuf,
        fake: Arc<FakeBackend>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn harness(with_assets: bool) -> Harness {
        let dir = config::test_temp_dir();
        let assets = dir.join("dist");
        if with_assets {
            std::fs::create_dir_all(assets.join("assets")).unwrap();
            std::fs::write(assets.join("index.html"), "<html>INDEX</html>").unwrap();
            std::fs::write(assets.join("assets/app-abc.js"), "console.log(1)").unwrap();
        }
        let config_path = dir.join("c.toml");
        let fake = Arc::new(FakeBackend::default());
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(crate::twitter::FollowingPage {
                ids: vec!["1".into(), "2".into(), "3".into()],
                next_cursor: 0,
            }),
        );
        let service = Arc::new(
            Service::new(Arc::clone(&fake), config_path.clone()).with_following_timings(
                following::Timings {
                    request_timeout: Duration::from_millis(150),
                    interval: Duration::from_millis(1),
                    max_duration: Duration::from_secs(30),
                },
            ),
        );
        let app = router(service, &assets);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Harness {
            base,
            client: reqwest::Client::new(),
            config_path,
            assets,
            fake,
            _task: task,
        }
    }

    impl Harness {
        fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
            self.client
                .request(method, format!("{}{}", self.base, path))
        }
    }

    fn setup_body() -> Value {
        json!({"auth_token":"AUTH-SECRET","ct0":"CT0-SECRET","webhook_endpoint":"https://example.com/h"})
    }

    #[tokio::test]
    async fn api_and_health_need_no_credentials() {
        let h = harness(true).await;
        let health = h
            .client
            .get(format!("{}/health", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);

        let ok = h
            .req(reqwest::Method::GET, "/api/settings")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.headers()["cache-control"], "no-store");
        let body: Value = ok.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert_eq!(body["webhook_source"], "none");
        assert_eq!(body["webhook_endpoint"], Value::Null);
    }

    #[tokio::test]
    async fn api_misses_are_json_404_and_spa_routes_fall_back() {
        let h = harness(true).await;
        let miss = h
            .req(reqwest::Method::GET, "/api/nope")
            .send()
            .await
            .unwrap();
        assert_eq!(miss.status(), 404);
        let body: Value = miss.json().await.unwrap();
        assert_eq!(body["error"]["code"], "not_found");

        let spa = h
            .client
            .get(format!("{}/settings/deep", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(spa.status(), 200);
        assert_eq!(spa.headers()["cache-control"], "no-cache");
        assert!(spa.text().await.unwrap().contains("INDEX"));

        let asset = h
            .client
            .get(format!("{}/assets/app-abc.js", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(asset.status(), 200);
        assert!(
            asset.headers()["cache-control"]
                .to_str()
                .unwrap()
                .contains("immutable")
        );
    }

    #[tokio::test]
    async fn missing_assets_report_clearly_while_api_works() {
        let h = harness(false).await;
        assert!(!h.assets.exists());
        let page = h.client.get(format!("{}/", h.base)).send().await.unwrap();
        assert_eq!(page.status(), 503);
        assert!(page.text().await.unwrap().contains("WEBUI_ASSETS_DIR"));
        let ok = h
            .req(reqwest::Method::GET, "/api/status")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let body: Value = ok.json().await.unwrap();
        assert_eq!(body["state"], "unconfigured");
        assert_eq!(body["received_count"], 0);
    }

    #[tokio::test]
    async fn cross_origin_requests_are_rejected() {
        let h = harness(true).await;
        let host = h.base.trim_start_matches("http://").to_string();

        let bad = h
            .req(reqwest::Method::PUT, "/api/settings")
            .header("Origin", "http://evil.example")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 403);
        assert!(!h.config_path.exists());

        let good = h
            .req(reqwest::Method::PUT, "/api/settings")
            .header("Origin", format!("http://{host}"))
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        assert_eq!(good.status(), 200);

        for path in [
            "/api/settings",
            "/api/registration",
            "/api/status",
            "/api/notifications",
            "/api/following-notifications",
            "/api/nope",
        ] {
            let read = h
                .req(reqwest::Method::GET, path)
                .header("Origin", "http://evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(read.status(), 403, "{path}");
            assert_eq!(read.headers()["cache-control"], "no-store");
            let body: Value = read.json().await.unwrap();
            assert_eq!(body["error"]["code"], "origin_mismatch");
            assert!(!body.to_string().contains("SECRET"));
        }

        let same_origin_read = h
            .req(reqwest::Method::GET, "/api/settings")
            .header("Origin", format!("http://{host}"))
            .header("Sec-Fetch-Site", "same-origin")
            .send()
            .await
            .unwrap();
        assert_eq!(same_origin_read.status(), 200);

        let direct_navigation = h
            .req(reqwest::Method::GET, "/api/settings")
            .header("Sec-Fetch-Site", "none")
            .send()
            .await
            .unwrap();
        assert_eq!(direct_navigation.status(), 200);
    }

    #[tokio::test]
    async fn cross_site_fetch_metadata_is_rejected_for_every_method() {
        let h = harness(true).await;
        for method in [
            reqwest::Method::GET,
            reqwest::Method::PUT,
            reqwest::Method::POST,
            reqwest::Method::DELETE,
        ] {
            let resp = h
                .req(method, "/api/registration")
                .header("Sec-Fetch-Site", "cross-site")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403);
        }
        assert!(!h.config_path.exists());
    }

    #[tokio::test]
    async fn body_limit_and_json_errors_use_error_shape() {
        let h = harness(true).await;
        let big = json!({"webhook_endpoint": "x".repeat(20 * 1024)});
        let resp = h
            .req(reqwest::Method::PUT, "/api/settings")
            .json(&big)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "payload_too_large");

        let resp = h
            .req(reqwest::Method::PUT, "/api/settings")
            .header("Content-Type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "invalid_body");

        let resp = h
            .req(reqwest::Method::PUT, "/api/settings")
            .header("Content-Type", "text/plain")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 415);
    }

    #[tokio::test]
    async fn settings_flow_never_returns_secrets() {
        let h = harness(true).await;
        let first_without_ct0 = h
            .req(reqwest::Method::PUT, "/api/settings")
            .json(&json!({"auth_token":"A","webhook_endpoint":""}))
            .send()
            .await
            .unwrap();
        assert_eq!(first_without_ct0.status(), 400);

        let saved = h
            .req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        assert_eq!(saved.status(), 200);
        let text = saved.text().await.unwrap();
        assert!(!text.contains("SECRET"));
        let dto: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(dto["configured"], true);
        assert_eq!(dto["auth_token_set"], true);
        assert_eq!(dto["ct0_set"], true);
        assert_eq!(dto["credentials_locked"], false);
        assert_eq!(dto["webhook_endpoint"], "https://example.com/h");
        assert_eq!(dto["webhook_source"], "config");

        let reg = h
            .req(reqwest::Method::GET, "/api/registration")
            .send()
            .await
            .unwrap();
        let reg: Value = reg.json().await.unwrap();
        assert_eq!(
            reg,
            json!({"registered": false, "uaid": null, "channel_id": null})
        );

        let bad_url = h
            .req(reqwest::Method::PUT, "/api/settings")
            .json(&json!({"webhook_endpoint":"ftp://x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad_url.status(), 400);
        assert_eq!(
            Config::load(&h.config_path).unwrap().twitter.ct0,
            "CT0-SECRET"
        );
    }

    #[tokio::test]
    async fn register_and_unregister_over_http() {
        let h = harness(true).await;
        let no_config = h
            .req(reqwest::Method::POST, "/api/register")
            .send()
            .await
            .unwrap();
        assert_eq!(no_config.status(), 409);

        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        let reg: Value = h
            .req(reqwest::Method::POST, "/api/register")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reg["registered"], true);
        assert!(reg["uaid"].is_string());

        let status: Value = h
            .req(reqwest::Method::GET, "/api/status")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["registered"], true);
        assert_eq!(status["state"], "connected");

        let gone: Value = h
            .req(reqwest::Method::DELETE, "/api/registration")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(gone["registered"], false);
    }

    #[tokio::test]
    async fn malformed_config_is_reported_without_details() {
        let h = harness(true).await;
        std::fs::write(&h.config_path, "this is [not valid toml").unwrap();
        let resp = h
            .req(reqwest::Method::GET, "/api/settings")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        let text = resp.text().await.unwrap();
        assert!(text.contains("config_invalid"));
        assert!(!text.contains("toml"));
        let status: Value = h
            .req(reqwest::Method::GET, "/api/status")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["state"], "error");
    }

    async fn json_of(resp: reqwest::Response) -> Value {
        resp.json().await.unwrap()
    }

    async fn poll_state(h: &Harness, wanted: &[&str]) -> Value {
        for _ in 0..500 {
            let body = json_of(
                h.req(reqwest::Method::GET, "/api/following-notifications")
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            if wanted.contains(&body["state"].as_str().unwrap()) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("state never reached {wanted:?}");
    }

    #[tokio::test]
    async fn following_notifications_endpoint_lifecycle() {
        let h = harness(true).await;
        let idle = h
            .req(reqwest::Method::GET, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(idle.status(), 200);
        assert_eq!(idle.headers()["cache-control"], "no-store");
        assert_eq!(
            json_of(idle).await,
            json!({
                "state": "idle",
                "total": null,
                "success_count": 0,
                "failed_count": 0,
                "remaining_count": null,
                "last_error": null,
            })
        );

        let cancel_idle = h
            .req(reqwest::Method::DELETE, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(cancel_idle.status(), 200);

        let unconfigured = h
            .req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(unconfigured.status(), 400);
        assert_eq!(
            json_of(unconfigured).await["error"]["code"],
            "not_configured"
        );

        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        let started = h
            .req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(started.status(), 202);
        assert_eq!(started.headers()["cache-control"], "no-store");
        let started = json_of(started).await;
        assert_eq!(started["state"], "loading");
        assert_eq!(started["total"], Value::Null);

        let done = poll_state(&h, &["completed", "failed"]).await;
        assert_eq!(done["state"], "completed");
        assert_eq!(done["total"], 3);
        assert_eq!(done["success_count"], 3);
        assert_eq!(done["remaining_count"], 0);
        assert!(!done.to_string().contains("SECRET"));
        assert_eq!(*h.fake.updated.lock().unwrap(), ["1", "2", "3"]);
    }

    #[tokio::test]
    async fn following_notifications_conflict_cancel_and_credential_lock() {
        let h = harness(true).await;
        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        h.fake
            .hang_pages
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let first = h
            .req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 202);
        let second = h
            .req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), 409);
        assert_eq!(
            json_of(second).await["error"]["code"],
            "following_job_active"
        );

        let locked = h
            .req(reqwest::Method::PUT, "/api/settings")
            .json(&json!({"auth_token":"NEW","webhook_endpoint":""}))
            .send()
            .await
            .unwrap();
        assert_eq!(locked.status(), 409);
        assert_eq!(
            Config::load(&h.config_path).unwrap().twitter.auth_token,
            "AUTH-SECRET"
        );

        let cancel = h
            .req(reqwest::Method::DELETE, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), 202);
        assert_eq!(json_of(cancel).await["state"], "cancelling");
        let done = poll_state(&h, &["cancelled", "failed", "completed"]).await;
        assert_eq!(done["state"], "cancelled");
        assert_eq!(done["total"], Value::Null);
        assert_eq!(done["remaining_count"], Value::Null);
        assert!(h.fake.updated.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn following_notifications_surface_x_failures_without_success_spoofing() {
        let h = harness(true).await;
        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        *h.fake.screen_error.lock().unwrap() = Some(crate::twitter::XError::Forbidden);
        h.req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        let done = poll_state(&h, &["completed", "failed", "cancelled"]).await;
        assert_eq!(done["state"], "failed");
        assert!(done["last_error"].as_str().unwrap().contains("403"));
        assert_eq!(done["success_count"], 0);
    }

    #[tokio::test]
    async fn notifications_endpoint_validates_query_and_pages() {
        let h = harness(true).await;
        let empty = h
            .req(reqwest::Method::GET, "/api/notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), 200);
        assert_eq!(empty.headers()["cache-control"], "no-store");
        assert_eq!(
            json_of(empty).await,
            json!({
                "items": [],
                "next_cursor": null,
                "notification_filter": {
                    "tweet": true,
                    "retweet": true,
                    "quote": true,
                    "reply": true,
                    "xchat_message_received": true,
                    "other": true
                }
            })
        );

        for n in 0..3 {
            crate::notifications::append(
                &h.config_path,
                &format!("id-{n}"),
                &json!({"title": format!("t{n}"), "body": "b"}),
            )
            .unwrap();
        }
        let page = json_of(
            h.req(reqwest::Method::GET, "/api/notifications?limit=2")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["items"][0]["id"], "id-2");
        let cursor = page["next_cursor"].as_str().unwrap().to_string();

        let rest = json_of(
            h.req(
                reqwest::Method::GET,
                &format!("/api/notifications?limit=2&before={cursor}"),
            )
            .send()
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(rest["items"].as_array().unwrap().len(), 1);
        assert_eq!(rest["next_cursor"], Value::Null);

        for bad in [
            "limit=0",
            "limit=101",
            "limit=-1",
            "limit=abc",
            "limit=",
            "limit=1&limit=2",
            "before=",
            "before=missing",
            "foo=1",
        ] {
            let resp = h
                .req(reqwest::Method::GET, &format!("/api/notifications?{bad}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{bad}");
            assert_eq!(resp.headers()["cache-control"], "no-store");
            let body = json_of(resp).await;
            assert!(body["error"]["code"].is_string(), "{bad}");
        }

        let max = h
            .req(reqwest::Method::GET, "/api/notifications?limit=100")
            .send()
            .await
            .unwrap();
        assert_eq!(max.status(), 200);
    }

    #[tokio::test]
    async fn notification_filter_is_saved_applied_and_retained_over_http() {
        let h = harness(true).await;
        let all_on = json!({
            "tweet": true,
            "retweet": true,
            "quote": true,
            "reply": true,
            "xchat_message_received": true,
            "other": true
        });

        let first = json_of(
            h.req(reqwest::Method::PUT, "/api/settings")
                .json(&setup_body())
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first["notification_filter"], all_on);

        for (id, kind) in [
            ("id-0", "tweet"),
            ("id-1", "xchat_message_received"),
            ("id-2", "novel"),
        ] {
            crate::notifications::append(
                &h.config_path,
                id,
                &json!({"title": id, "data": {"type": kind}}),
            )
            .unwrap();
        }

        let mut body = setup_body();
        body["notification_filter"] = json!({
            "tweet": false,
            "retweet": false,
            "quote": false,
            "reply": true,
            "xchat_message_received": true,
            "other": false
        });
        let saved = json_of(
            h.req(reqwest::Method::PUT, "/api/settings")
                .json(&body)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(saved["notification_filter"], body["notification_filter"]);

        let page = json_of(
            h.req(reqwest::Method::GET, "/api/notifications")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["notification_filter"], body["notification_filter"]);
        let shown: Vec<_> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["id"].as_str().unwrap(), i["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(shown, [("id-1", "xchat_message_received")]);

        // フィルターを省略した保存では、既存の設定を保持する。
        let kept = json_of(
            h.req(reqwest::Method::PUT, "/api/settings")
                .json(&setup_body())
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(kept["notification_filter"], body["notification_filter"]);
        let got = json_of(
            h.req(reqwest::Method::GET, "/api/settings")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(got["notification_filter"], body["notification_filter"]);

        let full = json!({
            "tweet": true, "retweet": true, "quote": true, "reply": true,
            "xchat_message_received": true, "other": true
        });
        let without = |key: &str| {
            let mut value = full.clone();
            value.as_object_mut().unwrap().remove(key);
            value
        };
        let with = |key: &str, value: serde_json::Value| {
            let mut full = full.clone();
            full[key] = value;
            full
        };
        for bad in [
            json!({"tweet": false}),
            json!({"tweet": true, "xchat_message_received": true, "other": true}),
            json!({"tweet": true, "retweet": true, "xchat_message_received": true, "other": true}),
            without("retweet"),
            without("quote"),
            without("reply"),
            with("tweet", json!("no")),
            with("retweet", json!("no")),
            with("quote", json!("no")),
            with("reply", json!(1)),
            with("extra", json!(1)),
            json!(null),
        ] {
            let mut request = setup_body();
            request["notification_filter"] = bad.clone();
            let resp = h
                .req(reqwest::Method::PUT, "/api/settings")
                .json(&request)
                .send()
                .await
                .unwrap();
            if bad.is_null() {
                assert_eq!(resp.status(), 200, "null is the same as omitted");
            } else {
                assert_eq!(resp.status(), 400, "{bad}");
            }
        }
        let still = json_of(
            h.req(reqwest::Method::GET, "/api/settings")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(still["notification_filter"], body["notification_filter"]);
    }

    #[tokio::test]
    async fn notifications_endpoint_hides_unreadable_history_details() {
        let h = harness(true).await;
        std::fs::write(
            h.config_path.with_file_name("c.toml.notifications.sqlite3"),
            "this is not a sqlite database, SECRET ".repeat(40),
        )
        .unwrap();
        let resp = h
            .req(reqwest::Method::GET, "/api/notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        let text = resp.text().await.unwrap();
        assert!(!text.contains("SECRET"));
        assert!(text.contains("history_unavailable"));
    }

    #[tokio::test]
    async fn new_routes_reject_wrong_methods_and_cross_origin_mutations() {
        let h = harness(true).await;
        let put = h
            .req(reqwest::Method::PUT, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 405);
        let post = h
            .req(reqwest::Method::POST, "/api/notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(post.status(), 405);

        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
            let resp = h
                .req(method, "/api/following-notifications")
                .header("Origin", "http://evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403);
        }
        assert!(h.fake.updated.lock().unwrap().is_empty());
        let state = json_of(
            h.req(reqwest::Method::GET, "/api/following-notifications")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(state["state"], "idle");
    }
    #[tokio::test]
    async fn settings_validate_endpoint_reports_cookie_state() {
        let h = harness(true).await;
        let missing = h
            .req(reqwest::Method::POST, "/api/settings/validate")
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 400);
        assert_eq!(missing.headers()["cache-control"], "no-store");
        assert_eq!(json_of(missing).await["error"]["code"], "not_configured");

        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        let ok = h
            .req(reqwest::Method::POST, "/api/settings/validate")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.headers()["cache-control"], "no-store");
        let text = ok.text().await.unwrap();
        assert!(!text.contains("SECRET"));
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"valid": true, "screen_name": "me", "message": "Cookieを確認しました。"})
        );

        *h.fake.screen_error.lock().unwrap() = Some(crate::twitter::XError::Unauthorized);
        let expired = h
            .req(reqwest::Method::POST, "/api/settings/validate")
            .send()
            .await
            .unwrap();
        assert_eq!(expired.status(), 200);
        assert_eq!(
            json_of(expired).await,
            json!({"valid": false, "screen_name": null, "message": "Cookieが無効か期限切れです。"})
        );

        for error in [
            crate::twitter::XError::RateLimited,
            crate::twitter::XError::Status(500),
            crate::twitter::XError::Transport,
        ] {
            *h.fake.screen_error.lock().unwrap() = Some(error);
            let resp = h
                .req(reqwest::Method::POST, "/api/settings/validate")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 502, "{error:?}");
            let text = resp.text().await.unwrap();
            assert!(!text.contains("SECRET"));
            assert!(text.contains("provider_failure"));
            assert!(!text.contains("\"valid\""));
        }

        let get = h
            .req(reqwest::Method::GET, "/api/settings/validate")
            .send()
            .await
            .unwrap();
        assert_eq!(get.status(), 405);
        let cross = h
            .req(reqwest::Method::POST, "/api/settings/validate")
            .header("Origin", "http://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(cross.status(), 403);
    }

    #[tokio::test]
    async fn settings_validate_conflicts_with_a_running_bulk_job() {
        let h = harness(true).await;
        h.req(reqwest::Method::PUT, "/api/settings")
            .json(&setup_body())
            .send()
            .await
            .unwrap();
        h.fake
            .hang_pages
            .store(true, std::sync::atomic::Ordering::SeqCst);
        h.req(reqwest::Method::POST, "/api/following-notifications")
            .send()
            .await
            .unwrap();

        let resp = h
            .req(reqwest::Method::POST, "/api/settings/validate")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        assert_eq!(json_of(resp).await["error"]["code"], "following_job_active");
        for (method, path) in [
            (reqwest::Method::POST, "/api/register"),
            (reqwest::Method::DELETE, "/api/registration"),
        ] {
            let resp = h.req(method, path).send().await.unwrap();
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(json_of(resp).await["error"]["code"], "following_job_active");
        }
        let resp = h
            .req(reqwest::Method::DELETE, "/api/following-notifications")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        poll_state(&h, &["cancelled", "failed", "completed"]).await;
    }
}
