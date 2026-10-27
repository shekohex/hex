//! One RAM-only retry record. Audio never crosses IPC or reaches disk.
use std::sync::{Arc, Mutex, mpsc};

use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::{
    events::DictationTimings,
    linux_context::{CaptureContext, ContextCapture},
};

const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Snapshot {
    pub id: u64,
    pub audio_available: bool,
    pub raw_available: bool,
    pub text: String,
    pub busy: bool,
    pub error: Option<String>,
    pub timings: Option<DictationTimings>,
}

pub struct Recording {
    pub samples: Arc<Vec<f32>>,
    pub raw: Option<String>,
    context: Arc<CaptureContext>,
    pending_context: Option<ContextCapture>,
}

impl Recording {
    /// Resolve only on the output worker, never on capture or IPC threads.
    pub fn context(&self) -> Arc<CaptureContext> {
        self.pending_context
            .as_ref()
            .map(ContextCapture::take)
            .unwrap_or_else(|| self.context.clone())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Retry {
    pub id: u64,
    pub cleanup_only: bool,
}

struct Retained {
    snapshot: Snapshot,
    audio: Option<Arc<Vec<f32>>>,
    raw: Option<String>,
    context: Arc<CaptureContext>,
    pending_context: Option<ContextCapture>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    retained: Option<Retained>,
    requests: Option<mpsc::SyncSender<Retry>>,
}

#[derive(Default)]
pub struct LastDictation(Mutex<State>);

pub struct Connection<'a> {
    owner: &'a LastDictation,
    pub requests: mpsc::Receiver<Retry>,
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.0.lock().unwrap();
        state.requests = None;
        if let Some(retained) = &mut state.retained {
            retained.snapshot.busy = false;
        }
    }
}

