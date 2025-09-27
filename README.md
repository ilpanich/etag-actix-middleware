# etag-actix-middleware

A lightweight Actix Web middleware that adds ETag headers to outgoing responses and enforces conditional request semantics for `If-Match` and `If-None-Match`. Responses delivered through the middleware gain automatic cache validation, while stale or conflicting client representations are rejected early with the appropriate HTTP status codes.

## Features
- Computes deterministic strong ETags over the materialized response body.
- Optional weak ETag emission (`ETag::weak()`) for resources that cannot guarantee byte-identical representations.
- Preserves existing `ETag` headers when a handler has already set one.
- Implements specification-aligned handling for `If-Match` and `If-None-Match` across all HTTP methods.
- Works transparently with any handler returning a `MessageBody`, including JSON, templates, and binary payloads.

## Getting Started
Add the crate to your project and wrap your `App` (or specific scopes) with the middleware:

```rust
use actix_web::{web, App, HttpResponse, HttpServer};
use etag_actix_middleware::ETag;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new()
            .wrap(ETag::strong())
            .route("/hello", web::get().to(|| async { HttpResponse::Ok().body("hello") }))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
```

When the returned body matches a cached representation identified by the client's `If-None-Match`, the middleware short-circuits with `304 Not Modified`. On conflicting `If-Match` headers it returns `412 Precondition Failed`, preventing accidental overwrites. Use `ETag::weak()` wherever strong validators may be inappropriate (for example, dynamically generated pages that vary slightly between requests); note that weak tags cannot satisfy `If-Match` comparisons by design, so conflicting edits will still fail with `412`.

## Development
Use the standard cargo workflow:
- `cargo fmt` — format the code before committing.
- `cargo clippy --all-targets --all-features` — lint for potential issues.
- `cargo test` — run the suite, including integration-style middleware checks.

The core implementation lives in `src/lib.rs`. Tests under `#[cfg(test)]` exercise successful ETag injection and the precondition failure paths so you can iterate confidently.

## Notes
- The middleware buffers the full body to compute its hash. For streaming or very large responses, consider splitting those routes into dedicated services or extending the crate with a configurable hashing strategy.
- Weak ETags (`W/""`) from upstream handlers are respected but interpreted in a strong comparison when evaluating `If-None-Match` to keep cache behavior predictable.

## Credits
For some implementation I took inspiration from an existing middleware (unfortunately not working properly with my code): [actix-middleware-etag](https://github.com/Unleash/actix-middleware-etag)

## Support the project
Since I work as developer and manager during the day, I will develop this project during the night and weekends, once I have time.
If you want to support the project bringing me a coffee to keep me awake and coding, you can do it by clicking the link below:

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/ilpanich)

Thank you for your support!
