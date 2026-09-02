use anyhow::{Context, Result, bail};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use md5::Digest as _;
use reqwest::header::HeaderValue;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::config::{AccountConfig, OsaioConfig};
use crate::util::{hex, now_secs};

pub fn md5_hex(input: &str) -> String {
    hex(&md5::Md5::digest(input.as_bytes()))
}

fn sign(secret: &str, appid: &str, timestamp: &str, uid: &str, token: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(format!("{appid}{timestamp}{uid}{token}").as_bytes());
    let digest = hex(&mac.finalize().into_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest.as_bytes())
}

pub fn phone_code(email: &str) -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "osaio-proxy".into());
    md5_hex(&format!("{host}:{email}"))
}

#[derive(Deserialize)]
struct Envelope<T> {
    code: i32,
    #[serde(default)]
    msg: Option<String>,
    data: Option<T>,
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    osaio: OsaioConfig,
    auth: Option<(String, String)>,
}

impl Client {
    pub fn new(base_url: impl Into<String>, osaio: OsaioConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            osaio,
            auth: None,
        }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let ts = now_secs().to_string();
        let (uid, token) = self.auth.clone().unwrap_or_default();
        let sign = sign(&self.osaio.app_secret, &self.osaio.appid, &ts, &uid, &token);
        let mut headers = reqwest::header::HeaderMap::new();

        // user-supplied strings
        if let Ok(v) = self.osaio.user_agent.parse() {
            headers.insert("User-Agent", v);
        }
        if let Ok(v) = self.osaio.appid.parse() {
            headers.insert("appid", v);
        }
        headers.insert(
            "ApiSignType",
            HeaderValue::from_static(if self.auth.is_some() { "2" } else { "1" }),
        );
        headers.insert("timeout", HeaderValue::from_static("10"));
        headers.insert("timestamp", ts.parse().unwrap());
        headers.insert("sign", sign.parse().unwrap());
        if self.auth.is_some() {
            headers.insert("uid", uid.parse().unwrap());
            headers.insert("api-token", token.parse().unwrap());
        }
        headers
    }

    async fn unwrap_body<T: DeserializeOwned>(resp: reqwest::Response, path: &str) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await.context("reading response body")?;
        if !status.is_success() {
            bail!("{path} returned http {status}: {text}");
        }
        let env: Envelope<T> =
            serde_json::from_str(&text).with_context(|| format!("decoding {path}: {text}"))?;
        if env.code != 1000 {
            bail!(
                "{path} failed: code={} msg={}",
                env.code,
                env.msg.unwrap_or_default()
            );
        }
        env.data.with_context(|| format!("{path} missing data"))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .headers(self.headers())
            .query(query)
            .send()
            .await
            .with_context(|| format!("get {path}"))?;
        Self::unwrap_body(resp, path).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .headers(self.headers())
            .json(body)
            .send()
            .await
            .with_context(|| format!("post {path}"))?;
        Self::unwrap_body(resp, path).await
    }
}

#[derive(Deserialize)]
struct BaseUrls {
    web: String,
    ws: String,
}

#[derive(Deserialize)]
struct LoginResult {
    api_token: String,
    uid: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Device {
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub model: String,
}

#[derive(Deserialize)]
struct DeviceListPage {
    data: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VideoCall {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_ices: Vec<Ice>,
    #[serde(default)]
    pub device_ices: Vec<Ice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ice {
    #[serde(default)]
    pub iceurl: String,
    #[serde(default)]
    pub iceurl_ip: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Clone)]
pub struct Session {
    pub ws_url: String,
    pub uid: String,
    pub api_token: String,
    pub phone_code: String,
    pub osaio: OsaioConfig,
    web_base_url: String,
}

impl Session {
    pub async fn login(osaio: &OsaioConfig, account: &AccountConfig) -> Result<Self> {
        let bootstrap = Client::new(&osaio.global_base_url, osaio.clone());
        let urls: BaseUrls = bootstrap
            .get(
                "/account/get-baseurl",
                &[("account", &account.email), ("country", "1")],
            )
            .await
            .context("resolving base url")?;

        let phone_code = phone_code(&account.email);
        let client = Client::new(&urls.web, osaio.clone());
        let result: LoginResult = client
            .post(
                "/login/login",
                &json!({
                    "account": account.email,
                    "country": "1",
                    "password": md5_hex(&account.password),
                    "phone_brand": "linux",
                    "phone_code": phone_code,
                    "timezone_name": "GMT",
                    "zone": 0.0,
                }),
            )
            .await
            .context("logging in")?;

        Ok(Self {
            ws_url: urls.ws,
            uid: result.uid,
            api_token: result.api_token,
            phone_code,
            osaio: osaio.clone(),
            web_base_url: urls.web,
        })
    }

    fn client(&self) -> Client {
        let mut client = Client::new(&self.web_base_url, self.osaio.clone());
        client.auth = Some((self.uid.clone(), self.api_token.clone()));
        client
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let page: DeviceListPage = self
            .client()
            .get("/device/list", &[("page", "1"), ("per_page", "100")])
            .await
            .context("listing devices")?;
        Ok(page.data)
    }

    pub async fn video_call(&self, uuid: &str) -> Result<VideoCall> {
        self.client()
            .post(
                "/webrtcsession/user/videocall",
                &json!({ "device_id": uuid }),
            )
            .await
            .context("requesting video call session")
    }

    pub async fn get_attribute(
        &self,
        device: &Device,
        name: &str,
        ws: &crate::ws::Ws,
    ) -> Result<serde_json::Value> {
        let reply = ws
            .request("atr.get", &device.uuid, &device.model, json!([name]))
            .await?;
        reply
            .get(name)
            .cloned()
            .with_context(|| format!("camera did not report {name}"))
    }

    pub async fn set_attribute(
        &self,
        device: &Device,
        name: &str,
        value: i64,
        ws: &crate::ws::Ws,
    ) -> Result<serde_json::Value> {
        ws.send(
            "atr.set",
            &device.uuid,
            &device.model,
            json!({ name: value }),
        )
        .await?;
        // applied async
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        self.get_attribute(device, name, ws).await
    }
}
