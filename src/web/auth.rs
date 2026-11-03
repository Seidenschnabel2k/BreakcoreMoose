//! Login with Fluxer: OAuth2 authorization code flow with PKCE, `identify`
//! scope only. Afterwards the page carries its own encrypted session
//! cookie; the Fluxer token is thrown away.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequestParts, OptionalFromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::PrivateCookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{AppState, random_token};

const SESSION_COOKIE: &str = "session";
const OAUTH_COOKIE: &str = "oauth";
const SESSION_SECS: u64 = 7 * 24 * 60 * 60;
const OAUTH_SECS: u64 = 10 * 60;

/// The logged-in person, from the session cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub user_id: String,
    pub name: String,
    /// Unix seconds.
    expires: u64,
}

/// In-flight login: kept in a short-lived cookie between redirect and callback.
#[derive(Serialize, Deserialize)]
struct Pending {
    state: String,
    verifier: String,
    next: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cookie(
    state: &AppState,
    name: &'static str,
    value: String,
    path: &'static str,
    max_age: u64,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .path(path)
        .http_only(true)
        .secure(state.web.public_url.starts_with("https://"))
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(max_age as i64))
        .build()
}

impl FromRequestParts<AppState> for Session {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar =
            PrivateCookieJar::<axum_extra::extract::cookie::Key>::from_request_parts(parts, state)
                .await
                .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let session: Session = jar
            .get(SESSION_COOKIE)
            .and_then(|c| serde_json::from_str(c.value()).ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if session.expires < now() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(session)
    }
}

impl OptionalFromRequestParts<AppState> for Session {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <Session as FromRequestParts<AppState>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// Only paths on this site, so the login can't be used as an open redirect.
fn safe_next(next: Option<String>) -> String {
    next.filter(|n| n.starts_with('/') && !n.starts_with("//") && !n.contains('\\'))
        .unwrap_or_else(|| "/".into())
}

pub async fn login(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<LoginQuery>,
) -> Response {
    let pending = Pending {
        state: random_token(),
        verifier: format!("{}{}", random_token(), random_token()),
        next: safe_next(q.next),
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));

    let mut url =
        url::Url::parse(&format!("{}/oauth2/authorize", state.config.api_url)).expect("api url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &state.web.client_id)
        .append_pair("redirect_uri", &state.web.redirect_uri())
        .append_pair("scope", "identify")
        .append_pair("state", &pending.state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    let value = serde_json::to_string(&pending).expect("serializable");
    let jar = jar.add(cookie(&state, OAUTH_COOKIE, value, "/auth", OAUTH_SECS));
    (jar, Redirect::to(url.as_str())).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct FluxerUser {
    id: String,
    username: String,
    global_name: Option<String>,
}

pub async fn callback(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let pending: Option<Pending> = jar
        .get(OAUTH_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok());
    let jar = jar.remove(Cookie::build(OAUTH_COOKIE).path("/auth"));
    if let Some(error) = q.error {
        return (
            jar,
            (
                StatusCode::FORBIDDEN,
                format!("Login was cancelled: {error}"),
            ),
        )
            .into_response();
    }
    let (Some(pending), Some(code)) = (pending, q.code) else {
        return (
            jar,
            (
                StatusCode::BAD_REQUEST,
                "Login expired. Go back and try again.",
            ),
        )
            .into_response();
    };
    if q.state.as_deref() != Some(pending.state.as_str()) {
        return (
            jar,
            (
                StatusCode::BAD_REQUEST,
                "Login state mismatch. Go back and try again.",
            ),
        )
            .into_response();
    }

    let user = match exchange(&state, &code, &pending.verifier).await {
        Ok(user) => user,
        Err(e) => {
            tracing::warn!("web login failed: {e:#}");
            return (
                jar,
                (StatusCode::BAD_GATEWAY, "Fluxer login failed. Try again."),
            )
                .into_response();
        }
    };
    tracing::info!(user = %user.username, "web login");
    let session = Session {
        user_id: user.id,
        name: user
            .global_name
            .filter(|n| !n.is_empty())
            .unwrap_or(user.username),
        expires: now() + SESSION_SECS,
    };
    let value = serde_json::to_string(&session).expect("serializable");
    let jar = jar.add(cookie(&state, SESSION_COOKIE, value, "/", SESSION_SECS));
    (jar, Redirect::to(&pending.next)).into_response()
}

/// Trades the code for a token and asks who it belongs to.
async fn exchange(state: &AppState, code: &str, verifier: &str) -> anyhow::Result<FluxerUser> {
    let api = &state.config.api_url;
    let token: TokenResponse = state
        .http
        .post(format!("{api}/oauth2/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &state.web.redirect_uri()),
            ("client_id", &state.web.client_id),
            ("client_secret", &state.web.client_secret),
            ("code_verifier", verifier),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let user = state
        .http
        .get(format!("{api}/users/@me"))
        .bearer_auth(&token.access_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(user)
}

pub async fn logout(jar: PrivateCookieJar) -> Response {
    (
        jar.remove(Cookie::build(SESSION_COOKIE).path("/")),
        Redirect::to("/"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::safe_next;

    #[test]
    fn next_stays_on_site() {
        assert_eq!(safe_next(Some("/g/123".into())), "/g/123");
        for bad in [
            "//evil.example",
            "https://evil.example",
            "/\\evil.example",
            "",
        ] {
            assert_eq!(safe_next(Some(bad.into())), "/", "{bad}");
        }
        assert_eq!(safe_next(None), "/");
    }
}
