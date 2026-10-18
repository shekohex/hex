//! Optional, per-recording context. Never injects Copy, steals focus, or captures a desktop.
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

use crate::cloud::ContextImage;
use crate::linux_pipeline::PipelineSettings;
use crate::linux_session::LinuxSession;

const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
// A stalled desktop adapter cannot cause unbounded worker creation.
static CAPTURE_BUSY: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenContextMode {
    #[default]
    Off,
    Ocr,
    Screenshot,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ContextSettings {
    pub clipboard: bool,
    pub active_field: bool,
    pub screen: ScreenContextMode,
    pub ocr_languages: String,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            clipboard: false,
            active_field: false,
            screen: ScreenContextMode::Off,
            ocr_languages: "eng".into(),
        }
    }
}

#[derive(Default)]
pub struct CaptureContext {
    pub application: Option<String>,
    pub clipboard: Option<String>,
    pub active_field: Option<String>,
    pub screenshot: Option<ContextImage>,
    pub screen_text: Option<String>,
}

#[derive(Clone)]
pub struct ContextCapture {
    result: Arc<Mutex<Receiver<CaptureContext>>>,
    captured: Arc<OnceLock<Arc<CaptureContext>>>,
}

impl ContextCapture {
    pub fn start(settings: PipelineSettings) -> Self {
        let (sender, result) = mpsc::sync_channel(1);
        if CAPTURE_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            std::thread::spawn(move || {
                struct Busy;
                impl Drop for Busy {
                    fn drop(&mut self) {
                        CAPTURE_BUSY.store(false, Ordering::Release);
                    }
                }
                let _busy = Busy;
                let _ = sender.try_send(capture(&settings));
            });
        }
        Self {
            result: Arc::new(Mutex::new(result)),
            captured: Arc::default(),
        }
    }

    pub fn take(&self) -> Arc<CaptureContext> {
        self.captured
            .get_or_init(|| {
                Arc::new(
                    self.result
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap_or_default(),
                )
            })
            .clone()
    }
}

fn capture(settings: &PipelineSettings) -> CaptureContext {
    if settings.modes.is_empty() && !settings.default_mode.enabled {
        return CaptureContext::default();
    }
    let wayland = LinuxSession::detect().is_wayland();
    let gnome = crate::linux_gnome::is_gnome();
    let window = (!wayland).then(x11_window).and_then(Result::ok);
    let gnome_window = gnome
        .then(crate::linux_gnome::focused_window)
        .and_then(Result::ok)
        .filter(|(identity, _)| !identity.is_empty());
    let wayland_window = (wayland && !gnome).then(focused_wayland_window).flatten();
    // AT-SPI also supplies active-app identity on Wayland. No screen-reader setting is changed.
    let accessible = accessibility(
        settings,
        window
            .as_ref()
            .map(|window| window.application.as_str())
            .or_else(|| {
                gnome_window
                    .as_ref()
                    .map(|(_, application)| application.as_str())
            })
            .or_else(|| {
                wayland_window
                    .as_ref()
                    .map(|window| window.application.as_str())
            }),
    )
    .unwrap_or_default();
    let application = window
        .as_ref()
        .map(|window| window.application.clone())
        .or_else(|| {
            gnome_window
                .as_ref()
                .map(|(_, application)| application.clone())
        })
        .or_else(|| {
            wayland_window
                .as_ref()
                .map(|window| window.application.clone())
        })
        .or(accessible.application);
    let mode = settings.select(application.as_deref());
    let mut context = CaptureContext {
        application,
        ..Default::default()
    };
    if !mode.enabled || accessible.protected {
        return context;
    }
    if mode.context.active_field {
        context.active_field = accessible.field;
    }
    if mode.context.clipboard {
        if gnome {
            context.clipboard = crate::linux_gnome::clipboard()
                .ok()
                .filter(|text| !text.is_empty());
        } else {
            let mut command = if wayland {
                let mut command = Command::new("wl-paste");
                command.args(["--no-newline", "--type", "text/plain"]);
                command
            } else {
                let mut command = Command::new("xclip");
                command.args(["-selection", "clipboard", "-out"]);
                command
            };
            context.clipboard = read_helper(&mut command, MAX_TEXT_BYTES)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|text| !text.is_empty());
        }
    }
    if mode.context.screen != ScreenContextMode::Off {
        let local_ocr = mode.context.screen == ScreenContextMode::Ocr;
        let maximum_dimension = if local_ocr { 2800.0 } else { 1024.0 };
        let image_limit = if local_ocr {
            16 * 1024 * 1024
        } else {
            MAX_IMAGE_BYTES
        };
        let image = if gnome {
            gnome_window.as_ref().and_then(|(identity, _)| {
                crate::linux_gnome::capture_window(identity, maximum_dimension as u32, image_limit)
                    .ok()
            })
        } else if wayland {
            wayland_window
                .as_ref()
                .map(|window| window.extents)
                .or(accessible.extents)
                .and_then(|(x, y, width, height)| {
                    if width <= 0
                        || height <= 0
                        || width > 8192
                        || height > 8192
                        || x == i32::MIN
                        || y == i32::MIN
                    {
                        return None;
                    }
                    if let Some(window) = &wayland_window
                        && focused_wayland_window()
                            .is_none_or(|current| current.identity != window.identity)
                    {
                        return None;
                    }
                    let geometry = format!("{x},{y} {width}x{height}");
                    let scale = (maximum_dimension / width.max(height) as f32)
                        .min(1.0)
                        .to_string();
                    read_helper(
                        Command::new("grim")
                            .args(["-g", &geometry, "-s", &scale, "-t", "png", "-"]),
                        image_limit,
                    )
                    .ok()
                })
        } else {
            window
                .as_ref()
                .and_then(|window| screenshot_x11(window, maximum_dimension, image_limit).ok())
        };
        if local_ocr {
            context.screen_text = image
                .and_then(|bytes| recognize_screen_text(&bytes, &mode.context.ocr_languages).ok());
        } else {
            context.screenshot = image.map(|bytes| ContextImage {
                mime_type: "image/png",
                bytes,
            });
        }
    }
    if let Some(window) = window
        && !x11_window().is_ok_and(|current| current.id == window.id)
    {
        return CaptureContext::default();
    }
    if let Some((identity, _)) = gnome_window
        && !crate::linux_gnome::focused_window().is_ok_and(|(current, _)| current == identity)
    {
        return CaptureContext::default();
    }
    context
}

