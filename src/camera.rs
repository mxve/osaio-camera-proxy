use anyhow::{Context, Result, bail};
use bytes::Bytes;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{RTCIceCandidateInit, RTCIceServer};
use rtc::rtp_transceiver::rtp_sender::RTCPFeedback;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, broadcast};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCPeerConnectionIceEvent,
};

use crate::auth::{Device, Session, VideoCall};
use crate::h265::Depacketizer;
use crate::sdp;
use crate::ws::Ws;

const SWITCH_INTERVAL: Duration = Duration::from_secs(25);
const MEDIA_TIMEOUT: Duration = Duration::from_secs(20);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Camera {
    video: broadcast::Sender<Bytes>,
    audio: broadcast::Sender<Bytes>,
}

impl Camera {
    pub fn start(device: Device, session: Arc<RwLock<Session>>, ws: Arc<Ws>) -> Self {
        let (video, _) = broadcast::channel(1024);
        let (audio, _) = broadcast::channel(1024);
        let camera = Self {
            video: video.clone(),
            audio: audio.clone(),
        };

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(2);
            loop {
                let started = std::time::Instant::now();
                match run_once(&device, &session, &ws, &video, &audio).await {
                    Ok(()) => tracing::warn!(camera = %device.name, "camera stream ended"),
                    Err(err) => tracing::warn!(camera = %device.name, %err, "camera stream error"),
                }
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(2);
                }
                tracing::info!(camera = %device.name, "reconnecting in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });

        camera
    }

    pub fn subscribe_video(&self) -> broadcast::Receiver<Bytes> {
        self.video.subscribe()
    }

    pub fn subscribe_audio(&self) -> broadcast::Receiver<Bytes> {
        self.audio.subscribe()
    }
}

async fn run_once(
    device: &Device,
    session: &Arc<RwLock<Session>>,
    ws: &Arc<Ws>,
    video_out: &broadcast::Sender<Bytes>,
    audio_out: &broadcast::Sender<Bytes>,
) -> Result<()> {
    let call = session.read().await.video_call(&device.uuid).await?;
    let call_id =
        (1_000_000_000_000_000u64 + rand::random::<u64>() % 999_999_999_999_999).to_string();
    let (pc, mut video_rx, mut audio_rx) = connect(device, ws, &call, &call_id).await?;

    // action 0 turns stream on and keeps it alive
    let switch = |action: i32| {
        let data = serde_json::json!({
            "Action": action,
            "Timestamp": 0,
            "Quality": 2,
            "SessionId": call.session_id.clone().unwrap_or_default(),
            "call_id": call_id.clone(),
        });
        ws.send("service.Switch", &device.uuid, &device.model, data)
    };
    switch(0).await?;

    let mut ffmpeg = Ffmpeg::spawn().await.context("spawning ffmpeg muxer")?;
    let mut depacketizer = Depacketizer::default();
    let mut keepalive = tokio::time::interval(SWITCH_INTERVAL);
    keepalive.tick().await;

    let result = loop {
        tokio::select! {
            packet = tokio::time::timeout(MEDIA_TIMEOUT, video_rx.recv()) => {
                let packet = match packet {
                    Err(_) => break Err(anyhow::anyhow!("no media timeout")),
                    Ok(None) => break Err(anyhow::anyhow!("video channel closed")),
                    Ok(Some(p)) => p,
                };
                let mut write_failed = None;
                for nal in depacketizer.push(&packet) {
                    if let Err(err) = ffmpeg.write(&nal).await {
                        write_failed = Some(err);
                        break;
                    }
                }
                if let Some(err) = write_failed {
                    break Err(err).context("writing video to ffmpeg");
                }
            }
            muxed = ffmpeg.read() => {
                match muxed {
                    Ok(Some(bytes)) => {
                        let _ = video_out.send(bytes);
                    }
                    Ok(None) => break Err(anyhow::anyhow!("ffmpeg closed output")),
                    Err(err) => break Err(err).context("reading muxed video"),
                }
            }
            frame = audio_rx.recv() => {
                match frame {
                    Some(frame) => {
                        let _ = audio_out.send(frame);
                    }
                    None => break Err(anyhow::anyhow!("audio channel closed")),
                }
            }
            _ = keepalive.tick() => {
                if let Err(err) = switch(0).await {
                    break Err(err).context("keepalive switch failed");
                }
            }
        }
    };

    ffmpeg.shutdown().await;
    let _ = pc.close().await;
    result
}

struct Handler {
    ws: Arc<Ws>,
    uuid: String,
    model: String,
    video_tx: tokio::sync::mpsc::Sender<Bytes>,
    audio_tx: tokio::sync::mpsc::Sender<Bytes>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else {
            return;
        };
        let _ = self
            .ws
            .send(
                "service.IceCandidate",
                &self.uuid,
                &self.model,
                serde_json::json!({
                    "WebrtcCandidate": init.candidate,
                    "WebrtcSdpMLineIndex": 0,
                    "WebrtcSdpMid": "",
                }),
            )
            .await;
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let sink = match track.kind().await {
            RtpCodecKind::Video => self.video_tx.clone(),
            _ => self.audio_tx.clone(),
        };
        tokio::spawn(async move {
            while let Some(evt) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = evt
                    && sink.send(packet.payload).await.is_err()
                {
                    break;
                }
            }
        });
    }
}

