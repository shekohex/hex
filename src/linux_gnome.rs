//! GNOME Shell adapter. The extension owns compositor objects; Rust owns dictation semantics.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atspi::zbus::{Connection, Proxy};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use futures_util::StreamExt;

use crate::linux_input::{DOUBLE_TAP_WINDOW, HotkeyEvent, HotkeyStatus};
use crate::linux_settings::LinuxHotkey;

const BUS: &str = "dev.hex.Gnome";
const PATH: &str = "/dev/hex/Gnome";
const INTERFACE: &str = "dev.hex.Gnome1";
static CLIENT: Mutex<Option<Client>> = Mutex::new(None);
static INDICATOR: AtomicU8 = AtomicU8::new(0);

#[derive(Clone)]
struct Client {
    connection: Connection,
    lease: u32,
}

pub fn is_gnome() -> bool {
    crate::linux_session::LinuxSession::detect().is_wayland()
        && desktop_is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

fn desktop_is_gnome(desktop: &str) -> bool {
    desktop
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("gnome"))
}

pub fn set_indicator(recording: bool, pending: usize) {
    INDICATOR.store(
        if recording {
            1
        } else if pending > 0 {
            2
        } else {
            0
        },
        Ordering::Release,
    );
}

fn client() -> Result<Client> {
    CLIENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .ok_or_else(|| eyre!("Enable the HEX GNOME extension and start the listener"))
}

impl Client {
    async fn proxy(&self) -> Result<Proxy<'_>> {
        Ok(Proxy::new(&self.connection, BUS, PATH, INTERFACE).await?)
    }
}

pub fn focused_window() -> Result<(String, String)> {
    let client = client()?;
    crate::cloud::run_cancellable(&AtomicBool::new(false), Duration::from_millis(400), async {
        Ok(client
            .proxy()
            .await?
            .call("FocusedWindow", &(client.lease,))
            .await?)
    })
}

pub fn clipboard() -> Result<String> {
    let client = client()?;
    crate::cloud::run_cancellable(&AtomicBool::new(false), Duration::from_millis(400), async {
        let text: String = client
            .proxy()
            .await?
            .call("Clipboard", &(client.lease,))
            .await?;
        if text.len() > 32 * 1024 {
            bail!("Clipboard context exceeds 32 KiB");
        }
        Ok(text)
    })
}

pub fn capture_window(identity: &str, maximum_dimension: u32, limit: usize) -> Result<Vec<u8>> {
    let client = client()?;
    crate::cloud::run_cancellable(&AtomicBool::new(false), Duration::from_millis(750), async {
        let bytes: Vec<u8> = client
            .proxy()
            .await?
            .call(
                "CaptureWindow",
                &(client.lease, identity, maximum_dimension),
            )
            .await?;
        if bytes.len() > limit {
            bail!("Focused window image exceeds its size limit");
        }
        Ok(bytes)
    })
}

pub fn paste(
    text: &str,
    with_shift: bool,
    cancelled: &AtomicBool,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let client = client()?;
    while crate::cloud::run_cancellable(cancelled, Duration::from_secs(2), async {
        Ok(client
            .proxy()
            .await?
            .call::<_, _, bool>("ModifiersHeld", &(client.lease,))
            .await?)
    })? {
        std::thread::sleep(Duration::from_millis(20));
    }
    commit()?;
    crate::cloud::run_cancellable(cancelled, Duration::from_secs(3), async {
        Ok(client
            .proxy()
            .await?
            .call("Paste", &(client.lease, text, with_shift))
            .await?)
    })
}