impl LastDictation {
    pub fn connect(&self) -> Connection<'_> {
        let (sender, requests) = mpsc::sync_channel(1);
        self.0.lock().unwrap().requests = Some(sender);
        Connection {
            owner: self,
            requests,
        }
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.0
            .lock()
            .unwrap()
            .retained
            .as_ref()
            .map(|record| record.snapshot.clone())
    }

    pub fn retain(&self, samples: Arc<Vec<f32>>) -> u64 {
        let mut state = self.0.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        let audio = (samples.len() <= MAX_AUDIO_BYTES / size_of::<f32>()).then_some(samples);
        state.retained = Some(Retained {
            snapshot: Snapshot {
                id,
                audio_available: audio.is_some(),
                raw_available: false,
                text: String::new(),
                busy: true,
                error: None,
                timings: None,
            },
            audio,
            raw: None,
            context: Arc::new(CaptureContext::default()),
            pending_context: None,
        });
        id
    }

    pub fn request(&self, retry: Retry) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let retained = state
            .retained
            .as_ref()
            .filter(|record| record.snapshot.id == retry.id)
            .ok_or_else(|| eyre!("Last recording changed; retry the current result"))?;
        if retained.snapshot.busy {
            return Err(eyre!("Last dictation is still processing"));
        }
        if retry.cleanup_only && retained.raw.is_none() {
            return Err(eyre!(
                "No raw transcript available; retry transcription first"
            ));
        }
        if !retry.cleanup_only && retained.audio.is_none() {
            return Err(eyre!(
                "Last audio unavailable; RAM retry limit is 32 MiB (about 8 minutes)"
            ));
        }
        state
            .requests
            .as_ref()
            .ok_or_else(|| eyre!("Start the listener before retrying"))?
            .try_send(retry)
            .map_err(|_| eyre!("Retry queue unavailable or busy"))?;
        state.retained.as_mut().unwrap().snapshot.busy = true;
        Ok(())
    }

    pub fn recording(&self, retry: Retry) -> Result<Recording> {
        let state = self.0.lock().unwrap();
        let retained = state
            .retained
            .as_ref()
            .filter(|record| record.snapshot.id == retry.id)
            .ok_or_else(|| eyre!("Last recording changed"))?;
        Ok(Recording {
            samples: retained.audio.clone().unwrap_or_default(),
            raw: retry.cleanup_only.then(|| retained.raw.clone()).flatten(),
            context: retained.context.clone(),
            pending_context: retained.pending_context.clone(),
        })
    }

    pub fn raw(&self, id: u64, text: &str, context: Arc<CaptureContext>) {
        let mut state = self.0.lock().unwrap();
        if let Some(record) = state
            .retained
            .as_mut()
            .filter(|record| record.snapshot.id == id)
        {
            if text.len() <= MAX_TEXT_BYTES {
                record.raw = Some(text.to_owned());
                record.snapshot.raw_available = true;
            }
            record.context = context;
            record.pending_context = None;
        }
    }

    pub fn pending_context(&self, id: u64, context: ContextCapture) {
        let mut state = self.0.lock().unwrap();
        if let Some(record) = state
            .retained
            .as_mut()
            .filter(|record| record.snapshot.id == id)
        {
            record.pending_context = Some(context);
        }
    }

    pub fn context(&self, id: u64, context: Arc<CaptureContext>) {
        let mut state = self.0.lock().unwrap();
        if let Some(record) = state
            .retained
            .as_mut()
            .filter(|record| record.snapshot.id == id)
        {
            record.context = context;
            record.pending_context = None;
        }
    }

    pub fn finish(
        &self,
        id: u64,
        text: Option<&str>,
        error: Option<String>,
        timings: Option<DictationTimings>,
    ) {
        let mut state = self.0.lock().unwrap();
        if let Some(record) = state
            .retained
            .as_mut()
            .filter(|record| record.snapshot.id == id)
        {
            if let Some(text) = text.filter(|text| text.len() <= MAX_TEXT_BYTES) {
                record.snapshot.text = text.to_owned();
            }
            record.snapshot.busy = false;
            record.snapshot.error = error;
            record.snapshot.timings = timings;
        }
    }

    pub fn clear(&self, id: u64) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state
            .retained
            .as_ref()
            .is_none_or(|record| record.snapshot.id != id)
        {
            return Err(eyre!("Last recording changed; clear the current result"));
        }
        if state
            .retained
            .as_ref()
            .is_some_and(|record| record.snapshot.busy)
        {
            return Err(eyre!(
                "Wait for the current dictation to finish before clearing"
            ));
        }
        state.retained = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_transcription_retains_audio_and_context_for_retry() {
        let last = LastDictation::default();
        let connection = last.connect();
        let audio = Arc::new(vec![0.25; 16_000]);
        let id = last.retain(audio.clone());
        let context = Arc::new(CaptureContext {
            active_field: Some("original field".into()),
            ..Default::default()
        });
        last.context(id, context.clone());
        last.finish(id, None, Some("Realtime connection failed".into()), None);
        let retry = Retry {
            id,
            cleanup_only: false,
        };
        last.request(retry).unwrap();
        assert_eq!(connection.requests.try_recv().unwrap().id, id);
        let recording = last.recording(retry).unwrap();
        assert!(Arc::ptr_eq(&recording.samples, &audio));
        assert!(Arc::ptr_eq(&recording.context, &context));
        assert!(recording.raw.is_none());
        assert!(last.snapshot().unwrap().busy);
        assert!(last.request(retry).is_err());
        drop(connection);
        assert!(!last.snapshot().unwrap().busy);
        assert!(last.request(retry).is_err());
    }

    #[test]
    fn unprocessed_recording_keeps_original_pending_context_for_retry() {
        let last = LastDictation::default();
        let id = last.retain(Arc::new(vec![0.25; 1600]));
        // Disabled context performs no desktop inspection, but retains the same async owner.
        let context = ContextCapture::start(Default::default());
        last.pending_context(id, context.clone());
        last.finish(id, None, Some("dictation queue is full".into()), None);
        let recording = last
            .recording(Retry {
                id,
                cleanup_only: false,
            })
            .unwrap();
        assert!(Arc::ptr_eq(&recording.context(), &context.take()));
    }

    #[test]
    fn cleanup_retries_raw_not_previous_enhancement_and_clear_releases_audio() {
        let last = LastDictation::default();
        let connection = last.connect();
        let audio = Arc::new(vec![0.25; 1600]);
        let id = last.retain(audio.clone());
        last.raw(id, "raw speech", Arc::default());
        last.finish(
            id,
            Some("enhanced speech"),
            None,
            Some(DictationTimings::default()),
        );
        let retry = Retry {
            id,
            cleanup_only: true,
        };
        last.request(retry).unwrap();
        connection.requests.try_recv().unwrap();
        assert_eq!(
            last.recording(retry).unwrap().raw.as_deref(),
            Some("raw speech")
        );
        assert!(last.clear(id).is_err());
        last.finish(id, None, Some("retry failed".into()), None);
        assert_eq!(last.snapshot().unwrap().text, "enhanced speech");
        last.clear(id).unwrap();
        assert!(last.snapshot().is_none());
        assert_eq!(Arc::strong_count(&audio), 1);
    }

    #[test]
    fn stale_completion_retry_and_clear_never_replace_new_recording() {
        let last = LastDictation::default();
        let _connection = last.connect();
        let old = last.retain(Arc::new(vec![0.25]));
        let current = last.retain(Arc::new(vec![0.5]));
        last.raw(old, "old", Arc::default());
        last.finish(old, Some("old"), None, None);
        assert!(
            last.request(Retry {
                id: old,
                cleanup_only: false
            })
            .is_err()
        );
        assert!(last.clear(old).is_err());
        let snapshot = last.snapshot().unwrap();
        assert_eq!(snapshot.id, current);
        assert!(!snapshot.raw_available);
        assert!(snapshot.text.is_empty());
        assert!(snapshot.busy);
    }

    #[test]
    fn oversized_audio_does_not_disable_cleanup_retry() {
        let last = LastDictation::default();
        let connection = last.connect();
        let id = last.retain(Arc::new(vec![0.0; MAX_AUDIO_BYTES / size_of::<f32>() + 1]));
        last.raw(id, "raw speech", Arc::default());
        last.finish(id, Some("speech"), None, None);
        assert!(!last.snapshot().unwrap().audio_available);
        assert!(
            last.request(Retry {
                id,
                cleanup_only: false
            })
            .is_err()
        );
        let retry = Retry {
            id,
            cleanup_only: true,
        };
        last.request(retry).unwrap();
        connection.requests.try_recv().unwrap();
        let recording = last.recording(retry).unwrap();
        assert!(recording.samples.is_empty());
        assert_eq!(recording.raw.as_deref(), Some("raw speech"));
    }
}