async fn connect(
    device: &Device,
    ws: &Arc<Ws>,
    call: &VideoCall,
    call_id: &str,
) -> Result<(
    Arc<dyn PeerConnection>,
    tokio::sync::mpsc::Receiver<Bytes>,
    tokio::sync::mpsc::Receiver<Bytes>,
)> {
    let mut media = MediaEngine::default();
    media.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: "video/H265".into(),
                clock_rate: sdp::VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![RTCPFeedback {
                    typ: "nack".into(),
                    parameter: String::new(),
                }],
            },
            payload_type: sdp::VIDEO_PT,
        },
        RtpCodecKind::Video,
    )?;
    media.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: "audio/AAC".into(),
                clock_rate: sdp::AUDIO_CLOCK_RATE,
                channels: 1,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            payload_type: sdp::AUDIO_PT,
        },
        RtpCodecKind::Audio,
    )?;

    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(ice_servers(call))
        .build();

    let (video_tx, video_rx) = tokio::sync::mpsc::channel::<Bytes>(2048);
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<Bytes>(1024);

    let handler = Arc::new(Handler {
        ws: ws.clone(),
        uuid: device.uuid.clone(),
        model: device.model.clone(),
        video_tx,
        audio_tx,
    });

    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media)
            .with_handler(handler)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await?,
    );

    pc.add_transceiver_from_kind(
        RtpCodecKind::Audio,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Sendrecv,
            send_encodings: vec![Default::default()],
            ..Default::default()
        }),
    )
    .await?;

    pc.add_transceiver_from_kind(
        RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            ..Default::default()
        }),
    )
    .await?;

    let mut events = ws.subscribe();
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer.clone()).await?;
    let (compact, audio_mid, video_mid) = sdp::encode_offer(&offer.sdp)?;

    let (urls, usernames, passwords) = device_ice_arrays(call);
    let mut data = serde_json::json!({
        "WebrtcSdp": compact,
        "Action": 2,
        "Timestamp": 0,
        "TrickleICE": false,
        "CodecMode": 1,
        "EnableSpeaker": false,
        "EnableMic": false,
        "Quality": 1,
        "DtlsWaitTime": 5000,
        "call_id": call_id,
        "IceUrl": urls,
        "IceUsername": usernames,
        "IcePassword": passwords,
    });
    if let (Some(id), Some(obj)) = (&call.session_id, data.as_object_mut()) {
        obj.insert("SessionId".into(), serde_json::json!(id));
    }
    ws.send("service.SdpOffer", &device.uuid, &device.model, data)
        .await?;

    let mut pending: Vec<RTCIceCandidateInit> = Vec::new();
    let answer_sdp = tokio::time::timeout(ANSWER_TIMEOUT, async {
        loop {
            let event = events.recv().await.context("websocket closed")?;
            if event.uuid.as_deref() != Some(device.uuid.as_str()) {
                continue;
            }
            match event.method.as_str() {
                "event.IceCandidate" => {
                    if let Some(c) = event.data.get("WebrtcCandidate").and_then(|v| v.as_str()) {
                        pending.push(RTCIceCandidateInit {
                            candidate: c.to_string(),
                            ..Default::default()
                        });
                    }
                }
                "response.SdpAnswer" => {
                    let ret = event.data.get("Ret").and_then(|v| v.as_i64()).unwrap_or(0);
                    if ret != 0 {
                        bail!("camera rejected offer with Ret={ret}");
                    }
                    let compact = event
                        .data
                        .get("WebrtcSdp")
                        .and_then(|v| v.as_str())
                        .context("sdp answer missing WebrtcSdp")?;
                    return sdp::decode_answer(compact, audio_mid, video_mid);
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for sdp answer")??;

    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;
    for candidate in pending {
        let _ = pc.add_ice_candidate(candidate).await;
    }

    let pc_ref = pc.clone();
    let uuid = device.uuid.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if event.method != "event.IceCandidate" || event.uuid.as_deref() != Some(&uuid) {
                continue;
            }
            if let Some(c) = event.data.get("WebrtcCandidate").and_then(|v| v.as_str()) {
                let _ = pc_ref
                    .add_ice_candidate(RTCIceCandidateInit {
                        candidate: c.to_string(),
                        ..Default::default()
                    })
                    .await;
            }
        }
    });

    Ok((pc, video_rx, audio_rx))
}

fn ice_servers(call: &VideoCall) -> Vec<RTCIceServer> {
    call.user_ices
        .iter()
        .filter(|i| !i.iceurl.is_empty())
        .map(|i| {
            let url = match (&i.iceurl_ip, i.iceurl.split_once(':')) {
                (Some(ip), Some((scheme, rest))) => {
                    let port = rest.rsplit(':').next().unwrap_or("3478");
                    format!("{scheme}:{ip}:{port}")
                }
                _ => i.iceurl.clone(),
            };
            RTCIceServer {
                urls: vec![url],
                username: i.username.clone().unwrap_or_default(),
                credential: i.password.clone().unwrap_or_default(),
            }
        })
        .collect()
}

fn device_ice_arrays(call: &VideoCall) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut urls = Vec::new();
    let mut usernames = Vec::new();
    let mut passwords = Vec::new();
    for ice in &call.device_ices {
        if ice.iceurl.is_empty() {
            continue;
        }
        urls.push(if ice.iceurl.starts_with("turn:") {
            format!("{}?transport=udp", ice.iceurl)
        } else {
            ice.iceurl.clone()
        });
        usernames.push(ice.username.clone().unwrap_or_default());
        passwords.push(ice.password.clone().unwrap_or_default());
    }
    (urls, usernames, passwords)
}

