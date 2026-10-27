//! GNOME Shell adapter. The extension owns compositor objects; Rust owns dictation semantics.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atspi::zbus::{Connection, Proxy};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use futures_util::StreamExt;

use crate::linux_input::LinuxHotkey;
use crate::linux_input::{Dispatcher, HotkeyOptions, HotkeyStatus, Input, Shortcut};

const BUS: &str = "dev.hex.Gnome";
const PATH: &str = "/dev/hex/Gnome";
const INTERFACE: &str = "dev.hex.Gnome1";
static CLIENT: Mutex<Option<Client>> = Mutex::new(None);
static INDICATOR: AtomicU8 = AtomicU8::new(0);
static HUD_POSITION: AtomicU8 = AtomicU8::new(0);

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

pub fn set_indicator(
    recording: bool,
    pending: usize,
    position: crate::app_settings::IndicatorPosition,
) {
    HUD_POSITION.store(position as u8, Ordering::Release);
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
    mut dispatcher: Dispatcher,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    binding: LinuxHotkey,
    options: HotkeyOptions,
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
        let mut named_shortcuts = proxy.receive_signal("ShortcutNamed").await?;
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
        let paste_last = options.paste_last.as_ref().map(accelerator).transpose()?.unwrap_or_default();
        let voice_action = options.voice_action.as_ref().map(accelerator).transpose()?.unwrap_or_default();
        let extras_bound = matches!(tokio::time::timeout(Duration::from_secs(1), proxy.call::<_, _, ()>("BindShortcuts", &(lease, &paste_last, &voice_action))).await, Ok(Ok(())));
        if !extras_bound && (!paste_last.is_empty() || !voice_action.is_empty()) {
            tracing::warn!("Additional GNOME shortcuts unavailable; reload the Hex extension or use hex paste-last / hex voice-action");
        }
        let mut granted = vec![crate::desktop_host::DesktopGrantedShortcut { name: "Dictation".into(), trigger: Some(binding.label()) }];
        for (name, binding) in [("Paste last dictation", &options.paste_last), ("Voice Action", &options.voice_action)] {
            if let Some(binding) = binding {
                granted.push(crate::desktop_host::DesktopGrantedShortcut { name: name.into(), trigger: extras_bound.then(|| binding.label()) });
            }
        }
        status.set_shortcuts(crate::desktop_host::DesktopShortcuts::Portal(granted));
        started.store(true, Ordering::Release);
        let _ = ready.try_send(Ok(()));

        let mut displayed = u8::MAX;
        let mut displayed_position = u8::MAX;
        let mut position_supported = true;
        let mut last_update = Instant::now();
        let mut displayed_transcript = String::new();
        let mut transcript_supported = true;
        let mut last_transcript_update = Instant::now();
        let result: Result<()> = async {
            loop {
                if stop.load(Ordering::Acquire) { return Ok(()); }
                let desired = INDICATOR.load(Ordering::Acquire);
                let position = HUD_POSITION.load(Ordering::Acquire);
                if position_supported && position != displayed_position {
                    match tokio::time::timeout(Duration::from_millis(200), proxy.call::<_, _, ()>("SetHud", &(lease, u32::from(position)))).await {
                        Ok(Ok(())) => displayed_position = position,
                        _ => { position_supported = false; tracing::warn!("HUD position unavailable; reload the Hex GNOME extension or log out and back in"); }
                    }
                }
                if desired != displayed || last_update.elapsed() >= Duration::from_secs(1) {
                    let available: bool = tokio::time::timeout(Duration::from_secs(1), proxy.call("SetState", &(lease, u32::from(desired)))).await??;
                    status.update_escape_availability(available);
                    displayed = desired;
                    last_update = Instant::now();
                }
                if transcript_supported && last_transcript_update.elapsed() >= Duration::from_millis(100) {
                    let transcript = crate::linux_rune_hud::live_transcript();
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
                        (session == lease).then_some(if pressed { Input::Pressed(Shortcut::Dictation) } else { Input::Released(Shortcut::Dictation) })
                    }
                    message = named_shortcuts.next() => {
                        let message = message.ok_or_else(|| eyre!("GNOME named shortcut stream closed"))?;
                        let (session, name, pressed): (u32, String, bool) = message.body().deserialize()?;
                        let shortcut = match name.as_str() { "paste-last" => Some(Shortcut::PasteLast), "voice-action" => Some(Shortcut::VoiceAction), _ => None };
                        shortcut.filter(|_| session == lease).map(|shortcut| if pressed { Input::Pressed(shortcut) } else { Input::Released(shortcut) })
                    }
                    message = cancellations.next() => {
                        let message = message.ok_or_else(|| eyre!("GNOME cancellation stream closed"))?;
                        let (session,): (u32,) = message.body().deserialize()?;
                        if session == lease { Some(Input::Escape) } else { None }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => None,
                };
                dispatcher.commands();
                if let Some(event) = event {
                    dispatcher.input(event);
                }
                if !dispatcher.flush()? { return Ok(()); }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gnome_detection_and_accelerator() {
        assert!(desktop_is_gnome("ubuntu:GNOME"));
        assert!(!desktop_is_gnome("sway"));
        assert_eq!(accelerator(&LinuxHotkey::default()).unwrap(), "<Alt>space");
    }
}
