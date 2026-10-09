//! Embedded, same-origin admin UI. The shell is public on the admin listener;
//! all data and mutations still require the private API's bearer credential.
use axum::{
    Router,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/admin/") }))
        .route("/admin", get(|| async { Redirect::temporary("/admin/") }))
        .route(
            "/admin/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_str!("admin_ui/index.html"),
                )
            }),
        )
        .route(
            "/admin/admin.css",
            get(|| async {
                asset(
                    "text/css; charset=utf-8",
                    include_str!("admin_ui/admin.css"),
                )
            }),
        )
        .route(
            "/admin/admin.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("admin_ui/admin.js"),
                )
            }),
        )
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", content_type),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    (headers, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn shell_and_assets_have_restrictive_browser_headers() {
        for (path, mime, content) in [
            ("/admin/", "text/html", "model-relay"),
            ("/admin/admin.css", "text/css", "--accent"),
            ("/admin/admin.js", "text/javascript", "/api/private/v1"),
        ] {
            let response = routes::<()>()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with(mime)
            );
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(
                response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("frame-ancestors 'none'")
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(std::str::from_utf8(&body).unwrap().contains(content));
        }
    }
}
