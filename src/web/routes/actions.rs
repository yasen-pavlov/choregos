//! POST `/actions/*`: forward operator commands to the runtime; respond with a flash fragment.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    state::{Command, CommandError, Mode},
    web::{WebError, WebState, render},
};

#[derive(Template)]
#[template(path = "partials/flash.html")]
struct Flash {
    ok: bool,
    text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModeForm {
    mode: Mode,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendRawForm {
    to: String,
    data: String,
}

fn flash(status: StatusCode, ok: bool, text: String) -> Result<Response, WebError> {
    Ok((status, render(&Flash { ok, text })?).into_response())
}

fn rejected(text: impl Into<String>) -> Result<Response, WebError> {
    flash(StatusCode::UNPROCESSABLE_ENTITY, false, text.into())
}

async fn run(web: &WebState, command: Command, done: String) -> Result<Response, WebError> {
    let description = format!("{command:?}");
    match web.app.command(command).await {
        Ok(()) => {
            tracing::info!(command = %description, "operator command accepted");
            flash(StatusCode::OK, true, done)
        }
        Err(error) => {
            tracing::warn!(command = %description, %error, "operator command failed");
            let status = match error {
                CommandError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                CommandError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
                CommandError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            flash(status, false, error.to_string())
        }
    }
}

fn known_role(web: &WebState, role: &str) -> bool {
    web.app
        .snapshot()
        .roles
        .iter()
        .any(|view| view.role == role)
}

/// Reloads the page after a successful command whose effect is shown outside the flash.
fn refresh_on_success(mut response: Response) -> Response {
    if response.status().is_success() {
        response
            .headers_mut()
            .insert("hx-refresh", HeaderValue::from_static("true"));
    }
    response
}

pub(crate) async fn mode(
    State(web): State<WebState>,
    Form(form): Form<ModeForm>,
) -> Result<Response, WebError> {
    // The mode badge lives in the page chrome.
    run(
        &web,
        Command::SetMode(form.mode),
        format!("Mode set to {}.", form.mode),
    )
    .await
    .map(refresh_on_success)
}

pub(crate) async fn all_off(State(web): State<WebState>) -> Result<Response, WebError> {
    run(&web, Command::AllOff, "All off requested.".to_owned()).await
}

pub(crate) async fn identify(
    State(web): State<WebState>,
    Path(la): Path<String>,
) -> Result<Response, WebError> {
    match la.parse::<u8>() {
        Ok(la) if la <= 14 => {
            run(
                &web,
                Command::Identify(la),
                format!("Identify sent to LA {la}."),
            )
            .await
        }
        _ => rejected("Logical address must be 0–14."),
    }
}

pub(crate) async fn forget(
    State(web): State<WebState>,
    Path(la): Path<String>,
) -> Result<Response, WebError> {
    match la.parse::<u8>() {
        // The forgotten row and any role bound through it live in the page, not the flash.
        Ok(la) if la <= 14 => run(&web, Command::Forget(la), format!("Forgot LA {la}."))
            .await
            .map(refresh_on_success),
        _ => rejected("Logical address must be 0–14."),
    }
}

pub(crate) async fn takeover(
    State(web): State<WebState>,
    Path(role): Path<String>,
) -> Result<Response, WebError> {
    if !known_role(&web, &role) {
        return Err(WebError::NotFound);
    }
    let done = format!("Takeover by {role} requested.");
    run(&web, Command::Takeover(role), done).await
}

pub(crate) async fn standby(
    State(web): State<WebState>,
    Path(role): Path<String>,
) -> Result<Response, WebError> {
    if !known_role(&web, &role) {
        return Err(WebError::NotFound);
    }
    let done = format!("Standby for {role} requested.");
    run(&web, Command::Standby(role), done).await
}

pub(crate) async fn send_raw(
    State(web): State<WebState>,
    Form(form): Form<SendRawForm>,
) -> Result<Response, WebError> {
    let Some(to) = form.to.trim().parse::<u8>().ok().filter(|to| *to <= 15) else {
        return rejected("Destination must be a logical address 0–15.");
    };
    let max_bytes = web.app.bridge_max_raw_bytes();
    let data = match parse_hex(&form.data, max_bytes) {
        Ok(data) => data,
        Err(message) => return rejected(message),
    };
    let done = format!("Sent {} to LA {to}.", hex::encode_upper(&data));
    run(&web, Command::SendRaw { to, data }, done).await
}

/// Accepts `8F`, `44 41`, `44:41`; requires 1–`max_bytes` bytes (the bridge's raw-send limit).
fn parse_hex(text: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let digits: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    let data =
        hex::decode(&digits).map_err(|_| "Data must be hex bytes, e.g. 44 41.".to_owned())?;
    if data.is_empty() || data.len() > max_bytes {
        return Err(format!("Data must be 1–{max_bytes} bytes."));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::parse_hex;

    #[test]
    fn raw_data_accepts_separators_and_enforces_the_given_limit() {
        assert_eq!(parse_hex("44 41", 14).unwrap(), vec![0x44, 0x41]);
        assert_eq!(parse_hex("8f", 14).unwrap(), vec![0x8F]);
        assert_eq!(parse_hex("44:41", 14).unwrap(), vec![0x44, 0x41]);
        assert!(parse_hex("", 14).is_err());
        assert!(parse_hex("4", 14).is_err());
        assert!(parse_hex("zz", 14).is_err());
        assert_eq!(parse_hex(&"00".repeat(14), 14).unwrap().len(), 14);
        assert!(parse_hex(&"00".repeat(15), 14).is_err());
    }
}
