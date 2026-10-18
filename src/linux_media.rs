//! Optional MPRIS pause/resume. Desktop calls never wait on media applications.
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use atspi::zbus::{
    Connection, Proxy,
    proxy::{Builder, CacheProperties},
    zvariant::{ObjectPath, OwnedValue},
};
use color_eyre::Result;
use tokio::sync::watch;

const PLAYER_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const CALL_TIMEOUT: Duration = Duration::from_millis(250);

pub struct MediaController {
    active: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
    pause_ready: Arc<AtomicBool>,
}

impl MediaController {
    pub fn new(enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        let (active, mut receiver) = watch::channel(false);
        let pause_ready = Arc::new(AtomicBool::new(false));
        let ready = pause_ready.clone();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let connection =
                            tokio::time::timeout(Duration::from_secs(1), Connection::session())
                                .await??;
                        let mut paused = Vec::new();
                        while receiver.changed().await.is_ok() {
                            let wanted = *receiver.borrow_and_update();
                            if wanted {
                                if paused.is_empty() {
                                    pause_players(&connection, &receiver, &mut paused).await;
                                }
                                ready.store(true, Ordering::Release);
                            } else {
                                resume_players(&mut paused, &receiver).await;
                            }
                        }
                        // Sender disconnection is shutdown, including error unwinding.
                        resume_players(&mut paused, &receiver).await;
                        Ok(())
                    })
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "media controls unavailable; dictation is unaffected");
            }
        });
        Some(Self {
            active,
            worker: Some(worker),
            pause_ready,
        })
    }

    pub fn update(&self, intentional_capture: bool, pending: usize) {
        let wanted = intentional_capture || pending > 0;
        self.active.send_if_modified(|active| {
            let changed = *active != wanted;
            if changed && wanted {
                self.pause_ready.store(false, Ordering::Release);
            }
            *active = wanted;
            changed
        });
    }

    pub fn pause_attempt_finished(&self) -> bool {
        self.active.is_closed() || self.pause_ready.load(Ordering::Acquire)
    }
}