struct WaylandWindow {
    identity: String,
    application: String,
    extents: (i32, i32, i32, i32),
}

fn focused_wayland_window() -> Option<WaylandWindow> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        let bytes = read_helper(
            Command::new("hyprctl").args(["activewindow", "-j"]),
            32 * 1024,
        )
        .ok()?;
        let window: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        return Some(WaylandWindow {
            identity: window["address"].as_str()?.into(),
            application: window["class"].as_str()?.into(),
            extents: (
                window["at"][0].as_i64()?.try_into().ok()?,
                window["at"][1].as_i64()?.try_into().ok()?,
                window["size"][0].as_i64()?.try_into().ok()?,
                window["size"][1].as_i64()?.try_into().ok()?,
            ),
        });
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        let bytes = read_helper(
            Command::new("swaymsg").args(["-t", "get_tree", "-r"]),
            512 * 1024,
        )
        .ok()?;
        let tree: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let mut pending = vec![&tree];
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > 1024 {
                break;
            }
            if node["focused"] == true {
                return Some(WaylandWindow {
                    identity: node["id"].as_u64()?.to_string(),
                    application: node["app_id"]
                        .as_str()
                        .or(node["window_properties"]["class"].as_str())?
                        .into(),
                    extents: (
                        node["rect"]["x"].as_i64()?.try_into().ok()?,
                        node["rect"]["y"].as_i64()?.try_into().ok()?,
                        node["rect"]["width"].as_i64()?.try_into().ok()?,
                        node["rect"]["height"].as_i64()?.try_into().ok()?,
                    ),
                });
            }
            for children in ["nodes", "floating_nodes"] {
                pending.extend(node[children].as_array().into_iter().flatten());
            }
        }
    }
    None
}

#[derive(Default)]
struct AccessibleContext {
    application: Option<String>,
    field: Option<String>,
    protected: bool,
    extents: Option<(i32, i32, i32, i32)>,
}

