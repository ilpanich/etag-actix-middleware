//! Actix middleware that computes strong ETags for responses and enforces
//! conditional request semantics for `If-Match` and `If-None-Match` headers.
//!
//! Wrap your Actix `App` with [`ETag`] to automatically add ETag headers to
//! successful responses and to short-circuit requests when the client's cached
//! representation is still current. The middleware emits strong ETags by
//! default; call [`ETag::weak`] when you need weak validators instead.
//!
//! # Examples
//!
//! ```rust
//! use actix_web::{web, App, HttpResponse, test, dev::Service};
//! use etag_actix_middleware::ETag;
//!
//! # actix_web::rt::System::new().block_on(async move {
//! let mut app = test::init_service(
//!     App::new()
//!         .wrap(ETag::strong())
//!         .route("/", web::get().to(|| async { HttpResponse::Ok().body("hello") }))
//! ).await;
//!
//! let response = test::call_service(&mut app, test::TestRequest::get().uri("/").to_request()).await;
//! assert_eq!(response.status(), actix_web::http::StatusCode::OK);
//! assert!(response.headers().contains_key(actix_web::http::header::ETAG));
//! # });
//! ```
//!
//! ```rust
//! use actix_web::{web, App, HttpResponse, test, dev::Service};
//! use etag_actix_middleware::ETag;
//!
//! # actix_web::rt::System::new().block_on(async move {
//! let mut app = test::init_service(
//!     App::new()
//!         .wrap(ETag::weak())
//!         .route("/", web::get().to(|| async { HttpResponse::Ok().body("hello") }))
//! ).await;
//!
//! // First response provides the current weak ETag.
//! let initial = test::call_service(&mut app, test::TestRequest::get().uri("/").to_request()).await;
//! let etag = initial.headers().get(actix_web::http::header::ETAG).unwrap().clone();
//!
//! // Revalidation request with If-None-Match short-circuits to 304 Not Modified.
//! let request = test::TestRequest::get()
//!     .uri("/")
//!     .insert_header((actix_web::http::header::IF_NONE_MATCH, etag))
//!     .to_request();
//! let response = test::call_service(&mut app, request).await;
//! assert_eq!(response.status(), actix_web::http::StatusCode::NOT_MODIFIED);
//! # });
//! ```

use actix_web::{
    Error, HttpResponse,
    body::{BoxBody, MessageBody, to_bytes},
    dev::{Service, ServiceRequest, ServiceResponse, Transform, forward_ready},
    http::{Method, StatusCode, header},
    web::Bytes,
};
use futures_util::future::{LocalBoxFuture, Ready, ok};
use std::rc::Rc;

use crc32fast::Hasher;

/// Middleware that injects ETag headers and evaluates conditional requests.
///
/// Use [`ETag::strong`] (default) or [`ETag::weak`] depending on whether your
/// handlers should produce strong or weak validators.
#[derive(Clone, Copy)]
pub struct ETag {
    strength: Strength,
}

#[derive(Clone, Copy)]
enum Strength {
    Strong,
    Weak,
}

impl ETag {
    /// Constructs middleware using the default strong ETag strategy.
    pub const fn new() -> Self {
        Self::strong()
    }

    /// Constructs middleware that emits strong ETags (the default behaviour).
    pub const fn strong() -> Self {
        Self {
            strength: Strength::Strong,
        }
    }

    /// Constructs middleware that emits weak ETags while still honouring
    /// conditional request handling.
    pub const fn weak() -> Self {
        Self {
            strength: Strength::Weak,
        }
    }
}

impl Default for ETag {
    fn default() -> Self {
        Self::strong()
    }
}

impl<S, B> Transform<S, ServiceRequest> for ETag
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
    B::Error: Into<Error>,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type InitError = ();
    type Transform = ETagMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(ETagMiddleware {
            service: Rc::new(service),
            strength: self.strength,
        })
    }
}

/// Internal service wrapper that materializes response bodies before hashing.
pub struct ETagMiddleware<S> {
    service: Rc<S>,
    strength: Strength,
}

impl<S, B> Service<ServiceRequest> for ETagMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
    B::Error: Into<Error>,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let srv = Rc::clone(&self.service);
        let strength = self.strength;

        Box::pin(async move {
            let res = srv.call(req).await?;
            let (req, res) = res.into_parts();
            let (mut head, body) = res.into_parts();
            let body_bytes = to_bytes(body).await.map_err(Into::into)?;

            let etag_value = extract_or_compute_etag(&mut head, &body_bytes, strength);

            if let Some(precondition) = evaluate_conditionals(&req, &etag_value) {
                return Ok(ServiceResponse::new(req, precondition));
            }

            let response = head.set_body(body_bytes).map_body(|_, body| body.boxed());

            Ok(ServiceResponse::new(req, response))
        })
    }
}

fn extract_or_compute_etag(
    head: &mut HttpResponse<()>,
    body: &Bytes,
    strength: Strength,
) -> String {
    if let Some(value) = head
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
    {
        return value.trim().to_string();
    }

    let value = build_entity_tag(body, strength);

    if let Ok(header_value) = header::HeaderValue::from_str(&value) {
        head.headers_mut().insert(header::ETAG, header_value);
    }

    value
}

