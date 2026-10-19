//! GTK4 / libadwaita control panel for vcam.
//!
//! The GUI never owns the camera pipeline: it is a client of `vcamd` over the
//! Unix control socket. The daemon also publishes a small preview frame in the
//! runtime directory, which the GUI reads and renders. Opening a second
//! `v4l2src` on the loopback device is not reliable while another application
//! is streaming it (v4l2loopback rejects `VIDIOC_REQBUFS` with `EBUSY`), so the
//! preview comes from the daemon instead.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use vcam_core::ipc::{Command, Request, ServerMessage};
use vcam_core::preview::{self, PREVIEW_HEADER_LEN, PREVIEW_MAGIC};
use vcam_core::{format_time_ms, Config, DoctorReport, PlaybackState, Status};

#[derive(Default)]
struct Shared {
    connected: bool,
    status: Option<Status>,
    doctor: Option<DoctorReport>,
    error: Option<String>,
}

type SharedState = Arc<Mutex<Shared>>;

enum WorkerCommand {
    Send(Command),
}

struct PreviewFrame {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("io.github.ArchDityaa.Vcam")
        .build();
    app.connect_activate(|app| build_ui(app.upcast_ref()));
    app.run()
}

fn build_ui(app: &gtk::Application) {
    let config = Config::load_or_default(&Config::default_path()).unwrap_or_default();
    let socket = config.socket_path();
    let defaults = config.output.clone();

    let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
    let running = Arc::new(AtomicBool::new(true));
    let preview_frame: Arc<Mutex<Option<PreviewFrame>>> = Arc::new(Mutex::new(None));
    let updating = Rc::new(Cell::new(false));
    let user_seeking = Rc::new(Cell::new(false));

    let worker = spawn_worker(socket.clone(), shared.clone());
    spawn_status_reader(socket.clone(), shared.clone(), running.clone());
    spawn_preview(
        preview::preview_path(&socket),
        preview_frame.clone(),
        running.clone(),
    );

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Virtual Camera")
        .default_width(760)
        .default_height(860)
        .build();

    let header = adw::HeaderBar::new();
    let title = gtk::Label::new(Some("Virtual Camera"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    // Preview -------------------------------------------------------------
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_height_request(270);
    picture.set_hexpand(true);
    let preview_frame_widget = gtk::Frame::new(None);
    preview_frame_widget.set_child(Some(&picture));
    preview_frame_widget.add_css_class("card");

    // Playback ------------------------------------------------------------
    let playback_group = adw::PreferencesGroup::builder().title("Playback").build();

    let source_row = adw::ActionRow::builder()
        .title("Source")
        .subtitle("No video selected")
        .build();
    let open_button = gtk::Button::with_label("Open…");
    open_button.set_valign(gtk::Align::Center);
    source_row.add_suffix(&open_button);
    playback_group.add(&source_row);

    let controls_row = adw::ActionRow::builder().title("Controls").build();
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.set_valign(gtk::Align::Center);
    let play_button = gtk::Button::with_label("Play");
    let pause_button = gtk::Button::with_label("Pause");
    let stop_button = gtk::Button::with_label("Stop");
    play_button.add_css_class("suggested-action");
    controls.append(&play_button);
    controls.append(&pause_button);
    controls.append(&stop_button);
    controls_row.add_suffix(&controls);
    playback_group.add(&controls_row);

    let loop_row = adw::SwitchRow::builder()
        .title("Loop")
        .subtitle("Restart automatically at the end")
        .active(defaults_loop(&config))
        .build();
    playback_group.add(&loop_row);

    let seek_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1000.0, 1.0);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);
    let seek_row = adw::ActionRow::builder()
        .title("Position")
        .child(&seek_scale)
        .build();
    playback_group.add(&seek_row);

    let position_label = gtk::Label::new(Some("00:00 / 00:00"));
    position_label.add_css_class("dim-label");
    position_label.set_halign(gtk::Align::Start);
    let state_label = gtk::Label::new(Some("Connecting to vcamd…"));
    state_label.set_halign(gtk::Align::Start);
    state_label.set_wrap(true);

    // Output --------------------------------------------------------------
    let output_group = adw::PreferencesGroup::builder()
        .title("Output format")
        .description("Applied to the virtual camera device")
        .build();

    let width_row = adw::SpinRow::with_range(160.0, 3840.0, 2.0);
    width_row.set_title("Width");
    width_row.set_value(f64::from(defaults.width));
    output_group.add(&width_row);

    let height_row = adw::SpinRow::with_range(120.0, 2160.0, 2.0);
    height_row.set_title("Height");
    height_row.set_value(f64::from(defaults.height));
    output_group.add(&height_row);

    let fps_row = adw::SpinRow::with_range(1.0, 60.0, 1.0);
    fps_row.set_title("Frame rate");
    fps_row.set_value(f64::from(defaults.fps));
    output_group.add(&fps_row);

    let pixel_list = gtk::StringList::new(&["NV12", "YUYV"]);
    let pixel_row = adw::ComboRow::builder()
        .title("Pixel format")
        .model(&pixel_list)
        .selected(if defaults.pixel_format.fourcc() == "YUYV" {
            1
        } else {
            0
        })
        .build();
    output_group.add(&pixel_row);

    let fit_list = gtk::StringList::new(&["Letterbox", "Fill", "Stretch"]);
    let fit_index = match defaults.fit.as_str() {
        "fill" => 1,
        "stretch" => 2,
        _ => 0,
    };
    let fit_row = adw::ComboRow::builder()
        .title("Scaling")
        .model(&fit_list)
        .selected(fit_index)
        .build();
    output_group.add(&fit_row);

    let rotate_list = gtk::StringList::new(&["0°", "90°", "180°", "270°"]);
    let rotate_index = match defaults.rotate {
        90 => 1,
        180 => 2,
        270 => 3,
        _ => 0,
    };
    let rotate_row = adw::ComboRow::builder()
        .title("Rotation")
        .model(&rotate_list)
        .selected(rotate_index)
        .build();
    output_group.add(&rotate_row);

    let mirror_row = adw::SwitchRow::builder()
        .title("Mirror")
        .active(defaults.mirror)
        .build();
    output_group.add(&mirror_row);

    let apply_row = adw::ActionRow::builder()
        .title("Apply output format")
        .build();
    let apply_button = gtk::Button::with_label("Apply");
    apply_button.set_valign(gtk::Align::Center);
    apply_row.add_suffix(&apply_button);
    output_group.add(&apply_row);

    // Camera users --------------------------------------------------------
    let consumers_group = adw::PreferencesGroup::builder()
        .title("Camera users")
        .description("Applications currently holding the virtual camera open")
        .build();

    let consumers_row = adw::ActionRow::builder()
        .title("Readers")
        .subtitle("Nobody is using the camera")
        .build();
    consumers_group.add(&consumers_row);

    let profile_row = adw::ActionRow::builder()
        .title("Profile")
        .subtitle("No profile active")
        .build();
    consumers_group.add(&profile_row);

    let force_row = adw::SwitchRow::builder()
        .title("Force output changes")
        .subtitle("Apply even while an application is using the camera (may wedge it)")
        .build();
    consumers_group.add(&force_row);

    // Diagnostics ---------------------------------------------------------
    let doctor_group = adw::PreferencesGroup::builder()
        .title("Diagnostics")
        .build();
    let doctor_row = adw::ActionRow::builder()
        .title("vcam doctor")
        .subtitle("Check module, device, permissions and GStreamer elements")
        .build();
    let doctor_button = gtk::Button::with_label("Run");
    doctor_button.set_valign(gtk::Align::Center);
    doctor_row.add_suffix(&doctor_button);
    doctor_group.add(&doctor_row);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&preview_frame_widget);
    content.append(&state_label);
    content.append(&position_label);
    content.append(&playback_group);
    content.append(&output_group);
    content.append(&consumers_group);
    content.append(&doctor_group);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    window.set_content(Some(&toolbar));

    // Actions -------------------------------------------------------------
    {
        let selected = Rc::new(std::cell::RefCell::new(None::<String>));
        let selected_for_play = selected.clone();
        let shared_play = shared.clone();
        let window_for_open = window.clone();
        let selected_for_open = selected.clone();
        let open_worker = worker.clone();
        let play_worker = worker.clone();

        open_button.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose a video").build();
            let worker = open_worker.clone();
            let selected = selected_for_open.clone();
            dialog.open(
                Some(&window_for_open),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    if let Ok(file) = result {
                        if let Some(path) = file.path() {
                            let path = path.display().to_string();
                            *selected.borrow_mut() = Some(path.clone());
                            let _ = worker.send(WorkerCommand::Send(Command::SetSource {
                                path,
                                loop_video: None,
                            }));
                        }
                    }
                },
            );
        });

        play_button.connect_clicked(move |_| {
            let has_source = shared_play
                .lock()
                .ok()
                .and_then(|shared| shared.status.as_ref().map(|status| status.source.is_some()))
                .unwrap_or(false)
                || selected_for_play.borrow().is_some();
            if !has_source {
                return;
            }
            let _ = play_worker.send(WorkerCommand::Send(Command::Start {
                source: None,
                loop_video: None,
            }));
        });
    }
    {
        let worker = worker.clone();
        let shared = shared.clone();
        pause_button.connect_clicked(move |_| {
            let paused = shared
                .lock()
                .ok()
                .and_then(|shared| {
                    shared
                        .status
                        .as_ref()
                        .map(|status| status.state == PlaybackState::Paused)
                })
                .unwrap_or(false);
            let command = if paused {
                Command::Resume
            } else {
                Command::Pause
            };
            let _ = worker.send(WorkerCommand::Send(command));
        });
    }
    {
        let worker = worker.clone();
        stop_button.connect_clicked(move |_| {
            let _ = worker.send(WorkerCommand::Send(Command::Stop));
        });
    }
    {
        let worker = worker.clone();
        let updating = updating.clone();
        loop_row.connect_active_notify(move |row| {
            if updating.get() {
                return;
            }
            let _ = worker.send(WorkerCommand::Send(Command::SetLoop {
                enabled: row.is_active(),
            }));
        });
    }
    {
        let worker = worker.clone();
        let user_seeking = user_seeking.clone();
        seek_scale.connect_change_value(move |_scale, _scroll, value| {
            user_seeking.set(true);
            let _ = worker.send(WorkerCommand::Send(Command::Seek {
                position_ms: value.max(0.0) as u64,
            }));
            let flag = user_seeking.clone();
            glib::timeout_add_local_once(Duration::from_millis(600), move || flag.set(false));
            glib::Propagation::Proceed
        });
    }
    {
        let apply_worker = worker.clone();
        let width_row = width_row.clone();
        let height_row = height_row.clone();
        let fps_row = fps_row.clone();
        let pixel_row = pixel_row.clone();
        let fit_row = fit_row.clone();
        let rotate_row = rotate_row.clone();
        let mirror_row = mirror_row.clone();
        let force_row = force_row.clone();
        apply_button.connect_clicked(move |_| {
            let width = width_row.value() as u32;
            let height = height_row.value() as u32;
            let fps = fps_row.value() as u32;
            let pixel_format = match pixel_row.selected() {
                1 => "YUYV",
                _ => "NV12",
            };
            let fit = match fit_row.selected() {
                1 => "fill",
                2 => "stretch",
                _ => "letterbox",
            };
            let rotate = match rotate_row.selected() {
                1 => 90,
                2 => 180,
                3 => 270,
                _ => 0,
            };
            let _ = apply_worker.send(WorkerCommand::Send(Command::SetOutput {
                width: Some(width),
                height: Some(height),
                fps: Some(fps),
                pixel_format: Some(pixel_format.to_string()),
                fit: Some(fit.to_string()),
                mirror: Some(mirror_row.is_active()),
                rotate: Some(rotate),
                force: if force_row.is_active() {
                    Some(true)
                } else {
                    None
                },
            }));
        });
    }
    {
        let worker = worker.clone();
        doctor_button.connect_clicked(move |_| {
            let _ = worker.send(WorkerCommand::Send(Command::Doctor));
        });
    }

    // Periodic UI refresh -------------------------------------------------
    {
        let shared = shared.clone();
        let preview_frame = preview_frame.clone();
        let updating = updating.clone();
        let user_seeking = user_seeking.clone();
        let window = window.clone();
        let state_label = state_label.clone();
        let position_label = position_label.clone();
        let source_row = source_row.clone();
        let loop_row = loop_row.clone();
        let seek_scale = seek_scale.clone();
        let pause_button = pause_button.clone();
        let picture = picture.clone();
        let width_row = width_row.clone();
        let height_row = height_row.clone();
        let fps_row = fps_row.clone();
        let pixel_row = pixel_row.clone();
        let fit_row = fit_row.clone();
        let rotate_row = rotate_row.clone();
        let mirror_row = mirror_row.clone();
        let consumers_row = consumers_row.clone();
        let profile_row = profile_row.clone();

        glib::timeout_add_local(Duration::from_millis(150), move || {
            if let Some(frame) = preview_frame.lock().ok().and_then(|mut guard| guard.take()) {
                let bytes = glib::Bytes::from_owned(frame.data);
                let texture = gtk::gdk::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gtk::gdk::MemoryFormat::R8g8b8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));
            }

            let (status, connected, error, doctor) = {
                let mut shared = match shared.lock() {
                    Ok(shared) => shared,
                    Err(_) => return glib::ControlFlow::Continue,
                };
                (
                    shared.status.clone(),
                    shared.connected,
                    shared.error.take(),
                    shared.doctor.take(),
                )
            };

            if let Some(report) = doctor {
                show_doctor(&window, &report);
            }

            if !connected {
                state_label
                    .set_text("vcamd is not running. Start it with: systemctl --user start vcamd");
                return glib::ControlFlow::Continue;
            }

            if let Some(error) = error {
                state_label.set_text(&format!("Error: {error}"));
            } else if let Some(status) = status {
                updating.set(true);

                let state_text = match status.state {
                    PlaybackState::Idle => "Idle",
                    PlaybackState::Playing => "Playing",
                    PlaybackState::Paused => "Paused",
                    PlaybackState::Error => "Error",
                };
                let mut text = format!("{state_text} — {}", status.device);
                if let Some(message) = &status.message {
                    text.push_str(&format!(" ({message})"));
                }
                state_label.set_text(&text);

                let source = status
                    .source
                    .clone()
                    .unwrap_or_else(|| "No video selected".to_string());
                source_row.set_subtitle(&if status.loop_video {
                    format!("{source}  •  loop")
                } else {
                    source
                });

                loop_row.set_active(status.loop_video);
                pause_button.set_label(if status.state == PlaybackState::Paused {
                    "Resume"
                } else {
                    "Pause"
                });

                let duration = status.duration_ms.max(1) as f64;
                if (seek_scale.adjustment().upper() - duration).abs() > 1.0 {
                    seek_scale.set_range(0.0, duration);
                }
                if !user_seeking.get() {
                    seek_scale.set_value(status.position_ms as f64);
                }
                position_label.set_text(&if status.duration_ms > 0 {
                    format!(
                        "{} / {}",
                        format_time_ms(status.position_ms),
                        format_time_ms(status.duration_ms)
                    )
                } else {
                    format_time_ms(status.position_ms)
                });

                width_row.set_value(f64::from(status.output.width));
                height_row.set_value(f64::from(status.output.height));
                fps_row.set_value(f64::from(status.output.fps));
                pixel_row.set_selected(if status.output.pixel_format == "YUYV" {
                    1
                } else {
                    0
                });
                fit_row.set_selected(match status.output.fit.as_str() {
                    "fill" => 1,
                    "stretch" => 2,
                    _ => 0,
                });
                rotate_row.set_selected(match status.output.rotate {
                    90 => 1,
                    180 => 2,
                    270 => 3,
                    _ => 0,
                });
                mirror_row.set_active(status.output.mirror);

                let readers = if status.consumers.is_empty() {
                    "Nobody is using the camera".to_string()
                } else {
                    status
                        .consumers
                        .iter()
                        .map(|consumer| {
                            format!("{} ({})", consumer.label(), consumer.mode.as_str())
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                consumers_row.set_subtitle(&readers);

                let profile_text = match (&status.profile, &status.pending) {
                    (Some(name), _) => format!("Active: {name}"),
                    (None, Some(pending)) => format!("Deferred: {pending}"),
                    (None, None) => "No profile active".to_string(),
                };
                profile_row.set_subtitle(&profile_text);

                updating.set(false);
            }

            glib::ControlFlow::Continue
        });
    }

    {
        let running = running.clone();
        window.connect_close_request(move |_| {
            running.store(false, Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }

    window.present();
}

fn defaults_loop(config: &Config) -> bool {
    config.source.loop_video
}

fn show_doctor(window: &adw::ApplicationWindow, report: &DoctorReport) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("vcam doctor")
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    for check in &report.checks {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let mark = gtk::Label::new(Some(if check.ok { "✓" } else { "✗" }));
        mark.set_width_chars(2);
        let text = gtk::Label::new(Some(&format!("{}: {}", check.name, check.detail)));
        text.set_halign(gtk::Align::Start);
        text.set_wrap(true);
        text.set_xalign(0.0);
        text.set_selectable(true);
        row.append(&mark);
        row.append(&text);
        body.append(&row);
    }
    if report.ok {
        body.append(&gtk::Label::new(Some("All required checks passed.")));
    } else {
        body.append(&gtk::Label::new(Some("Some checks failed.")));
    }

    dialog.set_extra_child(Some(&body));
    dialog.add_response("close", "Close");
    dialog.present();
}

// -- IPC worker -----------------------------------------------------------

fn spawn_worker(socket: PathBuf, shared: SharedState) -> mpsc::Sender<WorkerCommand> {
    let (tx, rx) = mpsc::channel::<WorkerCommand>();
    std::thread::Builder::new()
        .name("vcam-gui-commands".to_string())
        .spawn(move || {
            for command in rx {
                let WorkerCommand::Send(command) = command;
                let is_doctor = matches!(&command, Command::Doctor);
                match send_request(&socket, command) {
                    Ok(value) => {
                        if is_doctor {
                            if let Ok(report) = serde_json::from_value::<DoctorReport>(value) {
                                if let Ok(mut shared) = shared.lock() {
                                    shared.doctor = Some(report);
                                    shared.error = None;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Ok(mut shared) = shared.lock() {
                            shared.error = Some(error);
                        }
                    }
                }
            }
        })
        .expect("failed to spawn command worker");
    tx
}

fn spawn_status_reader(socket: PathBuf, shared: SharedState, running: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("vcam-gui-status".to_string())
        .spawn(move || {
            while running.load(Ordering::Relaxed) {
                match UnixStream::connect(&socket) {
                    Ok(stream) => {
                        if let Ok(mut shared) = shared.lock() {
                            shared.connected = true;
                        }
                        let reader = BufReader::new(stream);
                        for line in reader.lines() {
                            if !running.load(Ordering::Relaxed) {
                                break;
                            }
                            let Ok(line) = line else { break };
                            if let Ok(ServerMessage::Event(event)) =
                                serde_json::from_str::<ServerMessage>(&line)
                            {
                                if event.event == "status" {
                                    if let Ok(status) = serde_json::from_value::<Status>(event.data)
                                    {
                                        if let Ok(mut shared) = shared.lock() {
                                            shared.status = Some(status);
                                            shared.error = None;
                                        }
                                    }
                                }
                            }
                        }
                        if let Ok(mut shared) = shared.lock() {
                            shared.connected = false;
                        }
                    }
                    Err(_) => {
                        if let Ok(mut shared) = shared.lock() {
                            shared.connected = false;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        })
        .expect("failed to spawn status reader");
}

fn send_request(socket: &PathBuf, command: Command) -> Result<serde_json::Value, String> {
    let stream = UnixStream::connect(socket).map_err(|error| {
        format!(
            "cannot connect to {}: {error}. Is vcamd running?",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| format!("failed to set socket timeout: {error}"))?;
    let mut stream = stream;
    let request = Request::new(1, command);
    let mut text = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .map_err(|error| format!("failed to send request: {error}"))?;
    stream.flush().map_err(|error| error.to_string())?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|error| format!("failed to read response: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Response(response)) if response.id == 1 => {
                if response.ok {
                    return Ok(response.data.unwrap_or(serde_json::Value::Null));
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

// -- Preview --------------------------------------------------------------

fn spawn_preview(
    preview_path: PathBuf,
    frame: Arc<Mutex<Option<PreviewFrame>>>,
    running: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("vcam-gui-preview".to_string())
        .spawn(move || {
            while running.load(Ordering::Relaxed) {
                if let Some(preview) = read_preview(&preview_path) {
                    if let Ok(mut slot) = frame.lock() {
                        *slot = Some(preview);
                    }
                }
                std::thread::sleep(Duration::from_millis(80));
            }
        })
        .expect("failed to spawn preview thread");
}

/// Read the daemon's preview file. The daemon writes a temporary file and
/// renames it into place, so a successful read is always a complete frame.
fn read_preview(path: &std::path::Path) -> Option<PreviewFrame> {
    let data = std::fs::read(path).ok()?;
    if data.len() < PREVIEW_HEADER_LEN || data[..8] != PREVIEW_MAGIC {
        return None;
    }
    let width = u16::from_le_bytes([data[8], data[9]]) as u32;
    let height = u16::from_le_bytes([data[10], data[11]]) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    let len = PREVIEW_HEADER_LEN + width as usize * height as usize * 3;
    if data.len() < len {
        return None;
    }
    Some(PreviewFrame {
        width,
        height,
        stride: width as usize * 3,
        data: data[PREVIEW_HEADER_LEN..len].to_vec(),
    })
}
