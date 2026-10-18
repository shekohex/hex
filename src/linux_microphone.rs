//! Warm input by default; optional generation-fenced, nonblocking cold opens.
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use color_eyre::Result;

use crate::audio::{AudioInput, AudioInputEvent};

pub struct Microphone {
    input: Option<AudioInput>,
    opening: Option<Receiver<Result<AudioInput, String>>>,
    opening_generation: u64,
    generation: u64,
    requested: bool,
    release_while_idle: bool,
    device: Option<String>,
    last_audio_at: Instant,
}

impl Microphone {
    pub fn new(device: Option<&str>, release_while_idle: bool) -> Result<Self> {
        Ok(Self {
            input: if release_while_idle {
                None
            } else {
                Some(open(device)?)
            },
            opening: None,
            opening_generation: 0,
            generation: 0,
            requested: false,
            release_while_idle,
            device: device.map(str::to_owned),
            last_audio_at: Instant::now(),
        })
    }

    pub fn start(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.requested = true;
        self.last_audio_at = Instant::now();
    }

    pub fn idle(&mut self) {
        self.requested = false;
        if self.release_while_idle {
            self.input = None;
        }
        // Retain a pending receiver until completion: repeated short taps must
        // never create concurrent native opens or accept an old generation.
    }

    pub fn poll(&mut self) -> Result<bool, String> {
        if let Some(receiver) = &self.opening {
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("Microphone open worker stopped".into()))
                }
            };
            if let Some(result) = result {
                self.opening = None;
                if self.requested && self.opening_generation == self.generation {
                    self.input = Some(result?);
                    self.last_audio_at = Instant::now();
                    return Ok(true);
                }
            }
        }
        if self.requested && self.input.is_none() && self.opening.is_none() {
            let device = self.device.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = sender.send(open(device.as_deref()).map_err(|error| error.to_string()));
            });
            self.opening = Some(receiver);
            self.opening_generation = self.generation;
        }
        Ok(false)
    }

    pub fn input(&self) -> Option<&AudioInput> {
        self.input.as_ref()
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> AudioInputEvent {
        match &self.input {
            Some(input) => match input.recv_timeout(timeout) {
                event @ AudioInputEvent::Chunk { .. } => {
                    self.last_audio_at = Instant::now();
                    event
                }
                AudioInputEvent::Timeout
                    if self.requested && self.last_audio_at.elapsed() >= Duration::from_secs(5) =>
                {
                    AudioInputEvent::StreamFailed(
                        "Microphone stopped delivering audio. Reconnect the microphone; for Bluetooth, disable Release microphone while idle to avoid profile switching.".into(),
                    )
                }
                event => event,
            },
            None => {
                std::thread::sleep(timeout);
                AudioInputEvent::Timeout
            }
        }
    }
}

fn open(device: Option<&str>) -> Result<AudioInput> {
    match device {
        Some(query) => AudioInput::open_matching(query),
        None => AudioInput::open(&[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens the native microphone; discards all audio"]
    fn native_warm_microphone_preserves_repeated_captures() {
        native_repeated_captures(false);
    }

    #[test]
    #[ignore = "opens the native microphone; switches Bluetooth profiles; discards all audio"]
    fn native_cold_microphone_preserves_repeated_captures() {
        native_repeated_captures(true);
    }

    fn native_repeated_captures(release_while_idle: bool) {
        use crate::audio::CaptureInstant;
        use crate::dictation::{DictationCapture, Finish};

        crate::linux_dictation::initialize_feedback(0.5);
        let mut microphone = Microphone::new(None, release_while_idle).unwrap();
        for attempt in 1..=3 {
            microphone.start();
            let started = Instant::now();
            let mut capture = None;
            let mut end = CaptureInstant::ZERO;
            while started.elapsed() < Duration::from_secs(12) {
                microphone.poll().unwrap();
                match microphone.recv_timeout(Duration::from_millis(20)) {
                    AudioInputEvent::Chunk {
                        samples,
                        captured_through,
                    } => {
                        let recording = capture.get_or_insert_with(|| {
                            let sample_rate = microphone.input().unwrap().sample_rate;
                            let mut recording = DictationCapture::new(sample_rate);
                            recording
                                .start(captured_through.sub_frames(samples.len(), sample_rate));
                            crate::feedback::play(crate::feedback::Tone::DictationStart);
                            recording
                        });
                        recording.ingest(&samples, captured_through);
                        recording.become_intentional_after_hold(started.elapsed());
                        end = captured_through;
                    }
                    AudioInputEvent::Timeout => {}
                    AudioInputEvent::StreamFailed(error) => panic!("{error}"),
                }
            }
            microphone.idle();
            let Finish::Transcribe(clip) = capture.expect("no microphone audio").finish(end) else {
                panic!("no retained capture");
            };
            eprintln!(
                "attempt={attempt} release_while_idle={release_while_idle} audio_ms={}",
                clip.duration_ms()
            );
            assert!(clip.duration_ms() > 5_000, "microphone lost audio");
            assert!(
                clip.into_transcription_samples()
                    .iter()
                    .any(|sample| *sample != 0.0),
                "microphone delivered only zeros"
            );
            let idle_started = Instant::now();
            while idle_started.elapsed() < Duration::from_secs(5) {
                microphone.recv_timeout(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn stalled_capture_reports_failure_instead_of_silent_timeouts() {
        let mut microphone = Microphone::new(None, true).unwrap();
        let (input, _sender) = AudioInput::channel_for_test();
        microphone.input = Some(input);
        microphone.start();
        microphone.last_audio_at = Instant::now() - Duration::from_secs(6);
        assert!(matches!(
            microphone.recv_timeout(Duration::ZERO),
            AudioInputEvent::StreamFailed(error) if error.contains("stopped delivering audio")
        ));
    }

    #[test]
    fn delivered_audio_resets_stall_deadline() {
        let mut microphone = Microphone::new(None, true).unwrap();
        let (input, sender) = AudioInput::channel_for_test();
        microphone.input = Some(input);
        microphone.start();
        microphone.last_audio_at = Instant::now() - Duration::from_secs(6);
        sender
            .send((vec![0.0; 480], crate::audio::CaptureInstant::ZERO))
            .unwrap();
        assert!(matches!(
            microphone.recv_timeout(Duration::ZERO),
            AudioInputEvent::Chunk { .. }
        ));
        assert!(matches!(
            microphone.recv_timeout(Duration::ZERO),
            AudioInputEvent::Timeout
        ));
    }

    #[test]
    fn idle_releases_microphone_without_waiting_for_output() {
        let mut microphone = Microphone::new(None, true).unwrap();
        assert!(microphone.input().is_none());
        let (input, sender) = AudioInput::channel_for_test();
        microphone.input = Some(input);
        microphone.start();
        microphone.idle();
        assert!(microphone.input().is_none());
        assert!(
            sender
                .send((vec![0.0], crate::audio::CaptureInstant::ZERO))
                .is_err()
        );
    }

    #[test]
    fn cancelled_cold_opens_are_bounded_and_never_reused_by_a_new_press() {
        let mut microphone = Microphone::new(None, true).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        microphone.start();
        microphone.opening_generation = microphone.generation;
        microphone.opening = Some(receiver);
        microphone.idle();
        microphone.start();
        assert!(!microphone.poll().unwrap());
        assert!(microphone.opening.is_some());
        microphone.idle();
        let (input, _) = AudioInput::channel_for_test();
        sender.send(Ok(input)).ok().unwrap();
        assert!(!microphone.poll().unwrap());
        assert!(microphone.input().is_none());
        assert!(microphone.opening.is_none());
    }
}