/// Applies `If-Match`/`If-None-Match` rules and returns a short-circuit response when
/// the request preconditions resolve without reaching the wrapped service.
fn evaluate_conditionals(req: &actix_web::HttpRequest, etag: &str) -> Option<HttpResponse> {
    if let Some(if_match) = req
        .headers()
        .get(header::IF_MATCH)
        .and_then(|h| h.to_str().ok())
    {
        if !match_if_match(etag, if_match) {
            return Some(
                HttpResponse::build(StatusCode::PRECONDITION_FAILED)
                    .insert_header((header::ETAG, etag.to_string()))
                    .finish(),
            );
        }
    }

    if let Some(if_none_match) = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
    {
        if match_if_none_match(etag, if_none_match) {
            let status = match *req.method() {
                Method::GET | Method::HEAD => StatusCode::NOT_MODIFIED,
                _ => StatusCode::PRECONDITION_FAILED,
            };

            return Some(
                HttpResponse::build(status)
                    .insert_header((header::ETAG, etag.to_string()))
                    .finish(),
            );
        }
    }

    None
}

fn match_if_match(etag: &str, header_value: &str) -> bool {
    header_value
        .split(',')
        .map(|value| value.trim())
        .any(|value| value == "*" || strong_compare(value, etag))
}

fn match_if_none_match(etag: &str, header_value: &str) -> bool {
    let etag_core = strip_weak_prefix(etag);

    header_value
        .split(',')
        .map(|value| value.trim())
        .any(|value| {
            if value == "*" {
                return true;
            }

            strip_weak_prefix(value) == etag_core
        })
}

fn build_entity_tag(body: &Bytes, strength: Strength) -> String {
    let mut hasher = Hasher::new();
    hasher.update(body);
    let digest = format!("{:x}", hasher.finalize());

    match strength {
        Strength::Strong => format!("\"{}\"", digest),
        Strength::Weak => format!("W/\"{}\"", digest),
    }
}

fn strong_compare(left: &str, right: &str) -> bool {
    !is_weak(left) && !is_weak(right) && left == right
}

fn strip_weak_prefix(value: &str) -> &str {
    value.strip_prefix("W/").unwrap_or(value)
}

fn is_weak(value: &str) -> bool {
    value.starts_with("W/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        App, HttpResponse,
        dev::ServiceResponse,
        http::header,
        test::{TestRequest, call_service, init_service},
        web,
    };

    fn expected_etag(payload: &[u8], strength: Strength) -> String {
        let bytes = Bytes::copy_from_slice(payload);
        build_entity_tag(&bytes, strength)
    }

    #[actix_web::test]
    async fn sets_etag_header_when_missing() {
        let app = init_service(App::new().wrap(ETag::strong()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let response: ServiceResponse =
            call_service(&app, TestRequest::get().uri("/").to_request()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let value = response.headers().get(header::ETAG).unwrap();
        assert_eq!(
            value.to_str().unwrap(),
            expected_etag(b"hello", Strength::Strong)
        );
    }

    #[actix_web::test]
    async fn returns_not_modified_for_matching_if_none_match() {
        let etag = expected_etag(b"hello", Strength::Strong);
        let app = init_service(App::new().wrap(ETag::strong()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let request = TestRequest::get()
            .uri("/")
            .insert_header((header::IF_NONE_MATCH, etag.clone()))
            .to_request();
        let response: ServiceResponse = call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            etag
        );
    }

    #[actix_web::test]
    async fn returns_precondition_failed_for_non_matching_if_match() {
        let app = init_service(App::new().wrap(ETag::strong()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let request = TestRequest::get()
            .uri("/")
            .insert_header((header::IF_MATCH, "\"deadbeef\""))
            .to_request();
        let response: ServiceResponse = call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[actix_web::test]
    async fn allows_if_match_when_strong_tag_matches() {
        let body = b"hello";
        let expected = expected_etag(body, Strength::Strong);
        let app = init_service(App::new().wrap(ETag::strong()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let request = TestRequest::get()
            .uri("/")
            .insert_header((header::IF_MATCH, expected.clone()))
            .to_request();
        let response: ServiceResponse = call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            expected
        );
    }

    #[actix_web::test]
    async fn sets_weak_etag_header_when_configured() {
        let app = init_service(App::new().wrap(ETag::weak()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let response: ServiceResponse =
            call_service(&app, TestRequest::get().uri("/").to_request()).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            expected_etag(b"hello", Strength::Weak)
        );
    }

    #[actix_web::test]
    async fn weak_etag_triggers_not_modified_with_strong_if_none_match() {
        let etag = expected_etag(b"hello", Strength::Weak);
        let app = init_service(App::new().wrap(ETag::weak()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let request = TestRequest::get()
            .uri("/")
            .insert_header((header::IF_NONE_MATCH, etag.trim_start_matches("W/")))
            .to_request();
        let response: ServiceResponse = call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            etag
        );
    }

    #[actix_web::test]
    async fn weak_etag_fails_if_match_even_when_value_matches() {
        let etag = expected_etag(b"hello", Strength::Weak);
        let app = init_service(App::new().wrap(ETag::weak()).route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("hello") }),
        ))
        .await;

        let request = TestRequest::get()
            .uri("/")
            .insert_header((header::IF_MATCH, etag.clone()))
            .to_request();
        let response: ServiceResponse = call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }
}