pub fn run(
    sender: SyncSender<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    binding: LinuxHotkey,
    double_tap: bool,
    started: Arc<AtomicBool>,
    status: HotkeyStatus,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let connection = Connection::session().await?;
        let proxy = Proxy::new(&connection, BUS, PATH, INTERFACE).await?;
        let mut shortcuts = proxy.receive_signal("Shortcut").await?;
        let mut cancellations = proxy.receive_signal("Cancel").await?;
        let lease: u32 = tokio::time::timeout(Duration::from_millis(1500), proxy.call("Acquire", &(accelerator(&binding)?,))).await?
            .wrap_err("GNOME integration unavailable: install and enable hex@shekohex.dev, then log out and back in")?;
        *CLIENT.lock().unwrap_or_else(|error| error.into_inner()) = Some(Client { connection: connection.clone(), lease });
        struct ClearClient;
        impl Drop for ClearClient {
            fn drop(&mut self) {
                *CLIENT.lock().unwrap_or_else(|error| error.into_inner()) = None;
            }
        }
        let _clear_client = ClearClient;
        started.store(true, Ordering::Release);
        let _ = ready.try_send(Ok(()));
        let mut gesture = ShortcutGesture::default();
        let mut displayed = u8::MAX;
        let mut last_update = Instant::now();
        let mut displayed_transcript = String::new();
        let mut transcript_supported = true;
        let mut last_transcript_update = Instant::now();
        let result: Result<()> = async {
            loop {
                if stop.load(Ordering::Acquire) { return Ok(()); }
                let desired = INDICATOR.load(Ordering::Acquire);
                if desired != displayed || last_update.elapsed() >= Duration::from_secs(1) {
                    let available: bool = tokio::time::timeout(Duration::from_secs(1), proxy.call("SetState", &(lease, u32::from(desired)))).await??;
                    status.update_escape_availability(available);
                    displayed = desired;
                    last_update = Instant::now();
                }
                if transcript_supported && last_transcript_update.elapsed() >= Duration::from_millis(100) {
                    let transcript = crate::linux_desktop::live_transcript();
                    if transcript != displayed_transcript {
                        match tokio::time::timeout(Duration::from_millis(200), proxy.call::<_, _, ()>("SetTranscript", &(lease, &transcript))).await {
                            Ok(Ok(())) => displayed_transcript = transcript,
                            _ => {
                                // Older Shell modules remain loaded until logout. Never
                                // break shortcuts because an optional HUD method is absent.
                                tracing::warn!("Live HUD unavailable: reload the HEX GNOME extension or log out and back in");
                                transcript_supported = false;
                            }
                        }
                    }
                    last_transcript_update = Instant::now();
                }
                let event = tokio::select! {
                    message = shortcuts.next() => {
                        let message = message.ok_or_else(|| eyre!("GNOME shortcut stream closed"))?;
                        let (session, pressed): (u32, bool) = message.body().deserialize()?;
                        (session == lease).then(|| gesture.edge(pressed, double_tap, Instant::now())).flatten()
                    }
                    message = cancellations.next() => {
                        let message = message.ok_or_else(|| eyre!("GNOME cancellation stream closed"))?;
                        let (session,): (u32,) = message.body().deserialize()?;
                        if session == lease && (gesture.cancel() || status.output_pending()) { Some(HotkeyEvent::Cancel) } else { None }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => None,
                };
                if let Some(event) = event {
                    sender.try_send(event).map_err(|_| eyre!("GNOME hotkey queue is full or closed"))?;
                }
            }
        }.await;
        let _ = tokio::time::timeout(Duration::from_millis(500), proxy.call::<_, _, ()>("Release", &(lease,))).await;
        result
    })
}

fn accelerator(binding: &LinuxHotkey) -> Result<String> {
    binding.validate()?;
    let mut accelerator = String::new();
    for (enabled, name) in [
        (binding.control, "Control"),
        (binding.alt, "Alt"),
        (binding.shift, "Shift"),
        (binding.super_key, "Super"),
    ] {
        if enabled {
            accelerator.push_str(&format!("<{name}>"));
        }
    }
    let key = match binding.key.as_str() {
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "tab" => "Tab",
        key => key,
    };
    if key.contains(['<', '>', '\r', '\n']) || key.len() > 64 {
        bail!("Invalid GNOME shortcut key");
    }
    if key.starts_with('f') && key[1..].parse::<u8>().is_ok() {
        accelerator.push_str(&key.to_uppercase());
    } else {
        accelerator.push_str(key);
    }
    Ok(accelerator)
}

#[derive(Default)]
struct ShortcutGesture {
    held: bool,
    locked: bool,
    second_tap: bool,
    last_release: Option<Instant>,
    finishing_lock: bool,
}

impl ShortcutGesture {
    fn edge(&mut self, pressed: bool, double_tap: bool, now: Instant) -> Option<HotkeyEvent> {
        if pressed {
            if self.held {
                return None;
            }
            self.held = true;
            if self.locked {
                self.locked = false;
                self.finishing_lock = true;
                self.second_tap = false;
                self.last_release = None;
                return Some(HotkeyEvent::Finish);
            }
            self.second_tap = double_tap
                && self
                    .last_release
                    .take()
                    .is_some_and(|released| now.duration_since(released) < DOUBLE_TAP_WINDOW);
            return Some(HotkeyEvent::Start);
        }
        if !self.held {
            return None;
        }
        self.held = false;
        if std::mem::take(&mut self.finishing_lock) {
            return None;
        }
        if self.second_tap {
            self.second_tap = false;
            self.locked = true;
            return None;
        }
        self.last_release = double_tap.then_some(now);
        Some(HotkeyEvent::Finish)
    }

    fn cancel(&mut self) -> bool {
        let active = self.held || self.locked;
        *self = Self::default();
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_detection_does_not_match_other_wayland_desktops() {
        assert!(desktop_is_gnome("ubuntu:GNOME"));
        assert!(desktop_is_gnome("gnome"));
        assert!(!desktop_is_gnome("sway"));
        assert!(!desktop_is_gnome(""));
        assert_eq!(accelerator(&LinuxHotkey::default()).unwrap(), "<Alt>space");
    }

    #[test]
    fn shortcut_edges_preserve_hold_double_tap_and_cancellation() {
        let now = Instant::now();
        let mut gesture = ShortcutGesture::default();
        assert_eq!(gesture.edge(true, true, now), Some(HotkeyEvent::Start));
        assert_eq!(gesture.edge(true, true, now), None);
        assert_eq!(gesture.edge(false, true, now), Some(HotkeyEvent::Finish));
        assert_eq!(
            gesture.edge(true, true, now + Duration::from_millis(100)),
            Some(HotkeyEvent::Start)
        );
        assert_eq!(
            gesture.edge(false, true, now + Duration::from_millis(150)),
            None
        );
        assert!(gesture.locked);
        assert!(gesture.cancel());
        assert!(!gesture.cancel());
    }
}