impl Drop for MediaController {
    fn drop(&mut self) {
        let (replacement, _) = watch::channel(false);
        drop(std::mem::replace(&mut self.active, replacement));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct PausedPlayer {
    // Address unique owner, never a replacement process with the same player name.
    proxy: Proxy<'static>,
    track: Option<String>,
}

async fn track(proxy: &Proxy<'_>) -> atspi::zbus::Result<Option<String>> {
    let metadata: HashMap<String, OwnedValue> = proxy.get_property("Metadata").await?;
    Ok(metadata
        .get("mpris:trackid")
        .and_then(|value| <&ObjectPath<'_>>::try_from(value).ok())
        .map(|path| path.as_str().to_owned()))
}

async fn pause_players(
    connection: &Connection,
    active: &watch::Receiver<bool>,
    paused: &mut Vec<PausedPlayer>,
) {
    let result = tokio::time::timeout(CALL_TIMEOUT, async {
        let bus = Proxy::new(
            connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let names: Vec<String> = bus.call("ListNames", &()).await?;
        Ok::<_, atspi::zbus::Error>((bus, names))
    })
    .await;
    let Ok(Ok((bus, names))) = result else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    for name in names
        .iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .take(16)
    {
        if !*active.borrow() || Instant::now() >= deadline {
            break;
        }
        let candidate = tokio::time::timeout(CALL_TIMEOUT, async {
            let owner: String = bus.call("GetNameOwner", &(name,)).await?;
            let proxy = Builder::<Proxy<'static>>::new(connection)
                .destination(owner)?
                .path(PLAYER_PATH)?
                .interface(PLAYER_INTERFACE)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            let status: String = proxy.get_property("PlaybackStatus").await?;
            let can_pause: bool = proxy.get_property("CanPause").await?;
            if status != "Playing" || !can_pause {
                return Ok(None);
            }
            let track = track(&proxy).await?;
            Ok::<_, atspi::zbus::Error>(Some(PausedPlayer { proxy, track }))
        })
        .await;
        if let Ok(Ok(Some(player))) = candidate {
            if !*active.borrow() {
                break;
            }
            // Keep ownership even if acknowledgement times out after the player paused.
            paused.push(player);
            let _ = tokio::time::timeout(
                CALL_TIMEOUT,
                paused.last().unwrap().proxy.call::<_, _, ()>("Pause", &()),
            )
            .await;
        }
    }
}

async fn resume_players(paused: &mut Vec<PausedPlayer>, active: &watch::Receiver<bool>) {
    while !paused.is_empty() {
        if *active.borrow() && active.has_changed().is_ok() {
            break;
        }
        let player = paused.pop().unwrap();
        let _ = tokio::time::timeout(CALL_TIMEOUT, async {
            let status: String = player.proxy.get_property("PlaybackStatus").await?;
            let can_play: bool = player.proxy.get_property("CanPlay").await?;
            if status == "Paused" && can_play && track(&player.proxy).await? == player.track {
                player.proxy.call::<_, _, ()>("Play", &()).await?;
            }
            Ok::<_, atspi::zbus::Error>(())
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atspi::zbus;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct PlayerState {
        status: String,
        track: u32,
        pauses: usize,
        plays: usize,
    }

    struct Player(Arc<Mutex<PlayerState>>);

    #[atspi::zbus::interface(name = "org.mpris.MediaPlayer2.Player", crate = "atspi::zbus")]
    impl Player {
        fn pause(&self) {
            let mut state = self.0.lock().unwrap();
            state.status = "Paused".into();
            state.pauses += 1;
        }
        fn play(&self) {
            let mut state = self.0.lock().unwrap();
            state.status = "Playing".into();
            state.plays += 1;
        }
        #[zbus(property)]
        fn playback_status(&self) -> String {
            self.0.lock().unwrap().status.clone()
        }
        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            let path = format!("/track/{}", self.0.lock().unwrap().track);
            HashMap::from([(
                "mpris:trackid".into(),
                ObjectPath::try_from(path).unwrap().into(),
            )])
        }
    }

    #[test]
    #[ignore = "requires dbus-run-session and HEX_TEST_MEDIA_BUS=1; never run on the desktop bus"]
    fn media_wire_pauses_only_playing_players_and_resumes_owned_unchanged_tracks() {
        assert_eq!(std::env::var("HEX_TEST_MEDIA_BUS").as_deref(), Ok("1"));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let state = Arc::new(Mutex::new(PlayerState {
                    status: "Playing".into(),
                    ..Default::default()
                }));
                let already_paused = Arc::new(Mutex::new(PlayerState {
                    status: "Paused".into(),
                    ..Default::default()
                }));
                let service = atspi::zbus::connection::Builder::session()
                    .unwrap()
                    .name("org.mpris.MediaPlayer2.hex_fixture")
                    .unwrap()
                    .serve_at(PLAYER_PATH, Player(state.clone()))
                    .unwrap()
                    .build()
                    .await
                    .unwrap();
                let _other = atspi::zbus::connection::Builder::session()
                    .unwrap()
                    .name("org.mpris.MediaPlayer2.hex_paused_fixture")
                    .unwrap()
                    .serve_at(PLAYER_PATH, Player(already_paused.clone()))
                    .unwrap()
                    .build()
                    .await
                    .unwrap();
                let (active, receiver) = watch::channel(true);
                let mut paused = Vec::new();
                pause_players(&service, &receiver, &mut paused).await;
                assert_eq!(state.lock().unwrap().pauses, 1);
                assert_eq!(already_paused.lock().unwrap().pauses, 0);
                resume_players(&mut paused, &receiver).await;
                assert_eq!(
                    state.lock().unwrap().plays,
                    0,
                    "new capture/pending output keeps media paused"
                );
                active.send(false).unwrap();
                resume_players(&mut paused, &receiver).await;
                assert_eq!(state.lock().unwrap().plays, 1);
                assert_eq!(already_paused.lock().unwrap().plays, 0);
                active.send(true).unwrap();
                pause_players(&service, &receiver, &mut paused).await;
                state.lock().unwrap().track += 1;
                active.send(false).unwrap();
                resume_players(&mut paused, &receiver).await;
                assert_eq!(
                    state.lock().unwrap().plays,
                    1,
                    "do not resume changed media"
                );
                active.send(true).unwrap();
                state.lock().unwrap().status = "Playing".into();
                pause_players(&service, &receiver, &mut paused).await;
                drop(active);
                resume_players(&mut paused, &receiver).await;
                assert_eq!(
                    state.lock().unwrap().plays,
                    2,
                    "shutdown restores owned paused media"
                );

                let controller = MediaController::new(true).unwrap();
                controller.update(true, 0);
                let deadline = Instant::now() + Duration::from_secs(2);
                while !controller.pause_attempt_finished() {
                    assert!(Instant::now() < deadline, "media pause never acknowledged");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(state.lock().unwrap().status, "Paused");
                controller.update(false, 1);
                assert_eq!(
                    state.lock().unwrap().plays,
                    2,
                    "accepted cleanup keeps ownership"
                );
                tokio::task::spawn_blocking(move || drop(controller))
                    .await
                    .unwrap();
                assert_eq!(state.lock().unwrap().plays, 3);
            });
    }
}
