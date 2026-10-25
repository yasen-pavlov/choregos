//! Login (OIDC authorization code + PKCE, local break-glass), session identity, and CSRF.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use askama::Template;
use axum::{
    Form,
    body::Body,
    extract::{FromRequestParts, Query, Request, State},
    http::{HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
    url::{self, Url, form_urlencoded},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tower_sessions::{Session, cookie::Key};

use super::{WebError, WebState, render, view::Layout};
use crate::{config::Secret, state::AuthConfig};

pub(crate) const USER_KEY: &str = "user";
const CSRF_KEY: &str = "csrf";
const PENDING_KEY: &str = "oidc_pending";
pub(crate) const CSRF_HEADER: &str = "x-csrf-token";
pub(crate) const CSRF_FIELD: &str = "_csrf";
const FORM_LIMIT: usize = 1 << 20;
const SESSION_KEY_SALT: &[u8] = b"choregos-session-cookie-key-v1";
const PROVIDER_TTL: Duration = Duration::from_secs(3600);
const IDP_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_FAILED: &str = "Invalid username or password.";
const LOGIN_BUSY: &str = "Too many login attempts; try again in a minute.";
const LOGIN_CONCURRENCY: usize = 2;
const LOGIN_ATTEMPTS_PER_WINDOW: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoginMethod {
    Oidc,
    Local,
}

impl LoginMethod {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Oidc => "sso",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionUser {
    pub(crate) name: String,
    pub(crate) method: LoginMethod,
}

/// Login settings resolved once at router construction.
pub(crate) struct Auth {
    local_username: String,
    /// Argon2id PHC string of the break-glass password; the plaintext is not retained here.
    local_hash: String,
    secure_cookies: bool,
    /// Bounds concurrent Argon2 verifications (each needs ~19 MiB and a blocking thread).
    login_gate: Arc<Semaphore>,
    attempts: std::sync::Mutex<AttemptWindow>,
    oidc: Oidc,
}

/// Fixed-window count of local login attempts, shared by all clients.
struct AttemptWindow {
    started: Instant,
    count: u32,
}

impl Auth {
    pub(crate) fn new(config: &AuthConfig) -> Self {
        let mut salt = [0_u8; 16];
        rand::fill(&mut salt);
        let local_hash = Argon2::default()
            .hash_password_with_salt(config.local_login.password.expose().as_bytes(), &salt)
            .expect("default Argon2 parameters with a 16-byte salt are valid")
            .to_string();
        let callback = callback_url(&config.public_url);
        if let Err(error) = &callback {
            tracing::error!(%error, "web.public_url is not a valid URL; single sign-on is disabled");
        }
        Self {
            local_username: config.local_login.username.clone(),
            local_hash,
            // Fail closed: an unusable public_url still gets Secure cookies.
            secure_cookies: callback
                .as_ref()
                .ok()
                .is_none_or(|url| url.scheme() == "https"),
            login_gate: Arc::new(Semaphore::new(LOGIN_CONCURRENCY)),
            attempts: std::sync::Mutex::new(AttemptWindow {
                started: Instant::now(),
                count: 0,
            }),
            oidc: Oidc {
                issuer: config.oidc.issuer.clone(),
                client_id: config.oidc.client_id.clone(),
                client_secret: config.oidc.client_secret.clone(),
                redirect_url: callback
                    .map(RedirectUrl::from_url)
                    .map_err(|error| error.to_string()),
                provider: Mutex::new(None),
            },
        }
    }

    pub(crate) fn secure_cookies(&self) -> bool {
        self.secure_cookies
    }

    /// Counts an attempt against the shared window; `false` once the window is exhausted.
    fn admit_attempt(&self) -> bool {
        let mut window = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if window.started.elapsed() >= LOGIN_WINDOW {
            *window = AttemptWindow {
                started: Instant::now(),
                count: 0,
            };
        }
        if window.count >= LOGIN_ATTEMPTS_PER_WINDOW {
            return false;
        }
        window.count += 1;
        true
    }

    /// Constant-work check: the hash is always verified, even for an unknown username.
    fn verify_local(&self, username: &str, password: &str) -> bool {
        let password_ok = Argon2::default()
            .verify_password(password.as_bytes(), self.local_hash.as_str())
            .is_ok();
        let username_ok = constant_time_eq(username.as_bytes(), self.local_username.as_bytes());
        password_ok && username_ok
    }
}

/// `<public_url>/auth/callback`, parsed so the scheme is normalised (`HTTPS://` → `https`).
fn callback_url(public_url: &str) -> Result<Url, url::ParseError> {
    let mut base = Url::parse(public_url)?;
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.join("auth/callback")
}

/// Derive the 64-byte cookie signing key from the (≥ 32-byte, random) session secret.
pub(crate) fn session_key(secret: &str) -> Key {
    // The secret is high-entropy, so minimal Argon2 cost suffices for key expansion.
    let params = Params::new(Params::MIN_M_COST, 1, 1, Some(64))
        .expect("minimal Argon2 parameters are valid");
    let mut key = [0_u8; 64];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(secret.as_bytes(), SESSION_KEY_SALT, &mut key)
        .expect("a 30-byte salt and 64-byte output are valid for Argon2");
    Key::from(&key)
}

struct Oidc {
    issuer: String,
    client_id: String,
    client_secret: Secret,
    /// `<public_url>/auth/callback`, or why `public_url` is unusable.
    redirect_url: Result<RedirectUrl, String>,
    provider: Mutex<Option<CachedProvider>>,
}

#[derive(Clone)]
struct CachedProvider {
    fetched: Instant,
    http: reqwest::Client,
    metadata: CoreProviderMetadata,
}

#[derive(Debug, thiserror::Error)]
enum OidcError {
    #[error("identity provider discovery failed: {0}")]
    Discovery(String),
    #[error("invalid redirect URL derived from public_url: {0}")]
    RedirectUrl(String),
    #[error("identity provider returned an error: {0}")]
    Denied(String),
    #[error("no login is pending in this session")]
    NoPendingLogin,
    #[error("state parameter does not match")]
    StateMismatch,
    #[error("callback is missing the authorization code")]
    MissingCode,
    #[error("token exchange failed: {0}")]
    Exchange(String),
    #[error("token response contains no ID token")]
    MissingIdToken,
    #[error("ID token verification failed: {0}")]
    Claims(String),
}

#[derive(Debug, Deserialize, Serialize)]
struct PendingOidc {
    state: String,
    nonce: String,
    verifier: String,
}

impl Oidc {
    /// Discovery is lazy (the router is built synchronously and the IdP may be down at
    /// startup) and refreshed hourly so rotated signing keys are picked up.
    async fn provider(&self) -> Result<CachedProvider, OidcError> {
        let mut cached = self.provider.lock().await;
        if let Some(provider) = cached.as_ref()
            && provider.fetched.elapsed() < PROVIDER_TTL
        {
            return Ok(provider.clone());
        }
        match self.discover().await {
            Ok(provider) => Ok(cached.insert(provider).clone()),
            Err(error) => match cached.as_ref() {
                Some(stale) => {
                    tracing::warn!(%error, "OIDC rediscovery failed; using cached metadata");
                    Ok(stale.clone())
                }
                None => Err(error),
            },
        }
    }

    async fn discover(&self) -> Result<CachedProvider, OidcError> {
        let discovery = |error: &dyn std::fmt::Display| OidcError::Discovery(error.to_string());
        let http = reqwest::ClientBuilder::new()
            // Following redirects would expose the client to SSRF.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(IDP_TIMEOUT)
            .build()
            .map_err(|error| discovery(&error))?;
        let issuer = IssuerUrl::new(self.issuer.clone()).map_err(|error| discovery(&error))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|error| discovery(&error))?;
        Ok(CachedProvider {
            fetched: Instant::now(),
            http,
            metadata,
        })
    }

    fn client(&self, provider: &CachedProvider) -> Result<OidcClient, OidcError> {
        let redirect = self.redirect_url.clone().map_err(OidcError::RedirectUrl)?;
        Ok(CoreClient::from_provider_metadata(
            provider.metadata.clone(),
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.expose().to_owned())),
        )
        .set_redirect_uri(redirect))
    }

    async fn begin(&self) -> Result<(String, PendingOidc), OidcError> {
        let provider = self.provider().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client(&provider)?
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".to_owned()))
            .add_scope(Scope::new("email".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        let pending = PendingOidc {
            state: state.secret().clone(),
            nonce: nonce.secret().clone(),
            verifier: verifier.secret().clone(),
        };
        Ok((url.to_string(), pending))
    }

    /// `pending` has already been matched against the callback's `state` and consumed.
    async fn complete(
        &self,
        pending: PendingOidc,
        callback: Callback,
    ) -> Result<String, OidcError> {
        if let Some(error) = callback.error {
            let detail = callback
                .error_description
                .map_or(error.clone(), |description| {
                    format!("{error}: {description}")
                });
            return Err(OidcError::Denied(detail));
        }
        let code = callback.code.ok_or(OidcError::MissingCode)?;
        let provider = self.provider().await?;
        let client = self.client(&provider)?;
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|error| OidcError::Exchange(error.to_string()))?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.verifier))
            .request_async(&provider.http)
            .await
            .map_err(|error| OidcError::Exchange(error.to_string()))?;
        let id_token = token.id_token().ok_or(OidcError::MissingIdToken)?;
        let verifier = client.id_token_verifier();
        let claims_error = |error: &dyn std::fmt::Display| OidcError::Claims(error.to_string());
        let claims = id_token
            .claims(&verifier, &Nonce::new(pending.nonce))
            .map_err(|error| claims_error(&error))?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                token.access_token(),
                id_token
                    .signing_alg()
                    .map_err(|error| claims_error(&error))?,
                id_token
                    .signing_key(&verifier)
                    .map_err(|error| claims_error(&error))?,
            )
            .map_err(|error| claims_error(&error))?;
            if actual != *expected {
                return Err(OidcError::Claims("access token hash mismatch".to_owned()));
            }
        }
        let name = claims
            .preferred_username()
            .map(|name| name.as_str().to_owned())
            .or_else(|| claims.email().map(|email| email.as_str().to_owned()))
            .unwrap_or_else(|| claims.subject().as_str().to_owned());
        Ok(name)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LocalLogin {
    username: String,
    password: String,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    layout: Option<Layout>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "login_local.html")]