// muxes annex-b hevc into mpegts stream
struct Ffmpeg {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Ffmpeg {
    async fn spawn() -> Result<Self> {
        let args = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-f",
            "hevc",
            "-i",
            "pipe:0",
            "-c:v",
            "copy",
            "-f",
            "mpegts",
            "-mpegts_flags",
            "resend_headers",
            "-flush_packets",
            "1",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "pipe:1",
        ];

        let program = std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn ffmpeg")?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(tokio::io::BufReader::new);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let stdin = self.stdin.as_mut().context("ffmpeg stdin unavailable")?;
        stdin.write_all(data).await.context("ffmpeg stdin write")
    }

    async fn read(&mut self) -> Result<Option<Bytes>> {
        use tokio::io::AsyncReadExt;
        let stdout = self.stdout.as_mut().context("ffmpeg stdout unavailable")?;
        let mut buf = vec![0u8; 32 * 1024];
        let n = stdout.read(&mut buf).await.context("ffmpeg stdout read")?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some(Bytes::from(buf)))
    }

    async fn shutdown(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill().await;
    }
}

pub struct Cameras {
    devices: Vec<Device>,
    running: Mutex<HashMap<String, Camera>>,
    session: Arc<RwLock<Session>>,
    ws: Arc<Ws>,
}

impl Cameras {
    pub fn new(devices: Vec<Device>, session: Arc<RwLock<Session>>, ws: Arc<Ws>) -> Self {
        Self {
            devices,
            running: Mutex::new(HashMap::new()),
            session,
            ws,
        }
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    // start streaming all cameras
    pub async fn start_all(&self) {
        for device in &self.devices {
            let _ = self.camera(&device.uuid).await;
        }
    }

    pub async fn camera(&self, uuid: &str) -> Result<Camera> {
        let mut running = self.running.lock().await;
        if let Some(cam) = running.get(uuid) {
            return Ok(cam.clone());
        }

        let device = self
            .devices
            .iter()
            .find(|d| d.uuid == uuid)
            .context("camera not found")?
            .clone();

        tracing::info!(camera = %device.name, id = %device.uuid, "starting stream");
        let camera = Camera::start(device, self.session.clone(), self.ws.clone());
        running.insert(uuid.to_string(), camera.clone());
        Ok(camera)
    }

    fn device(&self, uuid: &str) -> Result<&Device> {
        self.devices
            .iter()
            .find(|d| d.uuid == uuid)
            .context("camera not found")
    }

    pub async fn get_attribute(&self, uuid: &str, name: &str) -> Result<serde_json::Value> {
        let device = self.device(uuid)?.clone();
        self.session
            .read()
            .await
            .get_attribute(&device, name, &self.ws)
            .await
    }

    pub async fn set_attribute(
        &self,
        uuid: &str,
        name: &str,
        value: i64,
    ) -> Result<serde_json::Value> {
        let device = self.device(uuid)?.clone();
        self.session
            .read()
            .await
            .set_attribute(&device, name, value, &self.ws)
            .await
    }
}
