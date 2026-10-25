//! `/config`: redacted TOML, validate-only and validate + save.

use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    config::{Config, ConfigError},
    web::{WebError, WebState, auth::Ctx, render, view::Layout},
};

#[derive(Template)]
#[template(path = "config.html")]
struct ConfigTemplate {
    layout: Option<Layout>,
    form: ConfigForm,
}

/// The editor fragment; POST responses replace only this.
#[derive(Template)]
#[template(path = "partials/config_form.html")]
struct ConfigForm {
    text: String,
    message: Option<Message>,
}

struct Message {
    ok: bool,
    text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Submission {
    config: String,
    #[serde(default)]
    action: Action,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Validate,
    #[default]
    Save,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&ConfigTemplate {
        layout: Some(Layout::new(ctx, "config", &snapshot)),
        form: ConfigForm {
            text: web.app.config_text(),
            message: None,
        },
    })
}

pub(crate) async fn submit(
    State(web): State<WebState>,
    Form(submission): Form<Submission>,
) -> Result<Response, WebError> {
    // Validate before saving so a rejected edit never reaches the writer.
    let (status, message, text) = match Config::parse(&submission.config) {
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            error_message(&error),
            submission.config,
        ),
        Ok(_) if submission.action == Action::Validate => (
            StatusCode::OK,
            Message {
                ok: true,
                text: "Configuration is valid (not saved).".to_owned(),
            },
            submission.config,
        ),
        Ok(_) => match web.app.save_config(&submission.config).await {
            Ok(()) => {
                tracing::info!("configuration saved via web UI");
                (
                    StatusCode::OK,
                    Message {
                        ok: true,
                        text: "Saved and reloaded.".to_owned(),
                    },
                    web.app.config_text(),
                )
            }
            Err(error) => {
                tracing::warn!(%error, "configuration save failed");
                let status = match error {
                    ConfigError::Parse
                    | ConfigError::Validation(_)
                    | ConfigError::MissingSecret(_) => StatusCode::UNPROCESSABLE_ENTITY,
                    ConfigError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                    ConfigError::Io(_) | ConfigError::Serialize(_) => {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                };
                (status, error_message(&error), submission.config)
            }
        },
    };
    let form = ConfigForm {
        text,
        message: Some(message),
    };
    Ok((status, render(&form)?).into_response())
}

fn error_message(error: &ConfigError) -> Message {
    Message {
        ok: false,
        text: error.to_string(),
    }
}
