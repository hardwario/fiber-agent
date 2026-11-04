//! `GET /docs` — the product documentation, carried over from v1's
//! `/docs` page (`viewer/frontend/app/docs/page.tsx`).
//!
//! The content keeps v1's shape: a list of sections, each a list of typed
//! blocks (`text`, `stepCard`, `fieldCard`, `image`, …), one file per
//! language (`docs/<lang>.json`, v1's `lib/docsContent.ts`). The block types
//! and their fields are unchanged, so an edit on either side still ports
//! across; `templates/pages/docs.html` is the renderer v1 wrote in TSX.
//!
//! Content is data rather than markup because the same list drives the
//! in-page table of contents and, in v1, the PDF build (`viewer/pdfbuild`).

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use minijinja::context;
use serde_json::Value;

use crate::auth::CurrentUser;
use crate::http::AppState;
use crate::i18n;
use crate::render;

use super::render as render_page;

pub fn router() -> Router<AppState> {
    Router::new().route("/docs", get(docs_page))
}

/// The section list for `lang`, English for anything else, with the brand's placeholders
/// (`{brand}`, `{mobile_app}`, `{hostname_prefix}` …, `proximos_brand::expand`) filled in. Debug builds read
/// the file from the source tree first so an edit shows on the next render
/// without a rebuild — `i18n::catalogue_json` does the same for translations.
///
/// The content reaches the template through `render::json_for_template`, not
/// raw: the image boxes are fractional coordinates the template multiplies
/// (`{{ box.x * 100 }}`), and under serde_json's `arbitrary_precision` — on
/// whenever this binary is built with control-core — a raw number arrives as
/// a map and that multiplication fails the whole page. See that function.
fn sections(lang: &str) -> Value {
    #[cfg(debug_assertions)]
    {
        let path = format!("{}/docs/{lang}.json", env!("CARGO_MANIFEST_DIR"));
        if let Ok(json) = std::fs::read_to_string(path) {
            if let Ok(value) = serde_json::from_str(&proximos_brand::expand(&json)) {
                return value;
            }
        }
    }
    let json = match lang {
        "cs" => include_str!("../../docs/cs.json"),
        _ => include_str!("../../docs/en.json"),
    };
    serde_json::from_str(&proximos_brand::expand(json)).unwrap_or(Value::Array(Vec::new()))
}

async fn docs_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let lang = i18n::current_lang();
    let ctx = render::page_ctx(
        context! {
            nav_active => "docs",
            sections => render::json_for_template(&sections(lang)),
            // v1 ships one PDF per language next to the page (its
            // `public/`), linked from the first section's heading row. The brand names
            // the files (brand.toml docs_pdf_*); a brand without them shows no link.
            pdf_href => if lang == "cs" {
                proximos_brand::DOCS_PDF_CS
            } else {
                proximos_brand::DOCS_PDF_EN
            }
            .map(|f| format!("/static/{f}")),
        },
        Some(&current_user),
    );
    render_page(&state, "pages/docs.html", ctx)
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::SESSION_COOKIE;
    use crate::routes::test_support::{login, test_state};

    /// One render of the whole content file through the whole block
    /// vocabulary — the only thing that catches a typo in a template branch
    /// a rarely-used block type reaches.
    #[tokio::test]
    async fn docs_page_renders_every_section() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/docs")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("id=\"overview\""));
        assert!(body.contains("id=\"quickstart\""));
        assert!(body.contains("id=\"monitoring\""));
        // The last section, and the only one built from `qaRows` blocks.
        assert!(body.contains("id=\"troubleshooting\""));
        // minijinja escapes the slashes in an attribute value.
        assert!(body.contains("device-overview.svg"));
        assert!(
            !body.contains("common.docs"),
            "an untranslated key reached the page"
        );
    }

    /// The Client section sends a user to controls that exist where it says
    /// (plan/portal/devices.hub.md §2, §3.4, §4): Retry on the Hubs row is
    /// for "Registration incomplete" only, so a lost or expired bundle is
    /// pointed at the "gone" screen's Retry and the Cluster card's New
    /// password — by their labels, as the UI spells them, in each language.
    #[test]
    fn the_client_section_names_the_controls_the_ui_shows() {
        for lang in ["en", "cs"] {
            let sections = super::sections(lang);
            let client = sections
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == "client")
                .expect("a client section");
            let text = client.to_string();
            for key in [
                "monitoring.hubs.retry_button",
                "monitoring.hubs.rotate_button",
                "monitoring.hubs.cluster_heading",
                "monitoring.hubs.tab_system",
                "monitoring.hubs.registration_incomplete",
            ] {
                let label = crate::i18n::lookup(lang, key);
                assert!(
                    text.contains(&label),
                    "{lang}: the Client section does not name {key} ({label})"
                );
            }
        }
    }
}
