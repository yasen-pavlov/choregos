//! Read-only main-zone status. Function codes are traced, never mapped to HA names.
use crate::state::DirectPower;
use openidconnect::reqwest;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Status {
    pub state: DirectPower,
    pub source_code: Option<String>,
}

impl Status {
    fn unreachable() -> Self {
        Self {
            state: DirectPower::Unreachable,
            source_code: None,
        }
    }
}

pub(super) struct Client {
    http: reqwest::Client,
    url: String,
}

impl Client {
    pub fn new(host: &str, port: u16) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut url = reqwest::Url::parse("http://receiver.invalid/")?;
        url.set_host(Some(host))
            .map_err(|_| anyhow::anyhow!("invalid receiver host"))?;
        url.set_port(Some(port))
            .map_err(|_| anyhow::anyhow!("invalid receiver port"))?;
        url.set_path("/goform/formMainZone_MainZoneXmlStatusLite.xml");
        Ok(Self {
            http,
            url: url.into(),
        })
    }

    pub async fn poll(&self) -> Status {
        match self.read_status().await {
            Ok(status) => status,
            Err(_) => Status::unreachable(),
        }
    }

    async fn read_status(&self) -> anyhow::Result<Status> {
        let text = self
            .http
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_status(&text))
    }
}

fn value<'a>(xml: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let section = xml.split_once(open)?.1.split_once(close)?.0;
    Some(
        section
            .split_once("<value>")?
            .1
            .split_once("</value>")?
            .0
            .trim(),
    )
}

pub(super) fn parse_status(xml: &str) -> Status {
    let state = match value(xml, "<Power>", "</Power>") {
        Some("ON") => DirectPower::On,
        Some("OFF") => DirectPower::Off,
        _ => return Status::unreachable(),
    };
    Status {
        state,
        source_code: value(xml, "<InputFuncSelect>", "</InputFuncSelect>")
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_power_and_raw_function_code_without_mapping() {
        let xml = "<item><Power><value>OFF</value></Power><InputFuncSelect><value>MPLAY</value></InputFuncSelect><VolumeDisplay><value>Relative</value></VolumeDisplay><MasterVolume><value>-25.0</value></MasterVolume><Mute><value>off</value></Mute></item>";
        assert_eq!(
            parse_status(xml),
            Status {
                state: DirectPower::Off,
                source_code: Some("MPLAY".into())
            }
        );
        assert_eq!(
            parse_status(&xml.replace("OFF", "ON").replace("MPLAY", "SAT/CBL")),
            Status {
                state: DirectPower::On,
                source_code: Some("SAT/CBL".into())
            }
        );
        for suffix in ["", "<InputFuncSelect><value></value></InputFuncSelect>"] {
            assert_eq!(
                parse_status(&format!("<Power><value>ON</value></Power>{suffix}")),
                Status {
                    state: DirectPower::On,
                    source_code: None
                }
            );
        }
        for garbage in [
            "garbage",
            "<Power><value>unknown</value></Power><InputFuncSelect><value>MPLAY</value></InputFuncSelect>",
        ] {
            assert_eq!(parse_status(garbage), Status::unreachable());
        }
    }

    #[tokio::test]
    async fn stalled_http_request_times_out_as_unreachable() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new("127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
        let query = tokio::spawn(async move { client.poll().await });
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(query.await.unwrap(), Status::unreachable());
    }

    #[tokio::test]
    async fn polls_status_and_treats_http_failure_as_unreachable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new("127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            for (code, body) in [
                (
                    "200 OK",
                    "<item><Power><value>OFF</value></Power><InputFuncSelect><value>BD</value></InputFuncSelect></item>",
                ),
                ("503 Unavailable", ""),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 2048];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(
                    std::str::from_utf8(&buf[..n])
                        .unwrap()
                        .starts_with("GET /goform/formMainZone_MainZoneXmlStatusLite.xml HTTP/1.1")
                );
                socket.write_all(format!("HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        assert_eq!(
            client.poll().await,
            Status {
                state: DirectPower::Off,
                source_code: Some("BD".into())
            }
        );
        assert_eq!(client.poll().await, Status::unreachable());
        server.await.unwrap();
    }
}
