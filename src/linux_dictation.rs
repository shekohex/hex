use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::eyre;

use crate::audio::{AudioInput, AudioInputEvent, CaptureInstant};
use crate::dictation::{DictationCapture, Finish};
use crate::events::{DictationPhase, EventLog, TranscriptPhase, VoiceEvent, VoiceState, now_ms};
use crate::feedback::{self, Tone};
use crate::linux_desktop::LinuxIndicator;
use crate::linux_input::{HotkeyEvent, HotkeyStatus, LinuxHotkeyMonitor};
use crate::linux_paste::LinuxPaster;
use crate::linux_transcriber::LinuxTranscriber;

const UPDATE_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Default)]
struct CaptureWork {
    context: Option<crate::linux_context::ContextCapture>,
    realtime: Option<crate::cloud::RealtimeSession>,
    control: Arc<JobControl>,
}

struct Job {
    samples: Vec<f32>,
    audio_ms: u64,
    context: Option<crate::linux_context::ContextCapture>,
    realtime: Option<crate::cloud::RealtimeSession>,
    control: Arc<JobControl>,
}

#[derive(Default)]
struct JobControl {
    cancelled: Arc<AtomicBool>,
    // 0 pending, 1 cancelled, 2 insertion committed, 3 finished without insertion.
    state: AtomicU8,
}

impl JobControl {
    fn cancel(&self) -> bool {
        if self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancelled.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn commit(&self) -> Result<()> {
        self.state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| eyre!("Dictation cancelled before insertion"))
    }

    fn finish(&self) {
        let _ = self
            .state
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire);
    }
}

struct OutputWorker {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    unfinished: std::collections::VecDeque<Arc<JobControl>>,
}

