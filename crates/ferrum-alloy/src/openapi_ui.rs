//! The OpenAPI documentation UI (feature `openapi-ui`).
//!
//! Swagger UI is compiled into the binary from the files vendored under
//! `assets/swagger-ui/`, whose README records their source and hashes, so the
//! page never loads anything from another origin. The page is served at
//! `openapi.ui_path` and its assets beneath it, wherever the OpenAPI document
//! is served and under the same access policy: behind the management token on
//! the management listener, and on the application listener only with
//! `openapi.public`. The caller applies that policy.
//!
//! Every response carries a Content-Security-Policy that allows scripts,
//! styles, and requests from the page's own origin only, and no inline
//! script, so the initializer is a file of its own. The page loads the
//! document from the listener that served it. "Try it out" is disabled: the
//! UI documents the API and never calls it.

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use http::StatusCode;
use http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderName, HeaderValue, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

/// The page, with `{{assets}}` and `{{document}}` placeholders.
const PAGE: &str = include_str!("openapi_ui/index.html");

/// Files served beneath the page: name, content type, and contents. The
/// bundle's first line points to its license file, so that is served too.
const ASSETS: [(&str, &str, &[u8]); 4] = [
    (
        "swagger-ui.css",
        CSS,
        include_bytes!("../assets/swagger-ui/5.33.0/swagger-ui.css"),
    ),
    (
        "swagger-ui-bundle.js",
        JAVASCRIPT,
        include_bytes!("../assets/swagger-ui/5.33.0/swagger-ui-bundle.js"),
    ),
    (
        "swagger-ui-bundle.js.LICENSE.txt",
        TEXT,
        include_bytes!("../assets/swagger-ui/5.33.0/swagger-ui-bundle.js.LICENSE.txt"),
    ),
    (
        "swagger-initializer.js",
        JAVASCRIPT,
        include_bytes!("openapi_ui/swagger-initializer.js"),
    ),
];

/// Scripts, styles, and requests from the page's origin only; images may
/// also be `data:` URIs, which the stylesheet uses for icons. No inline
/// script or style element, no plugins, no forms, and no framing.
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// The documentation page and its assets for one configuration.
#[derive(Clone)]
pub(crate) struct DocsUi {
    path: String,
    page: Bytes,
}

impl DocsUi {
    /// The UI at `path` (a validated `openapi.ui_path`), showing the
    /// document at `document_path` on the same listener.
    pub(crate) fn new(path: &str, document_path: &str) -> Self {
        let page = PAGE
            .replace("{{assets}}", &escape_attribute(path))
            .replace("{{document}}", &escape_attribute(document_path));
        Self {
            path: path.to_owned(),
            page: Bytes::from(page),
        }
    }

    /// Routes for the page and its assets, without any access check.
    pub(crate) fn routes<S>(&self) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        let mut router = Router::new();
        let page = self.page.clone();
        router = router.route(&self.path, get(move || respond(HTML, page)));
        for (name, content_type, contents) in ASSETS {
            let body = Bytes::from_static(contents);
            let path = self.asset_path(name);
            router = router.route(&path, get(move || respond(content_type, body)));
        }
        router
    }

    /// The page's path and the path of every asset beneath it.
    pub(crate) fn paths(&self) -> impl Iterator<Item = String> + '_ {
        let assets = ASSETS
            .into_iter()
            .map(move |(name, _, _)| self.asset_path(name));
        std::iter::once(self.path.clone()).chain(assets)
    }

    fn asset_path(&self, name: &str) -> String {
        format!("{}/{name}", self.path)
    }
}

async fn respond(content_type: &'static str, body: Bytes) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
            ),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
            (X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
            (
                HeaderName::from_static("cross-origin-resource-policy"),
                HeaderValue::from_static("same-origin"),
            ),
        ],
        body,
    )
        .into_response()
}

/// Escapes `value` for a double-quoted HTML attribute.
fn escape_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(ui: &DocsUi) -> &str {
        std::str::from_utf8(&ui.page).unwrap_or_default()
    }

    #[test]
    fn the_page_references_only_its_own_assets_and_document() {
        let ui = DocsUi::new("/docs", "/openapi.json");
        let page = rendered(&ui);
        for reference in [
            r#"href="/docs/swagger-ui.css""#,
            r#"src="/docs/swagger-ui-bundle.js""#,
            r#"src="/docs/swagger-initializer.js""#,
            r#"data-document="/openapi.json""#,
        ] {
            assert!(page.contains(reference), "{reference} in {page}");
        }
        for absent in ["{{", "//", "http:", "https:", "style="] {
            assert!(!page.contains(absent), "{absent} in {page}");
        }
        assert_eq!(
            page.matches("<script").count(),
            page.matches("<script src=").count(),
            "no inline script"
        );
    }

    #[test]
    fn the_document_path_is_escaped() {
        let ui = DocsUi::new("/docs", r#"/a"><script>x</script>&'"#);
        let expected = r#"data-document="/a&quot;&gt;&lt;script&gt;x&lt;/script&gt;&amp;&#39;""#;
        assert!(rendered(&ui).contains(expected), "{}", rendered(&ui));
    }
}
