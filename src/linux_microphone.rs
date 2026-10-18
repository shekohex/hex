//! Warm input by default; optional generation-fenced, nonblocking cold opens.
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

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
        })
    }

    pub fn start(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.requested = true;
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

    pub fn recv_timeout(&self, timeout: Duration) -> AudioInputEvent {
        match &self.input {
            Some(input) => input.recv_timeout(timeout),
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