impl Drop for OutputWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for job in &self.unfinished {
            job.cancel();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Runs hotkey dictation until `shutdown`. A prepared transcriber skips the
/// model load; otherwise the supplied transcription selection is loaded here.
pub fn run(
    event_path: &Path,
    device: Option<&str>,
    shutdown: &AtomicBool,
    settings: crate::linux_settings::LinuxSettings,
    transcriber: Option<LinuxTranscriber>,
    hotkey_status: HotkeyStatus,
) -> Result<()> {
    let transcriber = if settings.cloud_transcription.is_none() {
        Some(transcriber.map_or_else(|| LinuxTranscriber::load(&settings.transcription), Ok)?)
    } else {
        None
    };
    // Drop audio/hotkeys and close the job queue before joining slow output on
    // every exit, including failures during startup or recording.
    let mut output = OutputWorker {
        stop: Arc::new(AtomicBool::new(false)),
        worker: None,
        unfinished: Default::default(),
    };
    let input = match device {
        Some(name) => AudioInput::open_matching(name)?,
        None => AudioInput::open(&[])?,
    };
    let hotkey_label = settings.dictation_hotkey.label();
    let hotkey = LinuxHotkeyMonitor::start(
        settings.dictation_hotkey,
        settings.double_tap_lock,
        hotkey_status.clone(),
    )?;
    let wayland_modifiers = hotkey.wayland_modifiers();
    let indicator = LinuxIndicator::new();
    let (jobs, job_receiver) = mpsc::sync_channel::<Job>(2);
    let (result_sender, results) = mpsc::channel();
    let worker_stop = output.stop.clone();
    let cloud = settings.cloud_transcription.clone();
    let processing = settings.processing.clone();
    let observations = EventLog::create(event_path)?;
    let partial_events = observations.clone();
    output.worker = Some(thread::spawn(move || {
        let mut transcriber = transcriber;
        let mut paster = LinuxPaster::new(
            worker_stop.clone(),
            settings.paste_with_shift,
            wayland_modifiers,
        );
        while let Ok(job) = job_receiver.recv() {
            if worker_stop.load(Ordering::Acquire) {
                break;
            }
            let started = Instant::now();
            let result = (|| -> Result<(String, Option<crate::events::DictationProcessing>)> {
                if job.control.cancelled.load(Ordering::Acquire) {
                    return Err(eyre!("Dictation cancelled"));
                }
                let text = if let Some(cloud) = &settings.cloud_transcription {
                    if let Some(realtime) = job.realtime {
                        realtime.finish(
                            &job.samples,
                            &job.control.cancelled,
                            Duration::from_secs(cloud.deadline_seconds),
                        )?
                    } else if cloud.mode == crate::cloud::TranscriptionMode::Realtime {
                        crate::cloud::RealtimeSession::start(
                            cloud.clone(),
                            job.control.cancelled.clone(),
                            |_| {},
                        )
                        .finish(
                            &job.samples,
                            &job.control.cancelled,
                            Duration::from_secs(cloud.deadline_seconds),
                        )?
                    } else {
                        crate::cloud::transcribe(
                            cloud,
                            &job.samples,
                            &job.control.cancelled,
                            |text| {
                                let _ = partial_events.emit(&VoiceEvent::Transcript {
                                    timestamp_ms: now_ms(),
                                    phase: TranscriptPhase::Updated,
                                    latency_ms: 0,
                                    text: text.to_owned(),
                                });
                            },
                        )?
                    }
                } else {
                    let mut samples = job.samples;
                    crate::dictation::pad_for_parakeet(&mut samples);
                    transcriber
                        .as_mut()
                        .expect("local selection has a transcriber")
                        .transcribe(&samples)?
                };
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Err(eyre!("transcription was empty"));
                }
                let context = job
                    .context
                    .map(crate::linux_context::ContextCapture::take)
                    .unwrap_or_default();
                let (text, observation) =
                    settings
                        .processing
                        .process(&text, &context, &job.control.cancelled);
                if worker_stop.load(Ordering::Acquire) {
                    job.control.cancel();
                }
                paster
                    .as_mut()
                    .map_err(|error| eyre!("{error:#}"))?
                    .paste_with_commit(&text, &job.control.cancelled, || job.control.commit())?;
                Ok((text, observation))
            })()
            .map_err(|error| format!("{error:#}"));
            job.control.finish();
            tracing::info!(
                audio_ms = job.audio_ms,
                elapsed_ms = started.elapsed().as_millis(),
                "completed Linux dictation job"
            );
            if result_sender.send(result).is_err() {
                break;
            }
        }
    }));

    let mut events = observations;
    let mut capture = DictationCapture::new(input.sample_rate);
    let mut captured_through = CaptureInstant::ZERO;
    let mut pending = 0_usize;
    let mut capture_work = CaptureWork::default();
    events.emit(&VoiceEvent::SessionStarted {
        timestamp_ms: now_ms(),
    })?;
    emit_state(&mut events, VoiceState::Listening, &input.device_name)?;
    println!(
        "HEX dictation is ready on {}. Hold {} to dictate; Ctrl-C stops.",
        input.device_name, hotkey_label
    );

