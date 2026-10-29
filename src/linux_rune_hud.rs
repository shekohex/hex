//! The Linux recording HUD: the shared Rune HUD (`rune_hud`), the capsule or
//! Dot scope drawn on the GPU, as an overlay in the Hex process.
//! Rune makes it a layer-shell surface on Wayland (overlay layer, no
//! keyboard, empty input region) and an override-redirect window with an
//! empty input shape on X11, so it never takes focus or a click.
//!
//! It only observes: the listener reports its capture, meter, and pending
//! output through `LinuxIndicator`, which never waits on the HUD and turns
//! those reports into the HUD's events. The listener can start before the
//! window's app exists; reports before `install` are dropped.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use rune::Availability;

use crate::app_settings::IndicatorPosition;
use crate::rune_hud::{HudEvent, HudSender};

/// The newest listener's generation: an older listener's reports stop counting.
static GENERATION: Mutex<u64> = Mutex::new(0);
/// Where the newest listener wants the HUD, for an `install` after it starts.
static POSITION: Mutex<IndicatorPosition> = Mutex::new(IndicatorPosition::Top);
static HUD: OnceLock<HudSender> = OnceLock::new();

/// A listener's handle on the HUD; dropping it hides what it reported.
pub(crate) struct LinuxIndicator {
    generation: Option<u64>,
    recording: bool,
    /// Submitted dictations still being transcribed or pasted, oldest first.
    jobs: VecDeque<u64>,
    next_job: u64,
}

impl LinuxIndicator {
    pub(crate) fn new(position: IndicatorPosition) -> Self {
        *POSITION.lock().unwrap_or_else(|error| error.into_inner()) = position;
        if let Some(sender) = HUD.get() {
            sender.set_position(position);
        }
        let generation = (position != IndicatorPosition::Hidden).then(|| {
            let mut current = GENERATION.lock().unwrap_or_else(|error| error.into_inner());
            *current += 1;
            *current
        });
        let indicator = Self {
            generation,
            recording: false,
            jobs: VecDeque::new(),
            next_job: 0,
        };
        indicator.send(HudEvent::Reset);
        indicator
    }

    fn sender(&self) -> Option<&'static HudSender> {
        let generation = self.generation?;
        let current = *GENERATION.lock().unwrap_or_else(|error| error.into_inner());
        (generation == current).then(|| HUD.get()).flatten()
    }

    fn send(&self, event: HudEvent) {
        if let Some(sender) = self.sender() {
            sender.send(event);
        }
    }

    /// The listener's state after each turn: whether it records, and how
    /// many dictations it submitted that have not finished.
    pub(crate) fn update(&mut self, recording: bool, pending: usize) {
        if recording && !self.recording {
            self.send(HudEvent::Started);
        }
        let submitted = pending > self.jobs.len();
        while pending > self.jobs.len() {
            self.next_job += 1;
            self.jobs.push_back(self.next_job);
            self.send(HudEvent::Submitted {
                job_id: self.next_job,
            });
        }
        if !recording && self.recording && !submitted {
            self.send(HudEvent::Cancelled);
        }
        if !recording && pending == 0 {
            clear_preview();
        }
        // Dictations that ended without `finished` (the listener stopping).
        while pending < self.jobs.len() {
            if let Some(job_id) = self.jobs.pop_front() {
                self.send(HudEvent::JobCancelled { job_id });
            }
        }
        self.recording = recording;
    }

    /// The oldest submitted dictation finished: pasted, or failed.
    pub(crate) fn finished(&mut self, pasted: bool) {
        if let Some(job_id) = self.jobs.pop_front() {
            self.send(if pasted {
                HudEvent::JobCompleted { job_id }
            } else {
                HudEvent::JobFailed { job_id }
            });
        }
    }

    /// Captured audio while recording, for the level the HUD shows.
    pub(crate) fn meter(&self, samples: &[f32]) {
        if let Some(sender) = self.sender() {
            sender.meter(samples);
        }
    }
}

impl Drop for LinuxIndicator {
    fn drop(&mut self) {
        self.send(HudEvent::Reset);
        clear_preview();
    }
}

/// Adds the HUD overlay to the window's app.
pub(crate) fn install<S: 'static>(app: &mut rune::App<S>) {
    let sender = crate::rune_hud::install(app);
    sender.set_position(*POSITION.lock().unwrap_or_else(|error| error.into_inner()));
    let _ = HUD.set(sender);
}

/// Why the HUD can't show, once Rune has tried.
pub(crate) fn limitation() -> Option<String> {
    if crate::linux_gnome::is_gnome() {
        return None;
    }
    (HUD.get()?.availability() == Availability::Unavailable).then(|| {
        "Recording HUD unavailable: this compositor does not support Wayland layer-shell. Dictation still works.".into()
    })
}

#[derive(Default)]
struct LiveTranscript {
    generation: u64,
    text: String,
    publisher: Option<crate::live_text::Publisher>,
}
static LIVE_TRANSCRIPT: Mutex<LiveTranscript> = Mutex::new(LiveTranscript {
    generation: 0,
    text: String::new(),
    publisher: None,
});

pub(crate) fn live_transcript() -> String {
    LIVE_TRANSCRIPT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .text
        .clone()
}

#[derive(Clone, Copy)]
pub(crate) struct Preview(u64);
impl Preview {
    pub(crate) fn acquire() -> Self {
        let mut preview = LIVE_TRANSCRIPT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        preview.generation = preview.generation.wrapping_add(1);
        preview.text.clear();
        preview.publisher = Some(crate::live_text::Publisher::new());
        Self(preview.generation)
    }
    pub(crate) fn update(self, text: &str) {
        let mut preview = LIVE_TRANSCRIPT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if preview.generation != self.0 {
            return;
        }
        // Keep the most recent 120 characters, matching the GNOME preview.
        let start = text
            .char_indices()
            .rev()
            .nth(119)
            .map_or(0, |(offset, _)| offset);
        preview.text = text[start..]
            .chars()
            .map(|character| {
                if character.is_whitespace() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        if let Some(publisher) = &preview.publisher {
            publisher.show(&crate::transcription_engine::Partial {
                stable: preview.text.clone(),
                tentative: String::new(),
            });
        }
    }
}

fn clear_preview() {
    let mut preview = LIVE_TRANSCRIPT
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if preview.publisher.take().is_some() {
        preview.generation = preview.generation.wrapping_add(1);
        preview.text.clear();
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn a_new_capture_fences_older_jobs_and_bounds_unicode_previews() {
        let changes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let observed = changes.clone();
        crate::live_text::watch(move |partial| {
            observed
                .lock()
                .unwrap()
                .push(partial.map(|partial| partial.stable.clone()));
        });
        let older = Preview::acquire();
        older.update("old");
        let newer = Preview::acquire();
        newer.update(&"界".repeat(150));
        older.update("stale");
        assert_eq!(live_transcript(), "界".repeat(120));
        assert_eq!(
            changes.lock().unwrap().last(),
            Some(&Some("界".repeat(120)))
        );
        clear_preview();
        newer.update("late final");
        assert!(live_transcript().is_empty());
        assert_eq!(changes.lock().unwrap().last(), Some(&None));
    }
}