struct LocalLoginTemplate {
    layout: Option<Layout>,
    csrf: String,
    username: String,
    error: Option<String>,
}

/// Authenticated request context for page handlers.
pub(crate) struct Ctx {
    pub(crate) user: SessionUser,
    pub(crate) csrf: String,
}

impl FromRequestParts<WebState> for Ctx {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &WebState) -> Result<Self, WebError> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| WebError::Unauthenticated)?;
        let user = session
            .get::<SessionUser>(USER_KEY)
            .await?
            .ok_or(WebError::Unauthenticated)?;
        let csrf = csrf_token(&session).await?;
        Ok(Self { user, csrf })
    }
}

/// The session's CSRF token, created on first use.
pub(crate) async fn csrf_token(session: &Session) -> Result<String, WebError> {
    if let Some(token) = session.get::<String>(CSRF_KEY).await? {
        return Ok(token);
    }
    let token = random_token();
    session.insert(CSRF_KEY, &token).await?;
    Ok(token)
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (left, right)| acc | (left ^ right))
            == 0
}

async fn is_logged_in(session: &Session) -> Result<bool, WebError> {
    Ok(session.get::<SessionUser>(USER_KEY).await?.is_some())
}

/// Start an authenticated session on a fresh id (prevents fixation) with a new CSRF token.
async fn establish(session: &Session, user: SessionUser) -> Result<(), WebError> {
    session.cycle_id().await?;
    session.insert(USER_KEY, user).await?;
    session.insert(CSRF_KEY, random_token()).await?;
    Ok(())
}