    while !shutdown.load(Ordering::Relaxed) {
        if let Ok(error) = hotkey.errors.try_recv() {
            return Err(eyre!("Linux hotkey monitor stopped: {error}"));
        }
        while let Ok(action) = hotkey.events.try_recv() {
            match action {
                HotkeyEvent::Start if !capture.is_recording() => {
                    capture_work = CaptureWork::default();
                    start_capture(&mut capture, captured_through, feedback::play);
                    events.dictation(DictationPhase::Started, "")?;
                    emit_state(&mut events, VoiceState::Dictating, &input.device_name)?;
                }
                HotkeyEvent::Finish if capture.is_recording() => {
                    let previous_pending = pending;
                    let control = capture_work.control.clone();
                    submit_capture(
                        &mut capture,
                        captured_through,
                        &jobs,
                        &mut events,
                        &mut pending,
                        std::mem::take(&mut capture_work),
                    )?;
                    if pending > previous_pending {
                        output.unfinished.push_back(control);
                    }
                    hotkey_status.set_output_pending(pending > 0);
                    emit_state(
                        &mut events,
                        active_state(capture.is_recording(), pending),
                        &input.device_name,
                    )?;
                }
                HotkeyEvent::Cancel if capture.is_recording() => {
                    capture.cancel();
                    capture_work.control.cancel();
                    capture_work = CaptureWork::default();
                    feedback::play(Tone::Cancel);
                    events.dictation(DictationPhase::Cancelled, "")?;
                    emit_state(
                        &mut events,
                        active_state(capture.is_recording(), pending),
                        &input.device_name,
                    )?;
                }
                HotkeyEvent::Cancel if output.unfinished.iter().rev().any(|job| job.cancel()) => {
                    feedback::play(Tone::Cancel);
                }
                _ => {}
            }
        }
        while let Ok(result) = results.try_recv() {
            pending = pending.saturating_sub(1);
            hotkey_status.set_output_pending(pending > 0);
            let cancelled = output
                .unfinished
                .pop_front()
                .is_some_and(|job| job.cancelled.load(Ordering::Acquire));
            if cancelled {
                events.dictation(DictationPhase::Cancelled, "")?;
            } else {
                match result {
                    Ok((text, processing)) => {
                        events.emit(&VoiceEvent::Transcript {
                            timestamp_ms: now_ms(),
                            phase: TranscriptPhase::Completed,
                            latency_ms: 0,
                            text: text.clone(),
                        })?;
                        events.processed_dictation(DictationPhase::Pasted, text, processing)?;
                    }
                    Err(error) => events.dictation(DictationPhase::Failed(error), "")?,
                }
            }
            emit_state(
                &mut events,
                active_state(capture.is_recording(), pending),
                &input.device_name,
            )?;
        }

        indicator.update(capture.is_recording(), pending);
        let chunk = match input.recv_timeout(UPDATE_INTERVAL) {
            AudioInputEvent::Chunk {
                samples,
                captured_through: chunk_captured_through,
            } => {
                captured_through = chunk_captured_through;
                samples
            }
            AudioInputEvent::Timeout => continue,
            AudioInputEvent::StreamFailed(error) => {
                return Err(eyre!("microphone stream stopped: {error}"));
            }
        };
        capture.ingest(&chunk, captured_through);
        if capture.become_intentional(captured_through) {
            capture_work.context = Some(crate::linux_context::ContextCapture::start(
                processing.clone(),
            ));
            if let Some(cloud) = cloud
                .as_ref()
                .filter(|cloud| cloud.mode == crate::cloud::TranscriptionMode::Realtime)
            {
                // Realtime is a disposable projection. Capture owns all PCM even if admission fails.
                capture_work.realtime = Some(crate::cloud::RealtimeSession::start(
                    cloud.clone(),
                    capture_work.control.cancelled.clone(),
                    |_| {},
                ));
            }
        }
        if let Some(realtime) = &mut capture_work.realtime {
            realtime.offer(capture.live_samples_from(realtime.offered_samples()));
        }
    }

    indicator.update(false, pending);
    emit_state(&mut events, VoiceState::Stopping, &input.device_name)?;
    output.stop.store(true, Ordering::Release);
    capture_work.control.cancel();
    hotkey_status.set_output_pending(false);
    for job in &output.unfinished {
        job.cancel();
    }
    drop(hotkey);
    drop(input);
    drop(jobs);
    if output
        .worker
        .take()
        .expect("dictation output worker exists")
        .join()
        .is_err()
    {
        return Err(eyre!("Linux dictation worker panicked"));
    }
    println!("Stopped.");
    Ok(())
}

pub(crate) fn initialize_feedback(volume: f32) {
    feedback::set_volume(volume);
    if let Err(error) = feedback::preload() {
        tracing::warn!(%error, "recording sounds are unavailable; continuing without feedback");
    }
}

fn start_capture(
    capture: &mut DictationCapture,
    started_at: CaptureInstant,
    play: impl FnOnce(Tone),
) {
    capture.start(started_at);
    play(Tone::DictationStart);
}

