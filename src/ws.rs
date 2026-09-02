use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message;

use crate::auth::Session;
use crate::config::{AccountConfig, OsaioConfig};
use crate::util::now_secs;

const PING_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub msg_id: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub data: Value,
}

type Sink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub struct Ws {
    sink: Mutex<Option<Sink>>,
    closed: Notify,
    events: broadcast::Sender<Incoming>,
    counter: AtomicU64,
}

const ATTR_TIMEOUT: Duration = Duration::from_secs(10);

impl Ws {
    pub async fn connect(
        osaio: OsaioConfig,
        account: AccountConfig,
        session: Arc<RwLock<Session>>,
    ) -> Result<Arc<Self>> {
        let (events, _) = broadcast::channel(256);
        let ws = Arc::new(Self {
            sink: Mutex::new(None),
            closed: Notify::new(),
            events,
            counter: AtomicU64::new(0),
        });

        let stream = dial(&*session.read().await).await?;
        ws.clone().serve(stream).await;
        ws.clone().spawn_keepalive();

        let supervisor = ws.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                supervisor.closed.notified().await;
                tracing::warn!(
                    "signaling websocket closed, reconnecting in {}s",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;

                match reconnect_stream(&session, &osaio, &account).await {
                    Ok(stream) => {
                        tracing::info!("signaling websocket reconnected");
                        backoff = Duration::from_secs(1);
                        supervisor.clone().serve(stream).await;
                    }
                    Err(_) => {
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        });

        Ok(ws)
    }

    fn spawn_keepalive(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PING_INTERVAL).await;
                let failed = {
                    let mut guard = self.sink.lock().await;
                    match guard.as_mut() {
                        Some(sink) => sink.send(Message::Ping(bytes::Bytes::new())).await.is_err(),
                        None => false,
                    }
                };
                if failed {
                    self.disconnect().await;
                }
            }
        });
    }

    async fn disconnect(&self) {
        *self.sink.lock().await = None;
        self.closed.notify_one();
    }

    async fn serve(self: Arc<Self>, stream: WsStream) {
        let (sink, mut source) = stream.split();
        *self.sink.lock().await = Some(sink);
        tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                if let Ok(incoming) = serde_json::from_str::<Incoming>(&text) {
                    let _ = self.events.send(incoming);
                }
            }
            self.disconnect().await;
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Incoming> {
        self.events.subscribe()
    }

    pub async fn send(&self, method: &str, uuid: &str, model: &str, data: Value) -> Result<()> {
        self.send_returning_id(method, uuid, model, data).await?;
        Ok(())
    }

    pub async fn request(
        &self,
        method: &str,
        uuid: &str,
        model: &str,
        data: Value,
    ) -> Result<Value> {
        let mut events = self.subscribe();
        let msg_id = self.send_returning_id(method, uuid, model, data).await?;

        tokio::time::timeout(ATTR_TIMEOUT, async {
            loop {
                let event = events.recv().await.context("websocket closed")?;
                if event.msg_id.as_deref() == Some(&msg_id) {
                    return Ok(event.data);
                }
            }
        })
        .await
        .context("timed out waiting for reply")?
    }

    async fn send_returning_id(
        &self,
        method: &str,
        uuid: &str,
        model: &str,
        data: Value,
    ) -> Result<String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let msg_id = format!("rs-{}{:04}", now_secs(), n);
        let envelope = json!({
            "method": method,
            "msg_id": msg_id,
            "ver": "1.0",
            "origin": 1,
            "time": now_secs(),
            "uuid": uuid,
            "device_model": model,
            "data": data,
        });

        let mut guard = self.sink.lock().await;
        let Some(sink) = guard.as_mut() else {
            bail!("websocket is reconnecting");
        };

        if let Err(err) = sink.send(Message::Text(envelope.to_string().into())).await {
            drop(guard);
            self.disconnect().await;
            return Err(err).context("sending on websocket");
        }
        Ok(msg_id)
    }
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn reconnect_stream(
    session: &Arc<RwLock<Session>>,
    osaio: &OsaioConfig,
    account: &AccountConfig,
) -> Result<WsStream> {
    match dial(&*session.read().await).await {
        Ok(stream) => return Ok(stream),
        Err(err) => tracing::warn!(%err, "reconnect failed, refreshing session"),
    }
    let fresh = Session::login(osaio, account).await.map_err(|err| {
        tracing::warn!(%err, "session refresh failed");
        err
    })?;
    let dialed = dial(&fresh).await;
    *session.write().await = fresh;
    dialed.map_err(|err| {
        tracing::warn!(%err, "reconnect failed after fresh login");
        err
    })
}

async fn dial(session: &Session) -> Result<WsStream> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = session.ws_url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("api_token", session.api_token.parse()?);
    headers.insert("appid", session.osaio.appid.parse()?);
    headers.insert("phone_code", session.phone_code.parse()?);
    headers.insert("uid", session.uid.parse()?);

    let (stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connecting to websocket")?;
    Ok(stream)
}
