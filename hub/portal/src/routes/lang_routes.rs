//! `POST /lang` — sets the [`crate::i18n::LANG_COOKIE`] cookie and redirects
//! back to `next`. Language is a per-browser display preference, not
//! account state (see `i18n.rs`'s module doc), so there's no DB write here
//! and nothing to gate behind `CurrentUser` — an anonymous visitor on
//! `/login`/`/setup` can switch language too.
//!
//! A normal POST (not a GET) so it runs through `auth::origin_guard` like
//! every other mutation. `next` is a hidden form field carrying the current
//! path (filled in client-side by `layout/nav.html`'s switcher — see that
//! template's header comment for why this isn't plumbed through every
//! page's context instead).

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use crate::http::AppState;
use crate::i18n;

pub fn router() -> Router<AppState> {
    Router::new().route("/lang", post(set_lang))
}

#[derive(Deserialize)]
struct LangForm {
    lang: String,
    #[serde(default)]
    next: String,
}

/// Only ever redirects to a local path — `next` is user-controlled (a form
/// field), so an absolute or protocol-relative (`//host/...`) value is
/// rejected in favour of `/`, never followed off-site.
/// Only a same-origin path. HS-10 (Hub sweep 2026-09-26): `/\evil.example` and `/<TAB>/…`
/// passed the `//` check and browsers read them as another host, and a CR/LF made the redirect
/// header invalid (500). A backslash, a control character or a second leading slash now sends
/// the visitor home instead.
fn safe_next(next: &str) -> &str {
    let bad = next.chars().any(|c| c == '\\' || c.is_control());
    if next.starts_with('/') && !next.starts_with("//") && !bad {
        next
    } else {
        "/"
    }
}

async fn set_lang(State(state): State<AppState>, Form(form): Form<LangForm>) -> Response {
    let lang = i18n::lang_or_default(&form.lang);
    let jar = CookieJar::new().add(i18n::lang_cookie(lang, state.cookie_secure));
    (jar, Redirect::to(safe_next(&form.next))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use super::super::test_support::test_state;
    use super::*;

    async fn post_lang(body: &str) -> Response {
        let (state, _dir) = test_state().await;
        Router::new()
            .merge(router())
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/lang")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn location(response: &Response) -> &str {
        response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
    }

    fn set_cookie(response: &Response) -> &str {
        response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
    }

    #[tokio::test]
    async fn sets_cookie_and_redirects_to_local_next() {
        let response = post_lang("lang=cs&next=%2Ftrends").await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/trends");
        assert!(set_cookie(&response).contains("lang=cs"));
    }

    #[tokio::test]
    async fn missing_next_falls_back_to_root() {
        let response = post_lang("lang=en").await;

        assert_eq!(location(&response), "/");
    }

    #[tokio::test]
    async fn absolute_next_is_rejected() {
        let response = post_lang("lang=en&next=https%3A%2F%2Fevil.example%2Fphish").await;

        assert_eq!(location(&response), "/");
    }

    /// HS-10: a backslash or a control character is not a same-origin path.
    #[tokio::test]
    async fn backslash_and_control_characters_are_rejected() {
        for next in [
            "%2F%5Cevil.example",
            "%2F%09%2Fevil.example",
            "%2Fa%0D%0ASet-Cookie%3Ax",
        ] {
            let response = post_lang(&format!("lang=en&next={next}")).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{next}");
            assert_eq!(location(&response), "/", "{next}");
        }
    }

    #[tokio::test]
    async fn protocol_relative_next_is_rejected() {
        let response = post_lang("lang=en&next=%2F%2Fevil.example").await;

        assert_eq!(location(&response), "/");
    }

    #[tokio::test]
    async fn unknown_lang_value_falls_back_to_english() {
        let response = post_lang("lang=de&next=%2F").await;

        assert!(set_cookie(&response).contains("lang=en"));
    }
}
