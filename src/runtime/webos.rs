//! One-shot LG webOS SSAP power-off, independent of HA's reconnect state.

use crate::config::Secret;
use futures_util::{SinkExt, StreamExt};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde_json::{Value, json};
use std::{net::Ipv6Addr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{Error as WebSocketError, Message},
};

#[derive(Debug, thiserror::Error)]
#[error("webOS pairing is required: the configured client key is unknown")]
pub(crate) struct PairingRequired;

fn websocket_url(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("wss://[{host}]:{port}")
    } else {
        format!("wss://{host}:{port}")
    }
}

// Never format the original error: URLs, remote replies and credentials may be embedded.
fn websocket_error(error: WebSocketError) -> anyhow::Error {
    let category = match error {
        WebSocketError::Io(error) => format!("I/O {:?}", error.kind()),
        WebSocketError::Tls(_) => "TLS handshake".into(),
        WebSocketError::Http(response) => format!("HTTP {}", response.status()),
        WebSocketError::Url(_) => "invalid URL".into(),
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
            "connection closed".into()
        }
        WebSocketError::Protocol(_) => "websocket protocol".into(),
        WebSocketError::Utf8(_) => "invalid UTF-8".into(),
        WebSocketError::Capacity(_) => "message capacity".into(),
        _ => "websocket transport".into(),
    };
    anyhow::anyhow!("webOS websocket failed: {category}")
}
pub async fn turn_off(host: &str, port: u16, key: &Secret) -> anyhow::Result<()> {
    within_timeout(async { session(connect(host, port).await?, key).await }).await
}

async fn within_timeout(operation: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .map_err(|_| anyhow::anyhow!("webOS power-off timed out after 5 seconds"))?
}

async fn connect(
    host: &str,
    port: u16,
) -> anyhow::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    // LG TVs expose only self-signed certificates, with no usable public trust chain.
    // Scope this exception to the TV connection; TLS handshake signatures are still verified.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TvCertificateVerifier(provider)))
        .with_no_client_auth();
    let (ws, _) = connect_async_tls_with_config(
        websocket_url(host, port),
        None,
        false,
        Some(Connector::Rustls(Arc::new(tls))),
    )
    .await
    .map_err(websocket_error)?;
    Ok(ws)
}

#[derive(Debug)]
struct TvCertificateVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for TvCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn registration(key: &Secret) -> Value {
    // REGISTRATION_PAYLOAD from aiowebostv/handshake.py (public LG manifest).
    json!({
        "type": "register",
        "id": "register_0",
        "payload": {
            "forcePairing": false,
            "manifest": {
                "appVersion": "1.1",
                "manifestVersion": 1,
                "permissions": [
                    "APP_TO_APP", "CLOSE", "CONTROL_AUDIO", "CONTROL_DISPLAY",
                    "CONTROL_INPUT_JOYSTICK", "CONTROL_INPUT_MEDIA_PLAYBACK",
                    "CONTROL_INPUT_MEDIA_RECORDING", "CONTROL_INPUT_TEXT", "CONTROL_INPUT_TV",
                    "CONTROL_MOUSE_AND_KEYBOARD", "CONTROL_POWER", "CONTROL_TV_SCREEN",
                    "LAUNCH", "LAUNCH_WEBAPP", "READ_APP_STATUS", "READ_COUNTRY_INFO",
                    "READ_CURRENT_CHANNEL", "READ_INPUT_DEVICE_LIST", "READ_INSTALLED_APPS",
                    "READ_LGE_SDX", "READ_LGE_TV_INPUT_EVENTS", "READ_NETWORK_STATE",
                    "READ_NOTIFICATIONS", "READ_POWER_STATE", "READ_RUNNING_APPS",
                    "READ_SETTINGS", "READ_TV_CHANNEL_LIST", "READ_TV_CURRENT_TIME",
                    "READ_UPDATE_INFO", "SEARCH", "TEST_OPEN", "TEST_PROTECTED", "TEST_SECURE",
                    "UPDATE_FROM_REMOTE_APP", "WRITE_NOTIFICATION_ALERT",
                    "WRITE_NOTIFICATION_TOAST", "WRITE_SETTINGS"
                ]
            },
            "pairingType": "PROMPT",
            "client-key": key.expose()
        }
    })
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    key: &Secret,
) -> anyhow::Result<()> {
    send(&mut ws, registration(key)).await?;
    let reply = receive(&mut ws).await?;
    if reply["type"] == "response" && reply["payload"]["pairingType"] == "PROMPT" {
        return Err(PairingRequired.into());
    }
    anyhow::ensure!(reply["type"] == "registered", "webOS registration failed");
    // turnOff can wake a TV in Active Standby. Check every attempt, including retries.
    send(
        &mut ws,
        json!({"type":"request", "id":"power_state_0", "uri":"ssap://com.webos.service.tvpower/power/getPowerState"}),
    )
    .await?;
    let power = receive(&mut ws).await?;
    anyhow::ensure!(
        power["id"] == "power_state_0" && (power["type"] == "response" || power["type"] == "error"),
        "webOS returned an unexpected power-state response"
    );
    if power["type"] == "response"
        && power["payload"]["returnValue"] != false
        && matches!(
            power["payload"]["state"].as_str(),
            Some("Power Off" | "Suspend" | "Active Standby")
        )
    {
        let _ = ws.close(None).await;
        return Ok(());
    }
    // Older webOS versions lack getPowerState; a service error still permits turnOff.
    send(
        &mut ws,
        json!({"type":"request", "id":"turn_off_0", "uri":"ssap://system/turnOff"}),
    )
    .await?;
    let reply = receive(&mut ws).await?;
    anyhow::ensure!(
        reply["type"] == "response"
            && reply["id"] == "turn_off_0"
            && reply["payload"]["returnValue"] == true,
        "webOS turnOff request was not acknowledged"
    );
    // The TV may reset TCP after acknowledging power-off; close must not cause a retry.
    let _ = ws.close(None).await;
    Ok(())
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    value: Value,
) -> anyhow::Result<()> {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .map_err(websocket_error)
}

