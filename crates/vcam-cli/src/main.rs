use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use gstreamer as gst;
use serde_json::Value;
use vcam_core::ipc::{Command, Request, ServerMessage};
use vcam_core::{format_time_ms, parse_time_ms, Config, DoctorReport, PlaybackState, Status};

#[derive(Debug, Parser)]
#[command(name = "vcam", version, about = "Control the vcam virtual camera")]
struct Cli {
    /// Path to the daemon control socket
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start playing a video (uses the configured source when omitted)
    Start {
        #[arg(long)]
        source: Option<PathBuf>,
        /// Disable looping
        #[arg(long)]
        no_loop: bool,
    },
    /// Stop playback and show a black placeholder
    Stop,
    /// Pause playback, keeping the last frame
    Pause,
    /// Resume playback
    Resume,
    /// Seek to a position (SS, MM:SS, or HH:MM:SS)
    Seek { position: String },
    /// Print daemon status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Change the source video without restarting the daemon
    Source {
        path: PathBuf,
        #[arg(long)]
        no_loop: bool,
    },
    /// Turn looping on or off for the current source
    Loop {
        #[arg(value_name = "on|off")]
        state: String,
    },
    /// Change the virtual camera output format
    Output {
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long, value_name = "NV12|YUYV")]
        pixel_format: Option<String>,
        #[arg(long, value_name = "letterbox|fill|stretch")]
        fit: Option<String>,
        #[arg(long, value_name = "true|false")]
        mirror: Option<bool>,
        #[arg(long, value_name = "0|90|180|270")]
        rotate: Option<u32>,
        /// Apply even while applications are using the camera (may wedge it)
        #[arg(long)]
        force: bool,
    },
    /// List applications currently using the virtual camera
    Consumers,
    /// Manage per-application output profiles
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
    },
    /// Run diagnostics on the module, device, and GStreamer installation
    Doctor {
        /// Device to check (defaults to the configured device)
        #[arg(long)]
        device: Option<PathBuf>,
    },
    /// Reload the configuration file
    Reload,
    /// Ask the daemon to exit
    Shutdown,
}

#[derive(Debug, Subcommand)]
enum ProfileCommands {
    /// List configured profiles
    List,
    /// Apply a profile now
    Use {
        name: String,
        /// Apply even while applications are using the camera (may wedge it)
        #[arg(long)]
        force: bool,
    },
    /// Turn automatic profile selection on or off
    Auto {
        #[arg(value_name = "on|off")]
        state: String,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("vcam: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let socket = match cli.socket {
        Some(socket) => socket,
        None => {
            let config = Config::load_or_default(&Config::default_path())
                .map_err(|error| error.to_string())?;
            config.socket_path()
        }
    };

    match cli.command {
        Commands::Start { source, no_loop } => {
            let data = request(
                &socket,
                Command::Start {
                    // The daemon may run from another working directory (for
                    // example under systemd), so send an absolute path.
                    source: source.map(|path| absolute(&path).display().to_string()),
                    loop_video: if no_loop { Some(false) } else { None },
                },
            )?;
            print_status_value(&data)?;
        }
        Commands::Stop => {
            let data = request(&socket, Command::Stop)?;
            print_status_value(&data)?;
        }
        Commands::Pause => {
            let data = request(&socket, Command::Pause)?;
            print_status_value(&data)?;
        }
        Commands::Resume => {
            let data = request(&socket, Command::Resume)?;
            print_status_value(&data)?;
        }
        Commands::Seek { position } => {
            let position_ms = parse_time_ms(&position)?;
            let data = request(&socket, Command::Seek { position_ms })?;
            print_status_value(&data)?;
        }
        Commands::Status { json } => {
            let data = request(&socket, Command::Status)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).map_err(|error| error.to_string())?
                );
            } else {
                print_status_value(&data)?;
            }
        }
        Commands::Source { path, no_loop } => {
            let data = request(
                &socket,
                Command::SetSource {
                    path: absolute(&path).display().to_string(),
                    loop_video: if no_loop { Some(false) } else { None },
                },
            )?;
            print_status_value(&data)?;
        }
        Commands::Loop { state } => {
            let enabled = match state.trim().to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => true,
                "off" | "false" | "no" | "0" => false,
                other => return Err(format!("invalid loop state '{other}' (use on or off)")),
            };
            let data = request(&socket, Command::SetLoop { enabled })?;
            print_status_value(&data)?;
        }
        Commands::Output {
            width,
            height,
            fps,
            pixel_format,
            fit,
            mirror,
            rotate,
            force,
        } => {
            let data = request(
                &socket,
                Command::SetOutput {
                    width,
                    height,
                    fps,
                    pixel_format,
                    fit,
                    mirror,
                    rotate,
                    force: if force { Some(true) } else { None },
                },
            )?;
            print_status_value(&data)?;
        }
        Commands::Consumers => {
            let data = request(&socket, Command::Status)?;
            let status: Status = serde_json::from_value(data).map_err(|error| error.to_string())?;
            print_consumers(&status);
        }
        Commands::Profile { command } => match command {
            ProfileCommands::List => {
                let data = request(&socket, Command::Profiles)?;
                print_profiles(&data)?;
            }
            ProfileCommands::Use { name, force } => {
                let data = request(
                    &socket,
                    Command::SetProfile {
                        name,
                        force: if force { Some(true) } else { None },
                    },
                )?;
                print_status_value(&data)?;
            }
            ProfileCommands::Auto { state } => {
                let enabled = match state.trim().to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" | "1" => true,
                    "off" | "false" | "no" | "0" => false,
                    other => return Err(format!("invalid auto state '{other}' (use on or off)")),
                };
                let data = request(&socket, Command::SetProfileAuto { enabled })?;
                print_status_value(&data)?;
            }
        },
        Commands::Doctor { device } => {
            // Run locally rather than through the daemon: the daemon cannot
            // start when the device is missing, which is exactly the case the
            // doctor is meant to diagnose.
            gst::init().map_err(|error| format!("failed to initialize GStreamer: {error}"))?;
            let config = Config::load_or_default(&Config::default_path())
                .map_err(|error| error.to_string())?;
            let device = device.unwrap_or_else(|| config.device_path());
            let report = vcam_doctor::run(&config, &device);
            print_doctor(&report);
            if !report.ok {
                std::process::exit(1);
            }
        }
        Commands::Reload => {
            let data = request(&socket, Command::Reload)?;
            print_status_value(&data)?;
        }
        Commands::Shutdown => {
            request(&socket, Command::Shutdown)?;
            println!("vcamd stopped");
        }
    }

    Ok(())
}