pub(crate) async fn require_login(
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, WebError> {
    if is_logged_in(&session).await? {
        return Ok(next.run(request).await);
    }
    if request.headers().contains_key("hx-request") {
        // htmx would otherwise swap the login page into a fragment target.
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response
            .headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/login"));
        return Ok(response);
    }
    Ok(Redirect::to("/login").into_response())
}

/// Every state-changing request must carry the session's token, as the `X-CSRF-Token`
/// header (htmx `hx-headers`) or a `_csrf` field of a urlencoded form.
pub(crate) async fn require_csrf(
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, WebError> {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return Ok(next.run(request).await);
    }
    let forbidden = || (StatusCode::FORBIDDEN, "missing or invalid CSRF token").into_response();
    let Some(expected) = session.get::<String>(CSRF_KEY).await? else {
        return Ok(forbidden());
    };
    if let Some(supplied) = request.headers().get(CSRF_HEADER) {
        return Ok(
            if constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
                next.run(request).await
            } else {
                forbidden()
            },
        );
    }
    let is_form = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return Ok(forbidden());
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, FORM_LIMIT)
        .await
        .map_err(|_| WebError::BadRequest("form body unreadable or too large".to_owned()))?;
    let valid = form_urlencoded::parse(&bytes)
        .find(|(name, _)| name == CSRF_FIELD)
        .is_some_and(|(_, value)| constant_time_eq(value.as_bytes(), expected.as_bytes()));
    if !valid {
        return Ok(forbidden());
    }
    Ok(next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await)
}