fn accessibility(
    settings: &PipelineSettings,
    application_hint: Option<&str>,
) -> Result<AccessibleContext> {
    crate::cloud::run_cancellable(&AtomicBool::new(false), Duration::from_millis(600), async {
        use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
        use atspi::proxy::{bus::BusProxy, component::ComponentProxy, text::TextProxy};
        use atspi::{CoordType, Role, State, zbus};

        let session = zbus::Connection::session().await?;
        let address = BusProxy::new(&session).await?.get_address().await?;
        let bus = zbus::connection::Builder::address(address.as_str())?
            .build()
            .await?;
        let desktop = AccessibleProxy::builder(&bus)
            .destination("org.a11y.atspi.Registry")?
            .path("/org/a11y/atspi/accessible/root")?
            .build()
            .await?;
        for application in desktop.get_children().await?.into_iter().take(128) {
            let application = application.as_accessible_proxy(&bus).await?;
            for window_ref in application.get_children().await?.into_iter().take(64) {
                let window = window_ref.as_accessible_proxy(&bus).await?;
                if !window.get_state().await?.contains(State::Active) {
                    continue;
                }
                let mut result = AccessibleContext {
                    application: application.name().await.ok(),
                    ..Default::default()
                };
                let mode = settings.select(application_hint.or(result.application.as_deref()));
                if !mode.enabled
                    || (mode.context.screen == ScreenContextMode::Off && !mode.context.active_field)
                {
                    return Ok(result);
                }
                let component = ComponentProxy::builder(&bus)
                    .destination(window.inner().destination().clone())?
                    .path(window.inner().path().clone())?
                    .build()
                    .await?;
                result.extents = component.get_extents(CoordType::Screen).await.ok();
                let mut pending = vec![window_ref];
                let mut scanned = 0;
                while let Some(object) = pending.pop() {
                    scanned += 1;
                    if scanned > 512 {
                        break;
                    }
                    let object = object.as_accessible_proxy(&bus).await?;
                    if object.get_state().await?.contains(State::Focused) {
                        result.protected = object.get_role().await? == Role::PasswordText;
                        if result.protected {
                            return Ok(result);
                        }
                        if !mode.context.active_field {
                            return Ok(result);
                        }
                        let text = TextProxy::builder(&bus)
                            .destination(object.inner().destination().clone())?
                            .path(object.inner().path().clone())?
                            .build()
                            .await?;
                        let count = text
                            .character_count()
                            .await
                            .unwrap_or_default()
                            .clamp(0, 8192);
                        result.field = text
                            .get_text(0, count)
                            .await
                            .ok()
                            .filter(|text| text.len() <= MAX_TEXT_BYTES && !text.is_empty());
                        return Ok(result);
                    }
                    let children = object.get_children().await?;
                    pending.extend(
                        children
                            .into_iter()
                            .take(512usize.saturating_sub(scanned + pending.len())),
                    );
                }
                return Ok(result);
            }
        }
        Ok(AccessibleContext::default())
    })
}

struct X11Window {
    id: u32,
    application: String,
}

fn x11_window() -> Result<X11Window> {
    let (connection, screen) = x11rb::connect(None)?;
    let atom = connection
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let id = connection
        .get_property(
            false,
            connection.setup().roots[screen].root,
            atom,
            AtomEnum::WINDOW,
            0,
            1,
        )?
        .reply()?
        .value32()
        .and_then(|mut values| values.next())
        .filter(|id| *id != 0)
        .ok_or_else(|| eyre!("Desktop exposes no active window"))?;
    let class = connection
        .get_property(false, id, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?
        .reply()?
        .value;
    let application = class
        .split(|byte| *byte == 0)
        .rfind(|part| !part.is_empty())
        .and_then(|part| std::str::from_utf8(part).ok())
        .unwrap_or_default()
        .to_owned();
    Ok(X11Window { id, application })
}

fn screenshot_x11(window: &X11Window, maximum_dimension: f32, limit: usize) -> Result<Vec<u8>> {
    if x11_window()?.id != window.id {
        bail!("Focused window changed");
    }
    let (connection, _) = x11rb::connect(None)?;
    let geometry = connection.get_geometry(window.id)?.reply()?;
    if u64::from(geometry.width) * u64::from(geometry.height) > 8_388_608 {
        bail!("Focused window is too large");
    }
    let (image, visual_id) = x11rb::image::Image::get(
        &connection,
        window.id,
        0,
        0,
        geometry.width,
        geometry.height,
    )?;
    let visual = connection
        .setup()
        .roots
        .iter()
        .flat_map(|screen| &screen.allowed_depths)
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == visual_id)
        .ok_or_else(|| eyre!("Unsupported window pixel format"))?;
    let scale = (maximum_dimension / f32::from(geometry.width.max(geometry.height))).min(1.0);
    let width = (f32::from(geometry.width) * scale).max(1.0) as u16;
    let height = (f32::from(geometry.height) * scale).max(1.0) as u16;
    let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for y in 0..height {
        for x in 0..width {
            let pixel =
                image.get_pixel((f32::from(x) / scale) as u16, (f32::from(y) / scale) as u16);
            for mask in [visual.red_mask, visual.green_mask, visual.blue_mask] {
                if mask == 0 {
                    bail!("Unsupported window pixel format");
                }
                pixels.push(
                    (((pixel & mask) >> mask.trailing_zeros()) as u64 * 255
                        / (mask >> mask.trailing_zeros()) as u64) as u8,
                );
            }
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, u32::from(width), u32::from(height));
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&pixels)?;
    }
    if bytes.len() > limit {
        bail!("Focused window image exceeds its size limit");
    }
    Ok(bytes)
}