fn submit_capture(
    capture: &mut DictationCapture,
    ended_at: CaptureInstant,
    jobs: &SyncSender<Job>,
    events: &mut EventLog,
    pending: &mut usize,
    work: CaptureWork,
) -> Result<()> {
    let CaptureWork {
        context,
        realtime,
        control,
    } = work;
    match capture.finish(ended_at) {
        Finish::Discard => events.dictation(DictationPhase::Discarded, "")?,
        Finish::Transcribe(clip) if clip.is_near_silent() => {
            events.dictation(DictationPhase::Discarded, "")?
        }
        Finish::Transcribe(clip) => {
            feedback::play(Tone::DictationStop);
            let audio_ms = clip.duration_ms();
            let job = Job {
                samples: clip.into_transcription_samples(),
                audio_ms,
                context,
                realtime,
                control,
            };
            match jobs.try_send(job) {
                Ok(()) => {
                    *pending += 1;
                    events.dictation(DictationPhase::Transcribing, "")?;
                }
                Err(TrySendError::Full(_)) => events
                    .dictation(DictationPhase::Failed("dictation queue is full".into()), "")?,
                Err(TrySendError::Disconnected(_)) => {
                    return Err(eyre!("dictation worker is unavailable"));
                }
            }
        }
    }
    Ok(())
}

fn active_state(recording: bool, pending: usize) -> VoiceState {
    if recording {
        VoiceState::Dictating
    } else if pending > 0 {
        VoiceState::Transcribing
    } else {
        VoiceState::Listening
    }
}

fn emit_state(events: &mut EventLog, state: VoiceState, device: &str) -> Result<()> {
    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state,
        device: device.into(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_wins_until_insertion_commits_and_never_cancels_committed_output() {
        let cancelled = JobControl::default();
        assert!(cancelled.cancel());
        assert!(cancelled.commit().is_err());
        assert!(cancelled.cancelled.load(Ordering::Acquire));
        let committed = JobControl::default();
        committed.commit().unwrap();
        assert!(!committed.cancel());
        assert!(!committed.cancelled.load(Ordering::Acquire));
        let finished = JobControl::default();
        finished.finish();
        assert!(!finished.cancel());
    }

    #[test]
    fn start_sound_precedes_audio_and_does_not_change_short_tap_discard() {
        let mut capture = DictationCapture::new(16_000);
        let started_at = CaptureInstant::from_nanos(60_000_000_000);
        let mut sounds = Vec::new();

        start_capture(&mut capture, started_at, |tone| sounds.push(tone));

        assert!(capture.is_recording());
        assert_eq!(sounds, [Tone::DictationStart]);
        assert!(matches!(
            capture.finish(started_at + Duration::from_millis(100)),
            Finish::Discard
        ));
    }

    #[test]
    fn cancelled_capture_keeps_pending_output_visible() {
        assert_eq!(active_state(true, 2), VoiceState::Dictating);
        assert_eq!(active_state(false, 2), VoiceState::Transcribing);
        assert_eq!(active_state(false, 1), VoiceState::Transcribing);
        assert_eq!(active_state(false, 0), VoiceState::Listening);
        assert_eq!(active_state(true, 0), VoiceState::Dictating);
    }

    #[test]
    fn output_guard_cancels_and_joins_even_on_listener_error() {
        let stopped = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let result: Result<()> = {
            let mut output = OutputWorker {
                unfinished: Default::default(),
                stop: stopped.clone(),
                worker: None,
            };
            let (_jobs, receiver) = mpsc::channel::<()>();
            let stop = stopped.clone();
            let completed = finished.clone();
            output.worker = Some(thread::spawn(move || {
                // The sender must close before the guard joins this worker.
                assert!(receiver.recv().is_err());
                while !stop.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                completed.store(true, Ordering::Release);
            }));
            Err(eyre!("listener failure"))
        };
        assert!(result.is_err());
        assert!(stopped.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire));
    }
}
