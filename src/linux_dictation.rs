use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::eyre;

use crate::app_settings::AppSettings;
use crate::audio::{CaptureInstant, RecoveringAudioInput, RecoveringAudioInputEvent};
use crate::dictation::{DictationCapture, Finish, POST_RELEASE_TAIL};
use crate::events::{
    DictationPhase, DictationTimings, EventLog, TranscriptPhase, VoiceEvent, VoiceState, now_ms,
};
use crate::feedback::{self, Tone};
use crate::linux_input::{
    CommandQueue, HotkeyEvent, HotkeyOptions, HotkeyStatus, LinuxHotkey, LinuxHotkeyMonitor,
};
use crate::linux_paste::LinuxPaster;
use crate::linux_rune_hud::LinuxIndicator;
use crate::local_transcriber::LocalTranscriber;
use crate::transcription_engine::{PendingTranscript, TranscriptionEngine};

const UPDATE_INTERVAL: Duration = Duration::from_millis(20);
/// How long past the tail a release waits for audio that never arrives.
const TAIL_STALL_GRACE: Duration = Duration::from_millis(100);

#[derive(Default)]
struct CaptureWork {
    context: Option<crate::linux_context::ContextCapture>,
    realtime: Option<crate::cloud::RealtimeSession>,
    control: Arc<JobControl>,
    preview: Option<crate::linux_rune_hud::Preview>,
}