pub(crate) async fn login_page(session: Session) -> Result<Response, WebError> {
    if is_logged_in(&session).await? {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(render(&LoginTemplate {
        layout: None,
        error: None,
    })?
    .into_response())
}

pub(crate) async fn local_form(session: Session) -> Result<Response, WebError> {
    Ok(render(&LocalLoginTemplate {
        layout: None,
        csrf: csrf_token(&session).await?,
        username: String::new(),
        error: None,
    })?
    .into_response())
}

pub(crate) async fn local_submit(
    State(web): State<WebState>,
    session: Session,
    Form(login): Form<LocalLogin>,
) -> Result<Response, WebError> {
    // Reject rather than queue: both the attempt window and the verification gate are bounded.
    let permit = if web.auth.admit_attempt() {
        web.auth.login_gate.clone().try_acquire_owned().ok()
    } else {
        None
    };
    let Some(permit) = permit else {
        tracing::warn!(user = %login.username, method = "local", "login throttled");
        return local_form_error(
            &session,
            login.username,
            StatusCode::TOO_MANY_REQUESTS,
            LOGIN_BUSY,
        )
        .await
        .map(|mut response| {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            response
        });
    };
    let auth = web.auth.clone();
    let username = login.username.clone();
    // Argon2 is deliberately expensive; keep it off the async workers. The permit moves into
    // the closure so a cancelled request cannot release it while the hash is still running.
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth.verify_local(&username, &login.password)
    })
    .await
    .unwrap_or(false);
    if valid {
        tracing::info!(user = %login.username, method = "local", "login succeeded");
        establish(
            &session,
            SessionUser {
                name: login.username,
                method: LoginMethod::Local,
            },
        )
        .await?;
        return Ok(Redirect::to("/").into_response());
    }
    tracing::warn!(user = %login.username, method = "local", "login failed");
    local_form_error(
        &session,
        login.username,
        StatusCode::UNAUTHORIZED,
        LOGIN_FAILED,
    )
    .await
}

async fn local_form_error(
    session: &Session,
    username: String,
    status: StatusCode,
    message: &str,
) -> Result<Response, WebError> {
    let page = render(&LocalLoginTemplate {
        layout: None,
        csrf: csrf_token(session).await?,
        username,
        error: Some(message.to_owned()),
    })?;
    Ok((status, page).into_response())
}

fn sso_failed(error: &OidcError) -> Result<Response, WebError> {
    tracing::warn!(%error, method = "oidc", "login failed");
    let page = render(&LoginTemplate {
        layout: None,
        error: Some(format!("Single sign-on failed: {error}.")),
    })?;
    Ok((StatusCode::UNAUTHORIZED, page).into_response())
}

pub(crate) async fn oidc_start(
    State(web): State<WebState>,
    session: Session,
) -> Result<Response, WebError> {
    match web.auth.oidc.begin().await {
        Ok((url, pending)) => {
            session.insert(PENDING_KEY, pending).await?;
            Ok(Redirect::to(&url).into_response())
        }
        Err(error) => sso_failed(&error),
    }
}

pub(crate) async fn oidc_callback(
    State(web): State<WebState>,
    session: Session,
    Query(callback): Query<Callback>,
) -> Result<Response, WebError> {
    // Correlate before consuming: an unrelated or forged callback (error callbacks included)
    // must leave the user's genuine pending login intact.
    let Some(pending) = session.get::<PendingOidc>(PENDING_KEY).await? else {
        return sso_failed(&OidcError::NoPendingLogin);
    };
    let correlated = callback
        .state
        .as_deref()
        .is_some_and(|state| constant_time_eq(state.as_bytes(), pending.state.as_bytes()));
    if !correlated {
        return sso_failed(&OidcError::StateMismatch);
    }
    // Single use from here on: a replay of this callback finds no pending login.
    if session.remove::<PendingOidc>(PENDING_KEY).await?.is_none() {
        return sso_failed(&OidcError::NoPendingLogin);
    }
    match web.auth.oidc.complete(pending, callback).await {
        Ok(name) => {
            tracing::info!(user = %name, method = "oidc", "login succeeded");
            establish(
                &session,
                SessionUser {
                    name,
                    method: LoginMethod::Oidc,
                },
            )
            .await?;
            Ok(Redirect::to("/").into_response())
        }
        Err(error) => sso_failed(&error),
    }
}

pub(crate) async fn logout(session: Session) -> Result<Response, WebError> {
    session.flush().await?;
    Ok(Redirect::to("/login").into_response())
}
