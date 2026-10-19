mod controller;
mod decoder;
mod frame;
mod output;
mod server;
mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use gstreamer as gst;
use tokio::sync::{oneshot, Notify};
use tracing_subscriber::EnvFilter;
use vcam_core::Config;

use controller::ControllerHandle;

#[derive(Debug, Parser)]
#[command(name = "vcamd", version, about = "Virtual camera daemon for Linux")]
struct Cli {
    /// Configuration file (default: ~/.config/vcam/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override the V4L2 device path, e.g. /dev/video10
    #[arg(long)]
    device: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long)]
    log_level: Option<String>,

    /// Write a default configuration file and exit
    #[arg(long)]
    write_default_config: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("vcamd: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(Config::default_path);

    if cli.write_default_config {
        let config = Config::default();
        config
            .save(&config_path)
            .map_err(|error| error.to_string())?;
        println!("wrote default configuration to {}", config_path.display());
        return Ok(());
    }

    let mut config = Config::load_or_default(&config_path).map_err(|error| error.to_string())?;
    if let Some(level) = cli.log_level {
        config.daemon.log_level = level;
    }
    config.validate()?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.daemon.log_level.clone()));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    gst::init().map_err(|error| format!("failed to initialize GStreamer: {error}"))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        "starting vcamd"
    );

    let handle = ControllerHandle::spawn(config, config_path, cli.device)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start async runtime: {error}"))?;

    runtime.block_on(run_async(handle))
}

async fn run_async(handle: ControllerHandle) -> Result<(), String> {
    let socket_path = handle.socket_path.clone();
    let (ready_tx, ready_rx) = oneshot::channel();
    let shutdown = Arc::new(Notify::new());

    let server_handle = tokio::spawn(server::serve(
        socket_path.clone(),
        handle.command_tx.clone(),
        handle.events.clone(),
        ready_tx,
        shutdown.clone(),
    ));

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err("control server failed to start".to_string()),
    }

    if let Err(error) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(%error, "sd_notify is unavailable");
    }

    tracing::info!(
        socket = %socket_path.display(),
        "vcamd is ready; the virtual camera is being held open"
    );

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| format!("failed to install SIGTERM handler: {error}"))?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::warn!(%error, "failed to listen for SIGINT");
            }
            tracing::info!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
        _ = handle.shutdown_notify.notified() => {
            tracing::info!("shutdown requested by a client");
        }
        _ = server_handle => {
            tracing::warn!("control server stopped unexpectedly");
        }
    }

    shutdown.notify_waiters();
    let _ = tokio::task::spawn_blocking(move || handle.shutdown()).await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
