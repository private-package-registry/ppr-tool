//! Credentials: a static PPR_TOKEN, or a GitHub Actions OIDC token exchanged for a publication session.

use crate::actions;
use crate::error::{Error, Kind, Result};
use crate::http::{Body, Client};
use crate::inputs::Scope;
use crate::output::Ui;
use crate::time;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub expires_at: Option<String>,
}

pub const REFRESH_MARGIN: i64 = 30;

const OIDC_TOKEN_HINT: &str = "the registry rejected the GitHub OIDC token; check that --registry is the registry's public origin (the token audience)";
const OIDC_TRUST_HINT: &str = "no publisher trust on the registry matches this workflow run; check the trust's repository name and owner ID \
(renamed or transferred repositories must be updated), top-level workflow file, release tag and variants";

/// Obtains a publication session for a release identity.
pub fn authenticate(client: &Client, scope: &Scope, version: &str, ui: &Ui) -> Result<Session> {
    let github = |url: &str, bearer: &str| client.fetch_json(url, bearer, Duration::from_secs(30));
    authenticate_with(client, scope, version, ui, github)
}

pub fn authenticate_with(
    client: &Client,
    scope: &Scope,
    version: &str,
    ui: &Ui,
    github: impl Fn(&str, &str) -> Result<crate::http::Response>,
) -> Result<Session> {
    if let Some(token) = std::env::var("PPR_TOKEN").ok().filter(|t| !t.is_empty()) {
        if token.contains(['\r', '\n']) {
            return Err(Error::auth("PPR_TOKEN contains line breaks"));
        }
        actions::add_mask(&token);
        ui.protect(&token);
        return Ok(Session { token, expires_at: None });
    }
    let endpoint = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok().filter(|v| !v.is_empty());
    let request_token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok().filter(|v| !v.is_empty());
    let (Some(endpoint), Some(request_token)) = (endpoint, request_token) else {
        return Err(Error::auth("no credentials available").hint("set PPR_TOKEN, or on GitHub Actions grant `permissions: id-token: write`"));
    };
    let mut url = url::Url::parse(&endpoint).map_err(|_| Error::auth("ACTIONS_ID_TOKEN_REQUEST_URL is not a valid URL"))?;
    if !trusted_oidc_endpoint(&url) {
        return Err(Error::auth("unexpected GitHub OIDC endpoint").hint("ACTIONS_ID_TOKEN_REQUEST_URL must be an https://*.actions.githubusercontent.com URL"));
    }
    let existing: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| k != "audience").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    {
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        for (k, v) in &existing {
            pairs.append_pair(k, v);
        }
        pairs.append_pair("audience", &client.registry);
    }
    ui.protect(&request_token);
    // Every failure on the OIDC path is a credential problem (exit 4), including transport errors.
    let response = github(url.as_str(), &request_token).map_err(|e| e.kind(Kind::Auth).hint("grant `permissions: id-token: write` to the job and retry"))?;
    if response.status != 200 {
        return Err(
            Error::auth(format!("GitHub OIDC token request failed with HTTP {}", response.status)).hint("grant `permissions: id-token: write` to the job")
        );
    }
    let Some(value) = response.json.get("value").and_then(Value::as_str).filter(|v| !v.is_empty()) else {
        return Err(Error::auth("GitHub returned no OIDC token"));
    };
    actions::add_mask(value);
    ui.protect(value);
    let body = serde_json::to_vec(&json!({ "token": value, "product": scope.product, "version": version, "variant": scope.variant, "commit": scope.commit }))?;
    let credentials = client.request("POST", "/api/v1/auth/oidc", Body::Json(&body), &[]).map_err(|e| {
        // The generic "credential is invalid" hint would mislead: 401 rejects the GitHub token itself, 403 means no publisher trust matched.
        let status = e.detail.as_ref().and_then(|d| d.get("httpStatus")).and_then(Value::as_u64);
        let hint = match (status, e.hint.clone()) {
            (Some(401), _) => OIDC_TOKEN_HINT.to_string(),
            (Some(403), _) | (_, None) => OIDC_TRUST_HINT.to_string(),
            (_, Some(hint)) => hint,
        };
        e.kind(Kind::Auth).hint(hint)
    })?;
    let session = parse_session(&credentials)?;
    actions::add_mask(&session.token);
    ui.protect(&session.token);
    Ok(session)
}

fn trusted_oidc_endpoint(url: &url::Url) -> bool {
    if url.scheme() == "https" && url.host_str().is_some_and(|h| h.ends_with(".actions.githubusercontent.com")) {
        return true;
    }
    // Debug builds only: lets the integration tests point at a local fake OIDC issuer.
    cfg!(debug_assertions) && std::env::var_os("PPR_TOOL_TEST_ALLOW_HTTP_OIDC").is_some() && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
}

fn parse_session(value: &Value) -> Result<Session> {
    let token = value.get("token").and_then(Value::as_str).filter(|t| !t.is_empty() && !t.contains(['\r', '\n']));
    let expires_at = value.get("expiresAt").and_then(Value::as_str);
    match (token, expires_at.and_then(time::parse_rfc3339)) {
        (Some(token), Some(at)) if at > time::now() => Ok(Session { token: token.to_string(), expires_at: expires_at.map(str::to_string) }),
        _ => Err(Error::auth("registry returned an invalid publication session")),
    }
}

/// Refreshes a session that is about to expire (static tokens never expire).
pub fn needs_refresh(expires_at: Option<&str>) -> bool {
    match expires_at.and_then(time::parse_rfc3339) {
        Some(at) => at < time::now() + REFRESH_MARGIN,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::ColorMode;

    #[test]
    fn rejects_foreign_oidc_endpoints() {
        let client = Client::new("https://registry.example.com").unwrap();
        let scope = Scope { product: "demo".into(), variant: "sources".into(), commit: "a".repeat(40) };
        let ui = Ui::new(ColorMode::Never, true);
        // SAFETY: tests in this module are the only readers of these variables and run single-threaded per process invocation of the test binary.
        unsafe {
            std::env::remove_var("PPR_TOKEN");
            std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_URL", "https://evil.example.com/token");
            std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "request-token");
        }
        let error = authenticate_with(&client, &scope, "1.0.0", &ui, |_, _| panic!("must not be called")).unwrap_err();
        assert!(error.message.contains("unexpected GitHub OIDC endpoint"));
        unsafe {
            std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
            std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        }
    }

    #[test]
    fn parses_sessions() {
        assert!(parse_session(&json!({"token": "t", "expiresAt": "2000-01-01T00:00:00Z"})).is_err());
        assert!(parse_session(&json!({"token": "", "expiresAt": "2999-01-01T00:00:00Z"})).is_err());
        assert!(parse_session(&json!({"token": "t", "expiresAt": "2999-01-01T00:00:00Z"})).is_ok());
        assert!(needs_refresh(Some("2000-01-01T00:00:00Z")));
        assert!(!needs_refresh(Some("2999-01-01T00:00:00Z")));
        assert!(!needs_refresh(None));
    }
}