struct Job {
    samples: Arc<Vec<f32>>,
    transcript: Option<PendingTranscript>,
    settings: AppSettings,
    processing: Option<crate::events::DictationProcessing>,
    retained_text: Option<String>,
    recording_id: u64,
    released_at: Instant,
    retry: Option<crate::linux_last_dictation::Recording>,
    audio_ms: u64,
    context: Option<crate::linux_context::ContextCapture>,
    realtime: Option<crate::cloud::RealtimeSession>,
    control: Arc<JobControl>,
    preview: Option<crate::linux_rune_hud::Preview>,
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

enum OutputJob {
    Dictation(Job),
    VoiceAction(Job),
    /// Pastes the newest completed dictation again.
    PasteLast,
}

#[derive(Clone, Copy, PartialEq)]
enum OutputKind {
    Dictation,
    VoiceAction,
    PasteLast,
}

struct CompletedOutput {
    kind: OutputKind,
    result: Result<String, String>,
    timings: DictationTimings,
    preview: bool,
    processing: Option<crate::events::DictationProcessing>,
    cancelled: bool,
}

/// A release whose capture is still collecting its post-release tail.
struct Finishing {
    ended_at: CaptureInstant,
    /// The recording is a Voice Action instruction.
    voice: bool,
    released: Instant,
    deadline: Instant,
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

/// What the host hands a running listener. Settings apply in place while no
/// dictation records, so changing one never stops listening.
pub(crate) enum ListenerUpdate {
    Settings(Box<AppSettings>),
    /// A model the host prepared, swapped in once no transcription is pending.
    Transcriber {
        selection: crate::transcription_models::TranscriptionSelection,
        transcriber: Box<LocalTranscriber>,
    },
    /// True while the window records a shortcut: global matching stops.
    SuspendShortcuts(bool),
}

/// The shortcuts a listener registers for `settings`.
fn shortcuts(settings: &AppSettings) -> (LinuxHotkey, HotkeyOptions) {
    let options = HotkeyOptions {
        double_tap: settings.double_tap_lock,
        // Double-tap-only needs the double-tap window and a non-modifier key.
        double_tap_only: settings.double_tap_lock
            && settings.double_tap_only
            && settings.dictation_hotkey.key.is_some(),
        toggle: settings.shortcut_mode == crate::dictation::ShortcutMode::Toggle,
        paste_last: settings.paste_last_hotkey.as_ref().map(LinuxHotkey::from),
        voice_action: settings
            .voice_action
            .enabled
            .then(|| LinuxHotkey::from(&settings.edit_hotkey)),
    };
    (LinuxHotkey::from(&settings.dictation_hotkey), options)
}

/// Opens the microphone `settings` select, or keeps it released until a
/// dictation starts.
fn microphone_input(settings: &AppSettings, device: Option<&str>) -> RecoveringAudioInput {
    let microphone = device.or(settings.microphone.as_deref());
    if settings.release_microphone_while_idle {
        RecoveringAudioInput::closed(None, 0, microphone)
    } else {
        RecoveringAudioInput::open(None, 0, microphone)
    }
}

/// What happens to a transcript before and after it is pasted: the modes
/// that process it and the history that keeps it. The host shares it
/// with the listener so saved settings apply without a restart.
#[derive(Clone)]
pub(crate) struct DictationOutput {
    pub(crate) last: Arc<crate::linux_last_dictation::LastDictation>,
    pub(crate) history: Option<crate::history::History>,
    pub(crate) modes: Arc<RwLock<crate::dictation_modes::ModeRuntime>>,
    pub(crate) transformations: Arc<crate::personal_commands::TransformationClient>,
}

impl DictationOutput {
    pub(crate) fn from_settings(
        settings: &crate::app_settings::AppSettings,
        history: Option<crate::history::History>,
    ) -> Self {
        Self {
            last: Default::default(),
            history,
            modes: Arc::new(RwLock::new(
                crate::dictation_modes::ModeRuntime::from_settings(settings),
            )),
            transformations: Arc::new(crate::personal_commands::TransformationClient::default()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &self,
        raw_text: String,
        paster: &mut Result<LinuxPaster>,
        job: &mut Job,
        context: &crate::linux_context::CaptureContext,
        inference_ms: u64,
        started: Instant,
        timings: &mut DictationTimings,
    ) -> Result<String> {
        let snapshot = crate::context::ContextSnapshot {
            application: context.application.clone(),
            ..Default::default()
        };
        let processing_started = Instant::now();
        let processed = if job.settings.processing != Default::default() {
            let (text, observation) =
                job.settings
                    .processing
                    .process(&raw_text, context, &job.control.cancelled);
            crate::dictation_processor::Processed {
                text,
                observation,
                transformations: Vec::new(),
            }
        } else {
            let modes = self
                .modes
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            modes.process_cancellable(
                &raw_text,
                &snapshot,
                &self.transformations,
                &job.control.cancelled,
            )
        };
        timings.cleanup_ms = Some(processing_started.elapsed().as_millis() as u64);
        if processed.text.trim().is_empty() {
            return Err(eyre!("text replacements produced empty output"));
        }
        job.retained_text = Some(processed.text.clone());
        job.processing = processed.observation.clone();
        if job.retry.is_some() {
            if job.control.cancelled.load(Ordering::Acquire) {
                return Err(eyre!("Dictation cancelled"));
            }
        } else {
            measure_stage(&mut timings.paste_ms, || {
                paster
                    .as_mut()
                    .map_err(|error| eyre!("{error:#}"))?
                    .paste_with_commit(&processed.text, &job.control.cancelled, || {
                        job.control.commit()
                    })
            })?;
            if let Some(history) = &self.history {
                let draft = crate::dictation_modes::history_draft(
                    raw_text,
                    &processed,
                    context.application.clone(),
                    job.audio_ms,
                    inference_ms,
                    started.elapsed().as_millis() as u64,
                );
                if let Err(error) = history.record(draft) {
                    tracing::warn!(%error, "could not retain Linux dictation history");
                }
            }
        }
        if let Some(error) = processed
            .observation
            .and_then(|processing| processing.fallback)
        {
            // Preserve successful corrected output, but show the cleanup failure in Last dictation.
            tracing::warn!(%error, "provider cleanup preserved its input");
        }
        Ok(processed.text)
    }
}

fn output_jobs(
    job_receiver: mpsc::Receiver<OutputJob>,
    worker_stop: Arc<AtomicBool>,
    worker_paste_with_shift: Arc<AtomicBool>,
    output_settings: DictationOutput,
    partial_events: EventLog,
    result_sender: mpsc::Sender<CompletedOutput>,
) {
    let mut paster = LinuxPaster::new(worker_stop.clone(), worker_paste_with_shift);
    let mut last_dictation: Option<String> = None;
    while let Ok(job) = job_receiver.recv() {
        if worker_stop.load(Ordering::Acquire) {
            break;
        }
        let (mut job, kind) = match job {
            OutputJob::Dictation(job) => (job, OutputKind::Dictation),
            OutputJob::VoiceAction(job) => (job, OutputKind::VoiceAction),
            OutputJob::PasteLast => {
                let result = last_dictation
                    .clone()
                    .ok_or_else(|| eyre!("there is no completed dictation to paste"))
                    .and_then(|text| {
                        paster
                            .as_mut()
                            .map_err(|error| eyre!("{error:#}"))?
                            .paste(&text)
                    })
                    .map(|()| String::new())
                    .map_err(|error| format!("{error:#}"));
                if result_sender
                    .send(CompletedOutput {
                        kind: OutputKind::PasteLast,
                        result,
                        timings: DictationTimings::default(),
                        preview: false,
                        processing: None,
                        cancelled: false,
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        let started = Instant::now();
        let preview = job.retry.is_some();
        let mut timings = DictationTimings {
            audio_ms: job.audio_ms,
            realtime: !preview
                && job
                    .settings
                    .cloud_transcription
                    .as_ref()
                    .is_some_and(|cloud| cloud.mode == crate::cloud::TranscriptionMode::Realtime),
            queue_ms: job.released_at.elapsed().as_millis() as u64,
            ..Default::default()
        };
        let mut context: Option<Arc<crate::linux_context::CaptureContext>> = None;
        let result = (|| -> Result<String> {
            if job.control.cancelled.load(Ordering::Acquire) {
                return Err(eyre!("Dictation cancelled"));
            }
            let text = if let Some(text) = job.retry.as_ref().and_then(|retry| retry.raw.clone()) {
                text
            } else {
                measure_stage(&mut timings.transcription_ms, || -> Result<String> {
                    if let Some(cloud) = &job.settings.cloud_transcription {
                        let outcome = crate::cloud::transcribe_recording(
                            cloud,
                            &job.samples,
                            job.realtime.take(),
                            preview,
                            &job.control.cancelled,
                            |text| {
                                if let Some(preview) = job.preview {
                                    preview.update(text);
                                }
                                let _ = partial_events.emit(&VoiceEvent::Transcript {
                                    timestamp_ms: now_ms(),
                                    phase: TranscriptPhase::Updated,
                                    latency_ms: 0,
                                    text: text.into(),
                                });
                            },
                        );
                        timings.retries = outcome.retries;
                        outcome.result
                    } else {
                        let transcribed = job
                            .transcript
                            .take()
                            .ok_or_else(|| eyre!("Local transcription is not ready"))?
                            .wait_cancellable(&job.control.cancelled)
                            .map_err(|error| eyre!(error))?;
                        tracing::info!(
                            source = transcribed.source.label(),
                            finish_ms = transcribed.finish_ms,
                            "transcribed Linux dictation"
                        );
                        Ok(transcribed.text)
                    }
                })?
            };
            if text.trim().is_empty() {
                return Err(eyre!("transcription was empty"));
            }
            let captured = measure_stage(&mut timings.context_wait_ms, || {
                job.retry
                    .as_ref()
                    .map(|retry| retry.context())
                    .unwrap_or_else(|| {
                        job.context
                            .as_ref()
                            .map(|capture| capture.take())
                            .unwrap_or_default()
                    })
            });
            output_settings
                .last
                .raw(job.recording_id, &text, captured.clone());
            context = Some(captured.clone());
            let inference_ms = started.elapsed().as_millis() as u64;
            if kind == OutputKind::VoiceAction {
                return run_voice_action(
                    &text,
                    paster.as_mut().map_err(|error| eyre!("{error:#}"))?,
                    &job.control,
                );
            }
            let text = output_settings.deliver(
                text,
                &mut paster,
                &mut job,
                &captured,
                inference_ms,
                started,
                &mut timings,
            )?;
            if worker_stop.load(Ordering::Acquire) {
                job.control.cancel();
            }
            Ok(text)
        })()
        .map_err(|error| format!("{error:#}"));
        if context.is_none()
            && let Some(capture) = &job.context
        {
            let captured = measure_stage(&mut timings.context_wait_ms, || capture.take());
            output_settings.last.context(job.recording_id, captured);
        }
        job.control.finish();
        timings.total_ms = job.released_at.elapsed().as_millis() as u64;
        output_settings.last.finish(
            job.recording_id,
            job.retained_text
                .as_deref()
                .filter(|_| !job.control.cancelled.load(Ordering::Acquire)),
            result.as_ref().err().cloned().or_else(|| {
                job.processing
                    .as_ref()
                    .and_then(|processing| processing.fallback.clone())
            }),
            Some(timings.clone()),
        );
        tracing::info!(?timings, preview, "completed Linux dictation job");
        if let Ok(text) = &result
            && kind == OutputKind::Dictation
            && !preview
        {
            last_dictation = Some(text.clone());
        }
        if result_sender
            .send(CompletedOutput {
                kind,
                result,
                timings,
                preview,
                processing: job.processing,
                cancelled: job.control.cancelled.load(Ordering::Acquire),
            })
            .is_err()
        {
            break;
        }
    }
}

/// Runs hotkey dictation until `shutdown`. A prepared transcriber skips the
/// model load; otherwise the supplied transcription selection is loaded here.
/// `updates` change settings, the model, and shortcut registration in place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    event_path: &Path,
    device: Option<&str>,
    shutdown: &AtomicBool,
    settings: &AppSettings,
    output_settings: DictationOutput,
    transcriber: Option<LocalTranscriber>,
    hotkey_status: HotkeyStatus,
    commands: CommandQueue,
    updates: mpsc::Receiver<ListenerUpdate>,
) -> Result<()> {
    let mut settings = settings.clone();
    let mut engine = if settings.cloud_transcription.is_none() {
        Some(TranscriptionEngine::start(transcriber.map_or_else(
            || LocalTranscriber::load(&settings.transcription),
            Ok,
        )?)?)
    } else {
        None
    };
    let last = output_settings.last.clone();
    let retry_connection = last.connect();
    // Drop audio/hotkeys and close the job queue before joining slow output on
    // every exit, including failures during startup or recording.
    let mut output = OutputWorker {
        stop: Arc::new(AtomicBool::new(false)),
        worker: None,
        unfinished: Default::default(),
    };
    // A missing microphone falls back to the default one. A released
    // microphone opens only while a dictation records.
    let mut input = microphone_input(&settings, device);
    let mut release_while_idle = settings.release_microphone_while_idle;
    let (binding, options) = shortcuts(&settings);
    let hotkey_label = binding.label();
    let mut toggle = options.toggle;
    let start_shortcuts = |(binding, options): (LinuxHotkey, HotkeyOptions)| {
        LinuxHotkeyMonitor::start(binding, options, hotkey_status.clone(), commands.clone())
    };
    let mut hotkey = Some(start_shortcuts((binding, options))?);
    let mut shortcuts_suspended = false;
    let mut indicator = LinuxIndicator::new(settings.indicator_position);
    let suppression_behavior = |behavior| {
        if behavior == crate::app_settings::RecordingAudioBehavior::PauseMedia {
            crate::app_settings::RecordingAudioBehavior::DoNothing
        } else {
            behavior
        }
    };
    let mut suppressor = crate::linux_audio_control::AudioSuppressor::new(suppression_behavior(
        settings.recording_audio_behavior,
    ));
    let mut media = crate::linux_media::MediaController::new(
        settings.recording_audio_behavior
            == crate::app_settings::RecordingAudioBehavior::PauseMedia,
    );
    let paste_with_shift = Arc::new(AtomicBool::new(settings.paste_with_shift));
    let worker_paste_with_shift = paste_with_shift.clone();
    let (jobs, job_receiver) = mpsc::sync_channel::<OutputJob>(2);
    let (result_sender, results) = mpsc::channel();
    let worker_stop = output.stop.clone();
    let partial_events = EventLog::create(event_path)?;
    output.worker = Some(thread::spawn(move || {
        output_jobs(
            job_receiver,
            worker_stop,
            worker_paste_with_shift,
            output_settings,
            partial_events,
            result_sender,
        );
    }));

    let mut events = EventLog::create(event_path)?;
    let mut audio_ready = input.is_open();
    // A released microphone's sample rate is unknown until it opens; the
    // first chunk replaces this placeholder capture.
    let mut capture = DictationCapture::new(if audio_ready {
        input.sample_rate()
    } else {
        16_000
    });
    let released_label = |settings: &AppSettings| {
        device
            .or(settings.microphone.as_deref())
            .unwrap_or("microphone")
            .to_string()
    };
    let mut device_label = if audio_ready {
        input.device_name().to_string()
    } else {
        released_label(&settings)
    };
    // Updates wait for a moment without a recording.
    let mut next_settings: Option<AppSettings> = None;
    let mut model = crate::local_transcriber::ModelSwap::new(&settings.transcription);
    let mut next_suspended: Option<bool> = None;
    // A press that arrived while the released microphone was opening;
    // recording starts at its first chunk.
    let mut start_pending = false;
    let mut physical_press: Option<Instant> = None;
    let mut last_audio_at = Instant::now();
    // The recording, or the pending one, is a Voice Action instruction.
    let mut recording_voice = false;
    let mut captured_through = CaptureInstant::ZERO;
    let mut finishing: Option<Finishing> = None;
    let mut pending = 0_usize;
    let mut capture_work = CaptureWork::default();
    events.emit(&VoiceEvent::SessionStarted {
        timestamp_ms: now_ms(),
    })?;
    emit_state(&mut events, VoiceState::Listening, &device_label)?;
    println!(
        "HEX dictation is ready on {device_label}. Hold {hotkey_label} to dictate; Ctrl-C stops."
    );

    while !shutdown.load(Ordering::Relaxed) {
        for retry in retry_connection.requests.try_iter() {
            let result = last.recording(retry).and_then(|recording| {
                let samples = recording.samples.clone();
                let control = Arc::new(JobControl::default());
                let transcript =
                    if settings.cloud_transcription.is_none() && recording.raw.is_none() {
                        Some(
                            engine
                                .as_ref()
                                .ok_or_else(|| eyre!("Local transcription is not ready"))?
                                .finish(None, (*samples).clone())?,
                        )
                    } else {
                        None
                    };
                let job = Job {
                    audio_ms: samples.len() as u64 / 16,
                    samples,
                    transcript,
                    settings: settings.clone(),
                    processing: None,
                    retained_text: None,
                    preview: None,
                    recording_id: retry.id,
                    released_at: Instant::now(),
                    retry: Some(recording),
                    context: None,
                    realtime: None,
                    control: control.clone(),
                };
                jobs.try_send(OutputJob::Dictation(job))
                    .map_err(|_| eyre!("Dictation queue is busy; retry again shortly"))?;
                output.unfinished.push_back(control);
                pending += 1;
                Ok(())
            });
            if let Err(error) = result {
                last.finish(retry.id, None, Some(error.to_string()), None);
            }
        }
        for update in updates.try_iter() {
            match update {
                ListenerUpdate::Settings(next) => next_settings = Some(*next),
                ListenerUpdate::Transcriber {
                    selection,
                    transcriber,
                } => model.offer(&selection, *transcriber),
                ListenerUpdate::SuspendShortcuts(suspended) => next_suspended = Some(suspended),
            }
        }
        if let Some(failed) = model.poll() {
            events.dictation(DictationPhase::Failed(failed.error), "")?;
        }
        if !capture.is_recording() && !start_pending && finishing.is_none() {
            let mut register = next_suspended.take().is_some_and(|suspended| {
                std::mem::replace(&mut shortcuts_suspended, suspended) != suspended
            });
            if let Some(next) = next_settings.take() {
                register |= shortcuts(&next) != shortcuts(&settings);
                if next.cloud_transcription.is_none() {
                    model.want(&next.transcription);
                }
                if next.cloud_transcription.is_none()
                    && engine.is_none()
                    && next.transcription == settings.transcription
                {
                    model.reload(&next.transcription);
                }
                toggle = next.shortcut_mode == crate::dictation::ShortcutMode::Toggle;
                paste_with_shift.store(next.paste_with_shift, Ordering::Relaxed);
                if next.indicator_position != settings.indicator_position {
                    indicator = LinuxIndicator::new(next.indicator_position);
                }
                if next.recording_audio_behavior != settings.recording_audio_behavior {
                    suppressor = crate::linux_audio_control::AudioSuppressor::new(
                        suppression_behavior(next.recording_audio_behavior),
                    );
                    media = crate::linux_media::MediaController::new(
                        next.recording_audio_behavior
                            == crate::app_settings::RecordingAudioBehavior::PauseMedia,
                    );
                }
                if device.is_none() && next.microphone != settings.microphone
                    || next.release_microphone_while_idle != release_while_idle
                {
                    drop(input);
                    input = microphone_input(&next, device);
                    release_while_idle = next.release_microphone_while_idle;
                    audio_ready = input.is_open();
                    if audio_ready {
                        capture = DictationCapture::new(input.sample_rate());
                        device_label = input.device_name().to_string();
                    } else {
                        device_label = released_label(&next);
                    }
                    emit_state(&mut events, active_state(false, pending), &device_label)?;
                }
                settings = next;
            }
            if register {
                // The old registration goes first: X11 grabs the same keys again.
                drop(hotkey.take());
                if !shortcuts_suspended {
                    hotkey = Some(start_shortcuts(shortcuts(&settings))?);
                }
            }
            if pending == 0
                && let Some(next) = model.take_ready()
                && let Some(previous) = engine.replace(TranscriptionEngine::start(next)?)
            {
                previous.retire();
            }
        }
        if let Some(hotkey) = &hotkey
            && let Ok(error) = hotkey.errors.try_recv()
        {
            return Err(eyre!("Linux hotkey monitor stopped: {error}"));
        }
        // A release keeps recording through its tail; shortcut events wait,
        // so a press inside the tail starts after this clip is submitted.
        if let Some(release) = &finishing
            && (captured_through >= release.ended_at + POST_RELEASE_TAIL
                || Instant::now() >= release.deadline)
        {
            let ended_at = release.ended_at;
            let voice = release.voice;
            tracing::debug!(
                tail_ms = release.released.elapsed().as_millis(),
                "post-release tail captured"
            );
            finishing = None;
            recording_voice = false;
            submit_capture(
                &mut capture,
                ended_at,
                engine.as_ref(),
                &jobs,
                &mut events,
                &mut pending,
                voice,
                std::mem::take(&mut capture_work),
                &last,
                &settings,
                &mut output.unfinished,
            )?;
            suppressor.restore();
            if release_while_idle {
                input.close_idle();
                audio_ready = false;
            }
            emit_state(
                &mut events,
                active_state(capture.is_recording(), pending),
                &device_label,
            )?;
        }
        while finishing.is_none()
            && let Some(action) = hotkey
                .as_ref()
                .and_then(|hotkey| hotkey.events.try_recv().ok())
        {
            match action {
                HotkeyEvent::Start | HotkeyEvent::VoiceStart
                    if !capture.is_recording() && !start_pending =>
                {
                    if settings.cloud_transcription.is_none() && engine.is_none() {
                        events.dictation(
                            DictationPhase::Failed(
                                "Local transcription model is still preparing".into(),
                            ),
                            "",
                        )?;
                        continue;
                    }
                    physical_press = Some(Instant::now());
                    last_audio_at = Instant::now();
                    recording_voice = action == HotkeyEvent::VoiceStart;
                    capture_work = CaptureWork {
                        context: None,
                        realtime: None,
                        control: Default::default(),
                        preview: Some(crate::linux_rune_hud::Preview::acquire()),
                    };
                    if audio_ready {
                        // An instruction plays no start tone, as on Windows.
                        start_capture(&mut capture, captured_through, || {
                            play_start_tone(recording_voice)
                        });
                        if settings.cloud_transcription.is_none()
                            && let Some(engine) = &engine
                        {
                            stream_when_due(&mut capture, engine, toggle);
                        }
                    } else if release_while_idle {
                        start_pending = true;
                        feedback::prepare();
                    } else {
                        continue;
                    }
                    events.dictation(DictationPhase::Started, "")?;
                    emit_state(&mut events, VoiceState::Dictating, &device_label)?;
                }
                HotkeyEvent::Finish | HotkeyEvent::VoiceFinish | HotkeyEvent::Cancel
                    if start_pending =>
                {
                    recording_voice = false;
                    // Released before the opening microphone produced audio:
                    // nothing was captured.
                    start_pending = false;
                    suppressor.restore();
                    input.close_idle();
                    audio_ready = false;
                    events.dictation(
                        if action == HotkeyEvent::Cancel {
                            DictationPhase::Cancelled
                        } else {
                            DictationPhase::Discarded
                        },
                        "",
                    )?;
                    emit_state(&mut events, active_state(false, pending), &device_label)?;
                }
                HotkeyEvent::Finish | HotkeyEvent::VoiceFinish
                    if capture.is_recording()
                        && (action == HotkeyEvent::VoiceFinish) == recording_voice =>
                {
                    finishing = Some(begin_finishing(
                        &capture,
                        captured_through,
                        recording_voice,
                        || suppressor.restore_then(|| feedback::play(Tone::DictationStop)),
                    ));
                }
                HotkeyEvent::PasteLast if !capture.is_recording() => {
                    match jobs.try_send(OutputJob::PasteLast) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => {
                            return Err(eyre!("dictation worker is unavailable"));
                        }
                    }
                }
                HotkeyEvent::Cancel if capture.is_recording() => {
                    recording_voice = false;
                    capture_work.control.cancel();
                    capture_work.realtime = None;
                    capture.cancel();
                    feedback::retract_start();
                    feedback::play(Tone::Cancel);
                    suppressor.restore();
                    if release_while_idle {
                        input.close_idle();
                        audio_ready = false;
                    }
                    events.dictation(DictationPhase::Cancelled, "")?;
                    emit_state(
                        &mut events,
                        active_state(capture.is_recording(), pending),
                        &device_label,
                    )?;
                }
                HotkeyEvent::Cancel => {
                    if let Some(job) = output.unfinished.iter().rev().find(|job| job.cancel()) {
                        let _ = job;
                    }
                }
                _ => {}
            }
        }
        while let Ok(CompletedOutput {
            kind,
            result,
            timings,
            preview,
            processing,
            cancelled,
        }) = results.try_recv()
        {
            // A repeated paste has no transcript and is not pending work.
            if kind == OutputKind::PasteLast {
                if let Err(error) = result {
                    events.dictation(DictationPhase::Failed(error), "")?;
                }
                continue;
            }
            pending = pending.saturating_sub(1);
            output.unfinished.pop_front();
            match result {
                Ok(text) => {
                    events.emit(&VoiceEvent::Transcript {
                        timestamp_ms: now_ms(),
                        phase: TranscriptPhase::Completed,
                        latency_ms: 0,
                        text: text.clone(),
                    })?;
                    events.timed_dictation(
                        if preview {
                            DictationPhase::Edited
                        } else {
                            DictationPhase::Pasted
                        },
                        text,
                        processing,
                        timings,
                    )?;
                }
                Err(error) => events.timed_dictation(
                    if cancelled {
                        DictationPhase::Cancelled
                    } else {
                        DictationPhase::Failed(error)
                    },
                    "",
                    processing,
                    timings,
                )?,
            }
            emit_state(
                &mut events,
                active_state(capture.is_recording(), pending),
                &device_label,
            )?;
        }

        if capture.is_recording() && (toggle || capture.is_intentional()) {
            if capture_work.context.is_none() {
                capture_work.context = Some(crate::linux_context::ContextCapture::start(
                    settings.processing.clone(),
                ));
                if let Some(cloud) = settings
                    .cloud_transcription
                    .as_ref()
                    .filter(|cloud| cloud.mode == crate::cloud::TranscriptionMode::Realtime)
                {
                    capture_work.realtime = Some(crate::cloud::RealtimeSession::start(
                        cloud.clone(),
                        capture_work.control.cancelled.clone(),
                        {
                            let preview = capture_work.preview;
                            move |text| {
                                if let Some(preview) = preview {
                                    preview.update(text);
                                }
                            }
                        },
                    ));
                }
            }
            if let Some(realtime) = &mut capture_work.realtime {
                realtime.offer(capture.live_samples_from(realtime.offered_samples()));
            }
        }
        if release_while_idle
            && capture.is_recording()
            && finishing.is_none()
            && physical_press
                .is_some_and(|press| press.elapsed() >= crate::dictation::MINIMUM_HOLD_DURATION)
            && capture.mark_intentional()
        {
            suppressor.suppress();
        }
        if release_while_idle && !capture.is_recording() && !start_pending && finishing.is_none() {
            input.close_idle();
            audio_ready = false;
        }
        let intentional_cold_press = start_pending
            && physical_press
                .is_some_and(|press| press.elapsed() >= crate::dictation::MINIMUM_HOLD_DURATION);
        if let Some(media) = &media {
            media.update(capture.is_intentional() || intentional_cold_press, pending);
        }
        if start_pending
            && media
                .as_ref()
                .is_none_or(|media| intentional_cold_press && media.pause_attempt_finished())
        {
            input.request_open();
        }
        indicator.update(capture.is_recording() || start_pending, pending);
        crate::linux_gnome::set_indicator(
            capture.is_recording() || start_pending,
            pending,
            settings.indicator_position,
        );
        match input.recv_timeout(UPDATE_INTERVAL, !capture.is_recording()) {
            RecoveringAudioInputEvent::Chunk {
                samples,
                captured_through: chunk_captured_through,
            } => {
                last_audio_at = Instant::now();
                captured_through = chunk_captured_through;
                if !audio_ready {
                    capture = DictationCapture::new(input.sample_rate());
                    audio_ready = true;
                    device_label = input.device_name().to_string();
                    if start_pending {
                        // The on-demand open finished while the shortcut is
                        // held: recording starts here, without pre-roll.
                        start_pending = false;
                        start_capture(&mut capture, captured_through, || {
                            play_start_tone(recording_voice)
                        });
                    }
                }
                capture.ingest(&samples, captured_through);
                if capture.become_intentional(captured_through) && finishing.is_none() {
                    // Ordinary shortcut chords and short taps never mute or pause.
                    suppressor.suppress();
                }
                if settings.cloud_transcription.is_none()
                    && let Some(engine) = &engine
                {
                    stream_when_due(&mut capture, engine, toggle);
                }
            }
            RecoveringAudioInputEvent::Timeout => {
                if capture.is_recording()
                    && finishing.is_none()
                    && last_audio_at.elapsed() >= Duration::from_secs(5)
                {
                    capture_work.control.cancel();
                    capture_work.realtime = None;
                    capture.cancel();
                    suppressor.restore();
                    feedback::retract_start();
                    feedback::play(Tone::Cancel);
                    input.request_recovery();
                    audio_ready = false;
                    events.dictation(DictationPhase::Failed("Microphone stopped delivering audio. Reconnect the microphone; for Bluetooth, disable Release microphone while idle to avoid profile switching.".into()), "")?;
                    emit_state(&mut events, active_state(false, pending), &device_label)?;
                }
            }
            RecoveringAudioInputEvent::Interrupted => {
                // A release still in its tail finishes with the audio it has.
                if let Some(release) = &mut finishing {
                    release.deadline = Instant::now();
                }
                audio_ready = false;
                if capture.is_recording() && finishing.is_none() {
                    recording_voice = false;
                    capture_work.control.cancel();
                    capture_work.realtime = None;
                    capture.cancel();
                    feedback::retract_start();
                    feedback::play(Tone::Cancel);
                    suppressor.restore();
                    events.dictation(DictationPhase::Cancelled, "")?;
                }
                events.dictation(
                    DictationPhase::Failed("microphone stream interrupted; reopening".into()),
                    "",
                )?;
                emit_state(
                    &mut events,
                    active_state(capture.is_recording(), pending),
                    &device_label,
                )?;
            }
            RecoveringAudioInputEvent::Reopened => {
                audio_ready = false;
                device_label = input.device_name().to_string();
            }
            RecoveringAudioInputEvent::OpenFailed(error) => {
                if !release_while_idle {
                    return Err(eyre!("could not open the microphone: {error}"));
                }
                // An on-demand open failed (device busy or unplugged): report
                // it and stay released rather than stopping the listener.
                start_pending = false;
                suppressor.restore();
                events.dictation(
                    DictationPhase::Failed(format!("could not open microphone: {error}")),
                    "",
                )?;
                emit_state(&mut events, active_state(false, pending), &device_label)?;
            }
        }
    }

    if capture.is_recording() {
        capture_work.control.cancel();
        capture_work.realtime = None;
        capture.cancel();
        events.dictation(DictationPhase::Cancelled, "")?;
    }
    indicator.update(false, pending);
    suppressor.restore();
    emit_state(&mut events, VoiceState::Stopping, &device_label)?;
    output.stop.store(true, Ordering::Release);
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

/// Pairs the spoken instruction with the selected text, asks OpenCode with
/// the Voice Action settings, and pastes the reply.
fn run_voice_action(
    instruction: &str,
    paster: &mut LinuxPaster,
    control: &JobControl,
) -> Result<String> {
    let context = crate::linux_context::capture();
    let selected = paster.selected_text();
    let processed = crate::dictation_processor::process_voice_action_cancellable(
        instruction,
        selected.as_deref(),
        &context,
        &control.cancelled,
    );
    if let Some(error) = processed
        .observation
        .and_then(|observation| observation.fallback)
    {
        return Err(eyre!(error));
    }
    paster.paste_with_commit(&processed.text, &control.cancelled, || control.commit())?;
    Ok(processed.text)
}

/// Applies the saved sound settings and preloads the tones.
pub(crate) fn initialize_feedback(settings: &crate::app_settings::AppSettings) {
    feedback::set_enabled(settings.sound_effects);
    feedback::set_volume(settings.sound_effect_volume.clamp(0.0, 1.0));
    if let Err(error) = feedback::preload() {
        tracing::warn!(%error, "recording sounds are unavailable; continuing without feedback");
    }
}

fn play_start_tone(voice: bool) {
    if !voice {
        feedback::play_provisional_start();
    }
}

fn start_capture(capture: &mut DictationCapture, started_at: CaptureInstant, play: impl FnOnce()) {
    capture.start(started_at);
    play();
}

/// Streams once the hold counts, so short taps and chords never reach the
/// model; a Toggle press is deliberate and streams at once.
fn stream_when_due(capture: &mut DictationCapture, engine: &TranscriptionEngine, toggle: bool) {
    if capture.is_recording() && (toggle || capture.is_intentional()) {
        capture.begin_stream(engine);
    }
}

/// Starts collecting a release's tail. A release that will be transcribed
/// is announced at once rather than after its tail; a short tap is discarded
/// on its release alone, needs no tail, and stays silent.
///
/// The release restores any muted output before playing the stop tone so it is
/// heard while the tail is still captured. The silence gate ignores the tail,
/// so the tone itself never makes a silent clip transcribe.
fn begin_finishing(
    capture: &DictationCapture,
    ended_at: CaptureInstant,
    voice: bool,
    announce: impl FnOnce(),
) -> Finishing {
    let now = Instant::now();
    let transcribes = capture.would_transcribe(ended_at);
    if transcribes {
        announce();
    }
    Finishing {
        ended_at,
        voice,
        released: now,
        deadline: if transcribes {
            now + POST_RELEASE_TAIL + TAIL_STALL_GRACE
        } else {
            now
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_capture(
    capture: &mut DictationCapture,
    ended_at: CaptureInstant,
    engine: Option<&TranscriptionEngine>,
    jobs: &SyncSender<OutputJob>,
    events: &mut EventLog,
    pending: &mut usize,
    voice: bool,
    work: CaptureWork,
    last: &crate::linux_last_dictation::LastDictation,
    settings: &AppSettings,
    unfinished: &mut std::collections::VecDeque<Arc<JobControl>>,
) -> Result<()> {
    match capture.finish_with_tail(ended_at, POST_RELEASE_TAIL) {
        Finish::Discard => {
            // A short tap or chord withdraws the start tone of its press.
            feedback::retract_start();
            events.dictation(DictationPhase::Discarded, "")?
        }
        Finish::Transcribe(clip) if clip.is_near_silent() => {
            events.dictation(DictationPhase::Discarded, "")?
        }
        Finish::Transcribe(mut clip) => {
            let audio_ms = clip.duration_ms();
            let stream = clip.take_stream();
            let samples = Arc::new(clip.into_transcription_samples());
            let recording_id = if voice {
                0
            } else {
                last.retain(samples.clone())
            };
            if let Some(context) = &work.context {
                last.pending_context(recording_id, context.clone());
            }
            let transcript = if settings.cloud_transcription.is_none() {
                Some(
                    engine
                        .ok_or_else(|| eyre!("Local transcription is not ready"))?
                        .finish(stream, (*samples).clone())?,
                )
            } else {
                None
            };
            let control = work.control.clone();
            let job = Job {
                transcript,
                samples,
                audio_ms,
                recording_id,
                released_at: Instant::now(),
                retry: None,
                context: work.context,
                realtime: work.realtime,
                control: work.control,
                settings: settings.clone(),
                processing: None,
                retained_text: None,
                preview: work.preview,
            };
            let job = if voice {
                OutputJob::VoiceAction(job)
            } else {
                OutputJob::Dictation(job)
            };
            match jobs.try_send(job) {
                Ok(()) => {
                    unfinished.push_back(control);
                    *pending += 1;
                    events.dictation(DictationPhase::Transcribing, "")?;
                }
                Err(TrySendError::Full(_)) => {
                    last.finish(
                        recording_id,
                        None,
                        Some("dictation queue is full".into()),
                        None,
                    );
                    events
                        .dictation(DictationPhase::Failed("dictation queue is full".into()), "")?;
                }
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

fn measure_stage<T>(duration: &mut Option<u64>, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = operation();
    *duration = Some(started.elapsed().as_millis() as u64);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription_engine::Command;

    fn capture_time() -> CaptureInstant {
        CaptureInstant::from_nanos(60_000_000_000)
    }

    fn event_log() -> (EventLog, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "hex-linux-dictation-{}-{}.ndjson",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (EventLog::create(&path).unwrap(), path)
    }

    /// Records `seconds` of speech-level audio in 100 ms chunks, streaming
    /// as the live loop would.
    fn record(
        capture: &mut DictationCapture,
        engine: &TranscriptionEngine,
        toggle: bool,
        seconds: u64,
    ) -> CaptureInstant {
        let start = capture_time();
        start_capture(capture, start, || {});
        stream_when_due(capture, engine, toggle);
        let mut through = start;
        for _ in 0..seconds * 10 {
            through = through + Duration::from_millis(100);
            let tone = (0..1_600)
                .map(|index| 0.1 * (index as f32 * 0.05).sin())
                .collect::<Vec<_>>();
            capture.ingest(&tone, through);
            capture.become_intentional(through);
            stream_when_due(capture, engine, toggle);
        }
        through
    }

    #[test]
    fn cleanup_retry_without_a_display_previews_corrections_without_pasting() {
        let mut settings = AppSettings::default();
        settings.processing.default_mode.replacements.push(
            crate::text_replacements::TextReplacement {
                matched_phrase: "hex app".into(),
                output: "HEX".into(),
            },
        );
        let output = DictationOutput::from_settings(&settings, None);
        let samples = Arc::new(vec![0.1; 16_000]);
        let id = output.last.retain(samples.clone());
        let context = Arc::new(crate::linux_context::CaptureContext::default());
        output.last.raw(id, "hex app", context.clone());
        let recording = output
            .last
            .recording(crate::linux_last_dictation::Retry {
                id,
                cleanup_only: true,
            })
            .unwrap();
        let mut job = Job {
            samples,
            transcript: None,
            settings,
            recording_id: id,
            released_at: Instant::now(),
            retry: Some(recording),
            audio_ms: 1000,
            context: None,
            realtime: None,
            control: Default::default(),
            preview: None,
            processing: None,
            retained_text: None,
        };
        let mut unavailable_paster = Err(eyre!("no display"));
        let mut timings = DictationTimings::default();
        let result = output
            .deliver(
                "hex app".into(),
                &mut unavailable_paster,
                &mut job,
                &context,
                0,
                Instant::now(),
                &mut timings,
            )
            .unwrap();
        assert_eq!(result, "HEX");
        assert!(timings.paste_ms.is_none());
        assert_eq!(job.control.state.load(Ordering::Acquire), 0);
    }

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
        let started_at = capture_time();
        let mut sounds = Vec::new();

        start_capture(&mut capture, started_at, || {
            sounds.push(Tone::DictationStart)
        });

        assert!(capture.is_recording());
        assert_eq!(sounds, [Tone::DictationStart]);
        assert!(matches!(
            capture.finish(started_at + Duration::from_millis(100)),
            Finish::Discard
        ));
    }

    #[test]
    fn hold_streams_after_the_threshold_and_toggle_streams_from_the_press() {
        for toggle in [false, true] {
            let (engine, commands) = TranscriptionEngine::for_test();
            let mut capture = DictationCapture::new(16_000);
            let start = capture_time();
            start_capture(&mut capture, start, || {});
            stream_when_due(&mut capture, &engine, toggle);
            assert_eq!(capture.is_streaming(), toggle);
            capture.ingest(&[0.1; 1_600], start + Duration::from_millis(100));
            capture.become_intentional(start + Duration::from_millis(100));
            stream_when_due(&mut capture, &engine, toggle);
            assert_eq!(capture.is_streaming(), toggle, "toggle={toggle}");
            capture.ingest(&[0.1; 3_200], start + MINIMUM_HOLD);
            capture.become_intentional(start + MINIMUM_HOLD);
            stream_when_due(&mut capture, &engine, toggle);
            assert!(capture.is_streaming());
            let begun = commands
                .try_iter()
                .filter(|command| matches!(command, Command::Begin { .. }))
                .count();
            assert_eq!(begun, 1);
        }
    }

    const MINIMUM_HOLD: Duration = crate::dictation::MINIMUM_HOLD_DURATION;

    #[test]
    fn a_release_finishes_its_stream_with_the_complete_clip() {
        let (engine, commands) = TranscriptionEngine::for_test();
        let (mut events, path) = event_log();
        let (jobs, queued) = mpsc::sync_channel(2);
        let mut capture = DictationCapture::new(16_000);
        let released = record(&mut capture, &engine, false, 2);
        let mut pending = 0;
        // The release is announced at once, then waits for its tail before
        // it is submitted.
        let mut sounds = Vec::new();
        let finishing = begin_finishing(
            &capture,
            released.checked_sub(POST_RELEASE_TAIL).unwrap(),
            false,
            || sounds.push(Tone::DictationStop),
        );
        assert_eq!(sounds, [Tone::DictationStop]);
        assert!(finishing.deadline > Instant::now());
        submit_capture(
            &mut capture,
            finishing.ended_at,
            Some(&engine),
            &jobs,
            &mut events,
            &mut pending,
            false,
            CaptureWork::default(),
            &Default::default(),
            &AppSettings::default(),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(pending, 1);
        let Ok(OutputJob::Dictation(job)) = queued.try_recv() else {
            panic!("a dictation job");
        };
        assert_eq!(job.audio_ms, 2_000);
        let received = commands.try_iter().collect::<Vec<_>>();
        let fed: usize = received
            .iter()
            .map(|command| match command {
                Command::Feed { samples, .. } => samples.len(),
                _ => 0,
            })
            .sum();
        assert!(matches!(
            received.first(),
            Some(Command::Begin { stream: 1, .. })
        ));
        assert!(matches!(
            received.last(),
            Some(Command::Finish { stream: Some(1), clip, .. }) if clip.len() == 32_000
        ));
        assert!(fed > 16_000 && fed <= 32_000);
        assert!(
            !received
                .iter()
                .any(|command| matches!(command, Command::Cancel { .. }))
        );
        drop(events);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn escape_and_short_taps_cancel_the_stream_and_submit_nothing() {
        let (engine, commands) = TranscriptionEngine::for_test();
        let (mut events, path) = event_log();
        let (jobs, queued) = mpsc::sync_channel(2);
        let mut pending = 0;

        let mut cancelled = DictationCapture::new(16_000);
        record(&mut cancelled, &engine, true, 1);
        cancelled.cancel();

        let mut short = DictationCapture::new(16_000);
        let start = capture_time();
        start_capture(&mut short, start, || {});
        stream_when_due(&mut short, &engine, true);
        short.ingest(&[0.1; 1_600], start + Duration::from_millis(100));
        let finishing = begin_finishing(&short, start + Duration::from_millis(100), false, || {
            panic!("a short tap plays no stop tone")
        });
        assert!(
            finishing.deadline <= Instant::now(),
            "a short tap needs no tail"
        );
        submit_capture(
            &mut short,
            finishing.ended_at,
            Some(&engine),
            &jobs,
            &mut events,
            &mut pending,
            false,
            CaptureWork::default(),
            &Default::default(),
            &AppSettings::default(),
            &mut Default::default(),
        )
        .unwrap();

        assert_eq!(pending, 0);
        assert!(queued.try_recv().is_err());
        let received = commands.try_iter().collect::<Vec<_>>();
        let count = |matches: fn(&Command) -> bool| received.iter().filter(|c| matches(c)).count();
        assert_eq!(count(|c| matches!(c, Command::Begin { .. })), 2);
        assert_eq!(count(|c| matches!(c, Command::Cancel { .. })), 2);
        assert_eq!(count(|c| matches!(c, Command::Finish { .. })), 0);
        drop(events);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_model_that_cannot_stream_transcribes_the_whole_clip_after_release() {
        let (engine, commands) = TranscriptionEngine::for_test_without_streaming();
        let (mut events, path) = event_log();
        let (jobs, queued) = mpsc::sync_channel(2);
        let mut capture = DictationCapture::new(16_000);
        let released = record(&mut capture, &engine, true, 2);
        assert!(!capture.is_streaming());
        let mut pending = 0;
        submit_capture(
            &mut capture,
            released.checked_sub(POST_RELEASE_TAIL).unwrap(),
            Some(&engine),
            &jobs,
            &mut events,
            &mut pending,
            false,
            CaptureWork::default(),
            &Default::default(),
            &AppSettings::default(),
            &mut Default::default(),
        )
        .unwrap();
        assert!(queued.try_recv().is_ok());
        assert!(matches!(
            commands.try_iter().collect::<Vec<_>>().as_slice(),
            [Command::Finish { stream: None, clip, .. }] if clip.len() == 32_000
        ));
        drop(events);
        let _ = std::fs::remove_file(path);
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
