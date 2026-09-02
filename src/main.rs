mod auth;
mod camera;
mod config;
mod h265;
mod sdp;
mod server;
mod settings;
mod util;
mod ws;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "osaio_proxy=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "config.toml".into());

    let cfg = config::Config::load(&config_path)?;
    tracing::info!(email = %cfg.account.email, "logging into osaio account");

    let session = Arc::new(RwLock::new(
        auth::Session::login(&cfg.osaio, &cfg.account).await?,
    ));
    tracing::info!(uid = %session.read().await.uid, "logged in successfully");

    let devices = session.read().await.devices().await?;
    tracing::info!("discovered {} camera(s)", devices.len());
    for d in &devices {
        tracing::info!("- {} ({}, id: {})", d.name, d.model, d.uuid);
    }

    let ws = ws::Ws::connect(cfg.osaio.clone(), cfg.account.clone(), session.clone()).await?;
    tracing::info!("signaling websocket connected");

    let cameras = Arc::new(camera::Cameras::new(devices, session, ws));
    cameras.start_all().await;

    let listener = tokio::net::TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding to {}", cfg.server.bind))?;

    tracing::info!("serving http endpoints on http://{}", cfg.server.bind);
    tracing::info!("  info:     http://{}/cameras/info", cfg.server.bind);
    tracing::info!(
        "  video:    http://{}/cameras/<id>/stream/video",
        cfg.server.bind
    );
    tracing::info!(
        "  audio:    http://{}/cameras/<id>/stream/audio",
        cfg.server.bind
    );
    tracing::info!(
        "  settings: http://{}/cameras/<id>/settings[/<name>[/<value>]]",
        cfg.server.bind
    );

    axum::serve(listener, server::router(cameras))
        .await
        .context("http server error")
}