async fn receive<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> anyhow::Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map_err(|_| anyhow::anyhow!("webOS returned invalid JSON"));
            }
            Some(Ok(Message::Ping(payload))) => ws
                .send(Message::Pong(payload))
                .await
                .map_err(websocket_error)?,
            Some(Ok(Message::Pong(_))) => {}
            Some(Err(error)) => return Err(websocket_error(error)),
            _ => anyhow::bail!("webOS websocket closed or returned an invalid message"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, connect_async};

    fn key() -> Secret {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config
            .resolve_secrets(|_| Some("test-only-dummy-credential-value-32".into()))
            .unwrap();
        config.secrets.ha_token
    }

    async fn fake(
        registration_reply: Value,
        power_reply: Option<Value>,
        off_reply: Option<Value>,
    ) -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            let register = receive(&mut ws).await.unwrap();
            assert_eq!(register, registration(&key()));
            assert_eq!(register["payload"]["client-key"], key().expose());
            assert!(
                register["payload"]["manifest"]["permissions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("CONTROL_POWER"))
            );
            send(&mut ws, registration_reply).await.unwrap();
            if let Some(reply) = power_reply {
                assert_eq!(
                    receive(&mut ws).await.unwrap(),
                    json!({"type":"request", "id":"power_state_0", "uri":"ssap://com.webos.service.tvpower/power/getPowerState"})
                );
                send(&mut ws, reply).await.unwrap();
                if off_reply.is_none() {
                    assert!(matches!(ws.next().await, Some(Ok(Message::Close(_)))));
                }
            }
            if let Some(reply) = off_reply {
                assert_eq!(
                    receive(&mut ws).await.unwrap(),
                    json!({"type":"request", "id":"turn_off_0", "uri":"ssap://system/turnOff"})
                );
                send(&mut ws, reply).await.unwrap();
                let _ = ws.next().await;
            }
        });
        let (ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
        let result = within_timeout(session(ws, &key())).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn registers_with_key_and_turns_off() {
        fake(
            json!({"type":"registered"}),
            Some(json!({"type":"response", "id":"power_state_0", "payload":{"state":"Active"}})),
            Some(json!({"type":"response", "id":"turn_off_0", "payload":{"returnValue":true}})),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unknown_key_requires_pairing_without_echoing_reply() {
        let failure = fake(json!({"type":"response", "payload":{"pairingType":"PROMPT", "client-key":"untrusted-echo"}}), None, None).await.unwrap_err();
        assert!(failure.is::<PairingRequired>());
        let error = failure.to_string();
        assert!(error.contains("pairing is required"));
        assert!(!error.contains("untrusted-echo"));
        assert!(!error.contains(key().expose()));
    }

    #[tokio::test]
    async fn rejects_failed_turn_off() {
        assert!(
            fake(
                json!({"type":"registered"}),
                Some(
                    json!({"type":"response", "id":"power_state_0", "payload":{"state":"Active"}})
                ),
                Some(
                    json!({"type":"response", "id":"turn_off_0", "payload":{"returnValue":false}})
                )
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not acknowledged")
        );
    }

    #[tokio::test]
    async fn already_off_states_never_send_turn_off() {
        for state in ["Active Standby", "Power Off", "Suspend"] {
            fake(
                json!({"type":"registered"}),
                Some(json!({"type":"response", "id":"power_state_0", "payload":{"state":state}})),
                None,
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn unsupported_power_state_still_sends_turn_off() {
        for reply in [
            json!({"type":"error", "id":"power_state_0", "error":"404 no such service"}),
            json!({"type":"response", "id":"power_state_0", "payload":{"returnValue":false}}),
        ] {
            fake(
                json!({"type":"registered"}),
                Some(reply),
                Some(json!({"type":"response", "id":"turn_off_0", "payload":{"returnValue":true}})),
            )
            .await
            .unwrap();
        }
    }

    #[test]
    fn ipv6_url_is_bracketed_without_changing_hostnames() {
        assert_eq!(
            websocket_url("2001:db8::1", 3001),
            "wss://[2001:db8::1]:3001"
        );
        assert_eq!(
            websocket_url("[2001:db8::1]", 3001),
            "wss://[2001:db8::1]:3001"
        );
        assert_eq!(
            websocket_url("tv.example.com", 3001),
            "wss://tv.example.com:3001"
        );
    }

    #[test]
    fn diagnostic_categories_do_not_echo_sensitive_material() {
        let error = websocket_error(WebSocketError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "sensitive-host-or-key",
        )))
        .to_string();
        assert!(error.contains("ConnectionRefused"));
        assert!(!error.contains("sensitive-host-or-key"));
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .body(Some(b"sensitive-host-or-key".to_vec()))
            .unwrap();
        let error = websocket_error(WebSocketError::Http(Box::new(response))).to_string();
        assert!(error.contains("403"));
        assert!(!error.contains("sensitive-host-or-key"));
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, registered) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            receive(&mut ws).await.unwrap();
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let (ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
        let client = tokio::spawn(async move { within_timeout(session(ws, &key())).await });
        registered.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(
            client
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        server.abort();
        tokio::time::resume();
    }
}