fn read_helper(command: &mut Command, limit: usize) -> Result<Vec<u8>> {
    run_helper(command, None, limit, Duration::from_millis(750))
}

fn recognize_screen_text(image: &[u8], languages: &str) -> Result<String> {
    let text = run_helper(
        Command::new("tesseract")
            .args(["stdin", "stdout", "-l", languages])
            .env("OMP_THREAD_LIMIT", "1"),
        Some(image),
        MAX_TEXT_BYTES,
        Duration::from_secs(2),
    )?;
    String::from_utf8(text)
        .map(|text| text.trim().to_owned())
        .map_err(Into::into)
}

fn run_helper(
    command: &mut Command,
    input: Option<&[u8]>,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut child = command
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("Context helper has no output"))?;
    let fd = stdout.as_raw_fd();
    let result = (|| {
        let mut stdin = child.stdin.take();
        for fd in std::iter::once(fd).chain(stdin.as_ref().map(AsRawFd::as_raw_fd)) {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let mut input = input.unwrap_or_default();
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            if Instant::now() >= deadline {
                bail!("Context helper timed out");
            }
            if let Some(writer) = &mut stdin {
                match writer.write(input) {
                    Ok(count) => input = &input[count..],
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
                if input.is_empty() {
                    stdin = None;
                }
            }
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        if status.success() {
                            return Ok(bytes);
                        }
                        bail!("Context helper failed");
                    }
                }
                Ok(count) => {
                    if bytes.len() + count > limit {
                        bail!("Context exceeds its size limit");
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(error) => return Err(error.into()),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_helper_streams_large_input_without_files_or_blocking_pipes() {
        let input = b"window pixels".repeat(16_384);
        assert_eq!(
            run_helper(
                Command::new("cat").args([] as [&str; 0]),
                Some(&input),
                input.len(),
                Duration::from_secs(2)
            )
            .unwrap(),
            input
        );
        let started = Instant::now();
        assert!(
            run_helper(
                Command::new("sleep").arg("5"),
                Some(&input),
                32,
                Duration::from_millis(80)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[ignore = "requires Tesseract with eng language data; no screen or network access"]
    fn local_ocr_reads_fixture_pixels_without_persisting_an_image() {
        let text =
            recognize_screen_text(include_bytes!("../tests/fixtures/ocr-context.png"), "eng")
                .unwrap();
        assert!(text.contains("HEX"), "{text}");
        assert!(text.contains("3000"), "{text}");
    }

    #[test]
    fn context_helper_bounds_bytes_and_waits_for_successful_exit() {
        assert_eq!(
            read_helper(Command::new("/bin/sh").args(["-c", "printf context"]), 16).unwrap(),
            b"context"
        );
        assert!(
            read_helper(
                Command::new("/bin/sh").args(["-c", "printf private; exit 1"]),
                16
            )
            .is_err()
        );
        assert!(read_helper(Command::new("/bin/sh").args(["-c", "printf toolong"]), 3).is_err());
    }
}
