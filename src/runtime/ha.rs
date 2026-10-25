use super::Input;
use crate::{config::Config, engine::Event};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct Call {
    domain: String,
    service: String,
    entity: String,
    data: Value,
    reply: oneshot::Sender<Result<(), String>>,
}
#[derive(Clone)]
pub struct Ha {
    calls: mpsc::Sender<Call>,
}
impl Ha {
    #[cfg(test)]
    pub(crate) fn fake() -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (calls, mut rx) = mpsc::channel::<Call>(16);
        let (observed, received) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let _ = observed.send(json!({"domain":call.domain,"service":call.service,"entity":call.entity,"data":call.data}));
                let _ = call.reply.send(Ok(()));
            }
        });
        (Self { calls }, received)
    }
    pub(crate) fn start(
        config: &Config,
        tx: mpsc::Sender<Input>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (calls, mut rx) = mpsc::channel::<Call>(64);
        let config = config.clone();
        let task = tokio::spawn(async move {
            let mut backoff = 1;
            loop {
                let outcome = session(&config, &tx, &mut rx, &mut backoff).await;
                let _ = tx
                    .send(Input::Connection {
                        mqtt: false,
                        connected: false,
                    })
                    .await;
                if rx.is_closed() || tx.is_closed() {
                    break;
                }
                if let Err(error) = outcome {
                    tracing::warn!(error = %error, "HA websocket disconnected");
                }
                // Fail calls queued while disconnected; never replay stale power actions on reconnect.
                let delay = tokio::time::sleep(Duration::from_secs(backoff));
                tokio::pin!(delay);
                loop {
                    tokio::select! {
                        _ = &mut delay => break,
                        call = rx.recv() => match call { Some(call) => { let _ = call.reply.send(Err("HA is disconnected".into())); }, None => return },
                    }
                }
                backoff = (backoff * 2).min(60);
            }
        });
        (Self { calls }, task)
    }
    pub async fn call(
        &self,
        domain: &str,
        service: &str,
        entity: &str,
        data: Value,
    ) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.calls
            .try_send(Call {
                domain: domain.into(),
                service: service.into(),
                entity: entity.into(),
                data,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("HA adapter stopped or call queue full"))?;
        tokio::time::timeout(Duration::from_secs(15), result)
            .await??
            .map_err(anyhow::Error::msg)
    }
}
async fn session(
    config: &Config,
    tx: &mpsc::Sender<Input>,
    rx: &mut mpsc::Receiver<Call>,
    backoff: &mut u64,
) -> anyhow::Result<()> {
    let url = websocket_url(&config.home_assistant.url)?;
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(url)).await??;
    let auth = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .ok_or_else(|| anyhow::anyhow!("HA closed before authentication"))??;
    let auth: Value = serde_json::from_str(auth.to_text()?)?;
    anyhow::ensure!(
        auth["type"] == "auth_required",
        "HA did not request authentication"
    );
    ws.send(Message::Text(
        json!({"type":"auth","access_token":config.secrets.ha_token.expose()})
            .to_string()
            .into(),
    ))
    .await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .ok_or_else(|| anyhow::anyhow!("HA closed during authentication"))??;
    let reply: Value = serde_json::from_str(reply.to_text()?)?;
    anyhow::ensure!(reply["type"] == "auth_ok", "HA authentication rejected");
    *backoff = 1;
    tx.send(Input::Connection {
        mqtt: false,
        connected: true,
    })
    .await?;
    ws.send(Message::Text(
        json!({"id":1,"type":"subscribe_events","event_type":"state_changed"})
            .to_string()
            .into(),
    ))
    .await?;
    ws.send(Message::Text(
        json!({"id":2,"type":"get_states"}).to_string().into(),
    ))
    .await?;
    let mut entities: BTreeSet<String> = [
        &config.tv.entity,
        &config.receiver.entity,
        &config.sonos.entity,
    ]
    .into_iter()
    .cloned()
    .collect();
    entities.extend(config.device.iter().filter_map(|d| d.media_player.clone()));
    let mut pending = BTreeMap::<u64, oneshot::Sender<Result<(), String>>>::new();
    let mut id = 2;
    let mut last_message = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::interval_at(
        last_message + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(last_message + Duration::from_secs(60)) => anyhow::bail!("HA websocket silent for 60 seconds"),
            _ = heartbeat.tick() => {
                id += 1;
                ws.send(Message::Text(json!({"id":id,"type":"ping"}).to_string().into())).await?;
            }
            call = rx.recv() => {
                let Some(call) = call else { break; };
                if call.reply.is_closed() { continue; }
                pending.retain(|_, reply| !reply.is_closed());
                id += 1;
                let payload = json!({"id":id,"type":"call_service","domain":call.domain,"service":call.service,"target":{"entity_id":call.entity},"service_data":call.data});
                ws.send(Message::Text(payload.to_string().into())).await?;
                pending.insert(id, call.reply);
            }
            message = ws.next() => {
                let Some(message) = message else { break; };
                let message = message?;
                last_message = tokio::time::Instant::now();
                match message {
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
                    Message::Close(_) => break,
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text)?;
                        if value["type"] == "result" {
                            if value["id"] == 2 && let Some(states) = value["result"].as_array() {
                                for state in states { forward_state(state, &entities, tx).await?; }
                            }
                            if let Some(reply) = value["id"].as_u64().and_then(|id| pending.remove(&id)) {
                                let outcome = if value["success"] == true { Ok(()) } else { Err("HA service call failed".into()) };
                                let _ = reply.send(outcome);
                            }
                            if value["id"] == 1 { anyhow::ensure!(value["success"] == true, "HA state subscription rejected"); }
                        }
                        if value["type"] == "event" { forward_state(&value["event"]["data"]["new_state"], &entities, tx).await?; }
                    }
                    _ => {}
                }
            }
        }
    }
    for reply in pending.into_values() {
        let _ = reply.send(Err("HA disconnected before result".into()));
    }
    anyhow::bail!("HA websocket closed")
}
async fn forward_state(
    value: &Value,
    entities: &BTreeSet<String>,
    tx: &mpsc::Sender<Input>,
) -> anyhow::Result<()> {
    if let (Some(entity), Some(state)) = (value["entity_id"].as_str(), value["state"].as_str())
        && entities.contains(entity)
    {
        let attrs = value["attributes"]
            .as_object()
            .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        tx.send(Input::Event(Event::HaState {
            entity: entity.into(),
            state: state.into(),
            attrs,
        }))
        .await?;
    }
    Ok(())
}
fn websocket_url(base: &str) -> anyhow::Result<String> {
    let mut url = openidconnect::reqwest::Url::parse(base)?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        _ => anyhow::bail!("invalid HA URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|()| anyhow::anyhow!("invalid HA URL"))?;
    url.set_path("/api/websocket");
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn authenticates_subscribes_filters_states_and_calls_service() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.home_assistant.url = format!("http://{}", listener.local_addr().unwrap());
        let entity = config.tv.entity.clone();
        let (observed_tx, observed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            ws.send(Message::Text(
                json!({"type":"auth_required"}).to_string().into(),
            ))
            .await
            .unwrap();
            let auth: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(auth["type"], "auth");
            ws.send(Message::Text(json!({"type":"auth_ok"}).to_string().into()))
                .await
                .unwrap();
            for expected in ["subscribe_events", "get_states"] {
                let request: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["type"], expected);
                let result = if expected == "get_states" {
                    json!([{"entity_id":entity,"state":"on","attributes":{"source":"HDMI"}}])
                } else {
                    Value::Null
                };
                ws.send(Message::Text(
                    json!({"id":request["id"],"type":"result","success":true,"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
            ws.send(Message::Text(json!({"type":"event","event":{"data":{"new_state":{"entity_id":"sensor.unrelated","state":"ignored"}}}}).to_string().into())).await.unwrap();
            ws.send(Message::Text(json!({"type":"event","event":{"data":{"new_state":{"entity_id":entity,"state":"off","attributes":{}}}}}).to_string().into())).await.unwrap();
            let call: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(call["type"], "call_service");
            assert_eq!(call["service"], "turn_on");
            assert_eq!(call["target"]["entity_id"], entity);
            ws.send(Message::Text(
                json!({"id":call["id"],"type":"result","success":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = observed_tx.send(());
            let _ = ws.next().await;
        });
        let (tx, mut rx) = mpsc::channel(16);
        let (ha, task) = Ha::start(&config, tx);
        let test = async {
            let mut states = vec![];
            while states.len() < 2 {
                if let Input::Event(Event::HaState { state, .. }) = rx.recv().await.unwrap() {
                    states.push(state);
                }
            }
            assert_eq!(states, ["on", "off"]);
            ha.call("media_player", "turn_on", &config.tv.entity, json!({}))
                .await
                .unwrap();
            observed_rx.await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), test)
            .await
            .unwrap();
        task.abort();
        server.abort();
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;
    #[tokio::test]
    async fn pings_after_thirty_seconds_and_disconnects_after_sixty_seconds_silence() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.home_assistant.url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, started) = oneshot::channel();
        let (ping, observed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.send(Message::Text(
                json!({"type":"auth_required"}).to_string().into(),
            ))
            .await
            .unwrap();
            ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(json!({"type":"auth_ok"}).to_string().into()))
                .await
                .unwrap();
            ws.next().await.unwrap().unwrap();
            ws.next().await.unwrap().unwrap();
            let _ = ready.send(());
            let message = ws.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let _ = ping.send(value);
            std::future::pending::<()>().await;
        });
        let (tx, mut rx) = mpsc::channel(16);
        let (_ha, task) = Ha::start(&config, tx);
        started.await.unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            Input::Connection {
                connected: true,
                ..
            }
        ));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(observed.await.unwrap()["type"], "ping");
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(matches!(
            rx.recv().await.unwrap(),
            Input::Connection {
                connected: false,
                ..
            }
        ));
        task.abort();
        server.abort();
        tokio::time::resume();
    }
}