/// Resolve a user supplied path against the client's working directory. The
/// daemon may run from a different directory, so relative paths must be made
/// absolute before they cross the socket.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

fn connect(socket: &Path) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(socket).map_err(|error| {
        format!(
            "cannot connect to {}: {error}. Is vcamd running?",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| format!("failed to set socket timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("failed to set socket timeout: {error}"))?;
    Ok(stream)
}

fn request(socket: &Path, command: Command) -> Result<Value, String> {
    let mut stream = connect(socket)?;
    let request = Request::new(1, command);
    let mut text =
        serde_json::to_string(&request).map_err(|error| format!("invalid request: {error}"))?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .map_err(|error| format!("failed to send request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush request: {error}"))?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|error| format!("failed to read response: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Response(response)) if response.id == 1 => {
                if response.ok {
                    return Ok(response.data.unwrap_or(Value::Null));
                }
                return Err(response
                    .error
                    .unwrap_or_else(|| "daemon returned an unknown error".to_string()));
            }
            Ok(_) => continue,
            Err(error) => return Err(format!("invalid daemon response: {error}")),
        }
    }

    Err("daemon closed the connection without a response".to_string())
}

fn print_status_value(data: &Value) -> Result<(), String> {
    let status: Status = serde_json::from_value(data.clone()).map_err(|error| error.to_string())?;
    print_status(&status);
    Ok(())
}

fn print_status(status: &Status) {
    let state = match status.state {
        PlaybackState::Idle => "idle",
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Error => "error",
    };
    println!("State:    {state}");
    match &status.source {
        Some(source) => println!(
            "Source:   {source}{}",
            if status.loop_video { " (loop)" } else { "" }
        ),
        None => println!("Source:   -"),
    }
    if status.duration_ms > 0 {
        println!(
            "Position: {} / {}",
            format_time_ms(status.position_ms),
            format_time_ms(status.duration_ms)
        );
    } else {
        println!("Position: {}", format_time_ms(status.position_ms));
    }
    println!("Device:   {} ({})", status.device, status.label);
    println!(
        "Output:   {}x{} @ {} fps, {} , {}",
        status.output.width,
        status.output.height,
        status.output.fps,
        status.output.pixel_format,
        status.output.fit
    );
    if let Some(profile) = &status.profile {
        println!("Profile:  {profile}");
    }
    if !status.consumers.is_empty() {
        let names: Vec<String> = status
            .consumers
            .iter()
            .map(|consumer| format!("{} ({})", consumer.label(), consumer.mode.as_str()))
            .collect();
        println!("Readers:  {}", names.join(", "));
    }
    if let Some(pending) = &status.pending {
        println!("Pending:  {pending}");
    }
    if let Some(message) = &status.message {
        println!("Message:  {message}");
    }
}

fn print_consumers(status: &Status) {
    if status.consumers.is_empty() {
        println!("No applications are using {}.", status.device);
        return;
    }
    println!("{:<8} {:<24} MODE", "PID", "APPLICATION");
    for consumer in &status.consumers {
        println!(
            "{:<8} {:<24} {}",
            consumer.pid,
            consumer.label(),
            consumer.mode.as_str()
        );
    }
}

fn print_profiles(data: &Value) -> Result<(), String> {
    let Some(profiles) = data.get("profiles").and_then(Value::as_array) else {
        return Err("daemon returned an invalid profile list".to_string());
    };
    let active = data.get("active").and_then(Value::as_str);
    let auto = data.get("auto").and_then(Value::as_bool).unwrap_or(false);
    if profiles.is_empty() {
        println!("No profiles configured (add [[profiles]] sections to the config).");
        return Ok(());
    }
    println!("Auto:       {}", if auto { "on" } else { "off" });
    if let Some(active) = active {
        println!("Active:     {active}");
    }
    for profile in profiles {
        let name = profile
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let matches = profile
            .get("match")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let output = profile.get("output");
        let spec = output
            .map(|output| {
                format!(
                    "{}x{} @ {} fps {}",
                    output.get("width").and_then(Value::as_u64).unwrap_or(0),
                    output.get("height").and_then(Value::as_u64).unwrap_or(0),
                    output.get("fps").and_then(Value::as_u64).unwrap_or(0),
                    output
                        .get("pixel_format")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                )
            })
            .unwrap_or_default();
        let marker = if Some(name) == active { "*" } else { " " };
        println!("{marker} {name:<16} match: {matches:<24} {spec}");
    }
    Ok(())
}

fn print_doctor(report: &DoctorReport) {
    for check in &report.checks {
        let mark = if check.ok { "ok" } else { "FAIL" };
        println!("[{mark:>4}] {:<32} {}", check.name, check.detail);
    }
    println!();
    if report.ok {
        println!("All required checks passed.");
    } else {
        println!("Some checks failed; see details above.");
    }
}
