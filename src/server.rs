use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use crate::camera::Cameras;
use crate::settings;

pub fn router(cameras: Arc<Cameras>) -> Router {
    Router::new()
        .route("/cameras/info", get(camera_info))
        .route("/cameras/{id}/stream/video", get(stream_video))
        .route("/cameras/{id}/stream/audio", get(stream_audio))
        .route("/cameras/{id}/settings", get(settings_all))
        .route("/cameras/{id}/settings/{setting}", get(setting_get))
        .route("/cameras/{id}/settings/{setting}/{value}", get(setting_set))
        .with_state(cameras)
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn camera_info(State(cameras): State<Arc<Cameras>>) -> Json<Value> {
    let list: Vec<_> = cameras
        .devices()
        .iter()
        .map(|d| {
            json!({
                "id": d.uuid,
                "name": d.name,
                "model": d.model,
                "streams": {
                    "video": format!("/cameras/{}/stream/video", d.uuid),
                    "audio": format!("/cameras/{}/stream/audio", d.uuid),
                },
                "settings": format!("/cameras/{}/settings", d.uuid),
            })
        })
        .collect();
    Json(Value::Array(list))
}

async fn stream_video(State(cameras): State<Arc<Cameras>>, Path(id): Path<String>) -> Response {
    match cameras.camera(&id).await {
        Ok(cam) => stream_body(cam.subscribe_video(), "video/mp2t"),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn stream_audio(State(cameras): State<Arc<Cameras>>, Path(id): Path<String>) -> Response {
    match cameras.camera(&id).await {
        Ok(cam) => stream_body(cam.subscribe_audio(), "audio/aac"),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn settings_all(State(cameras): State<Arc<Cameras>>, Path(id): Path<String>) -> Json<Value> {
    let mut list = Vec::new();
    for s in settings::SETTINGS {
        let entry = match cameras.get_attribute(&id, s.attribute).await {
            Ok(v) => json!({ "name": s.name, "value": s.label(v.as_i64().unwrap_or(-1)) }),
            Err(e) => json!({ "name": s.name, "error": e.to_string() }),
        };
        list.push(entry);
    }
    Json(Value::Array(list))
}

async fn setting_get(
    State(cameras): State<Arc<Cameras>>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    let Some(setting) = settings::find(&name) else {
        return err(StatusCode::NOT_FOUND, format!("unknown setting: {name}"));
    };
    match cameras.get_attribute(&id, setting.attribute).await {
        Ok(v) => Json(json!({ "name": name, "value": setting.label(v.as_i64().unwrap_or(-1)) }))
            .into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

async fn setting_set(
    State(cameras): State<Arc<Cameras>>,
    Path((id, name, value)): Path<(String, String, String)>,
) -> Response {
    let Some(setting) = settings::find(&name) else {
        return err(StatusCode::NOT_FOUND, format!("unknown setting: {name}"));
    };
    let Some(wanted) = setting.parse(&value) else {
        return err(
            StatusCode::BAD_REQUEST,
            format!("invalid value for {name}: {value}"),
        );
    };
    match cameras.set_attribute(&id, setting.attribute, wanted).await {
        Ok(reported) => {
            let now = reported.as_i64().unwrap_or(-1);
            let status = if now == wanted {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (
                status,
                Json(json!({ "name": name, "value": setting.label(now) })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

fn stream_body(
    rx: tokio::sync::broadcast::Receiver<bytes::Bytes>,
    content_type: &'static str,
) -> Response {
    let stream =
        BroadcastStream::new(rx).filter_map(|chunk| chunk.ok().map(Ok::<_, std::io::Error>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache, no-store")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "stream error"))
}
