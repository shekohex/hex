use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use base64::{Engine, prelude::BASE64_STANDARD};
use color_eyre::eyre::{Result, bail, eyre};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Sender, channel, error::TrySendError};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

use super::{MAX_RESPONSE_BYTES, TranscriptionSettings, cancellation, nonempty, run_cancellable};

enum AudioMessage {
    Samples(Vec<f32>),
    Finish(Vec<f32>),
}

/// One intentional recording. Admission never waits on network I/O.
pub struct RealtimeSession {
    audio: Sender<AudioMessage>,
    result: Receiver<Result<String, String>>,
    cancelled: Arc<AtomicBool>,
    offered_samples: usize,
}

impl RealtimeSession {
    pub fn start(
        settings: TranscriptionSettings,
        shutdown: Arc<AtomicBool>,
        mut observe: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        let (audio, input) = channel(64);
        let (sender, result) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    tokio::select! {
                        biased;
                        _ = cancellation(&worker_cancelled) => bail!("Provider request cancelled"),
                        _ = cancellation(&shutdown) => bail!("Provider request cancelled"),
                        result = stream(settings, input, &mut observe) => result,
                    }
                })
            })()
            .map_err(|error| error.to_string());
            let _ = sender.try_send(result);
        });
        Self {
            audio,
            result,
            cancelled,
            offered_samples: 0,
        }
    }

    pub fn offer(&mut self, samples: &[f32]) {
        for chunk in samples.chunks(1600) {
            match self.audio.try_send(AudioMessage::Samples(chunk.to_vec())) {
                Ok(()) => self.offered_samples += chunk.len(),
                // Capture retains the unaccepted suffix. Retry on the next
                // callback, or drain it in finish on the output worker.
                Err(TrySendError::Closed(_) | TrySendError::Full(_)) => return,
            }
        }
    }

    pub fn offered_samples(&self) -> usize {
        self.offered_samples
    }

    /// One live attempt. Batch recovery belongs to the recording coordinator.
    pub fn finish(
        self,
        samples: &[f32],
        shutdown: &AtomicBool,
        deadline: Duration,
    ) -> Result<String> {
        if samples.len() < self.offered_samples {
            bail!("Realtime capture boundary changed; nothing pasted");
        }
        let tail = samples[self.offered_samples..].to_vec();
        let audio = self.audio.clone();
        run_cancellable(shutdown, deadline, async {
            // Closed input means the worker exited; read its result rather than
            // hiding the connection/setup failure behind an audio-queue error.
            let _ = audio.send(AudioMessage::Finish(tail)).await;
            loop {
                match self.result.try_recv() {
                    Ok(result) => return result.map_err(|error| eyre!(error)),
                    Err(mpsc::TryRecvError::Disconnected) => bail!("Realtime worker stopped"),
                    Err(mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                }
            }
        })
    }
}

impl Drop for RealtimeSession {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

async fn stream(
    settings: TranscriptionSettings,
    mut input: tokio::sync::mpsc::Receiver<AudioMessage>,
    observe: &mut dyn FnMut(&str),
) -> Result<String> {
    settings.validate()?;
    let mut url = settings.connection.endpoint("realtime")?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| eyre!("Invalid realtime endpoint"))?;
    url.query_pairs_mut()
        .append_pair("intent", "transcription")
        .append_pair("model", &settings.model);
    let mut request = url.as_str().into_client_request()?;
    let key = settings.connection.api_key()?;
    if !key.is_empty() {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {key}").parse()?);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RESPONSE_BYTES))
        .max_frame_size(Some(MAX_RESPONSE_BYTES));
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| {
        eyre!(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Realtime connection timed out"
        ))
    })??;
    let mut transcription = json!({"model": settings.model});
    if !settings.language.is_empty() {
        if super::uses_language_list(&settings.model) {
            transcription["languages"] = json!([settings.language]);
        } else {
            transcription["language"] = json!(settings.language);
        }
    }
    if !settings.prompt.is_empty() {
        transcription["prompt"] = json!(settings.prompt);
    }
    socket
        .send(Message::Text(
            json!({
                "type": "session.update",
                "session": {
                    "type": "transcription",
                    "audio": {"input": {
                        "format": {"type": "audio/pcm", "rate": 24000},
                        "transcription": transcription,
                        "turn_detection": if settings.server_vad {
                            json!({"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": 500})
                        } else { Value::Null }
                    }}
                }
            })
            .to_string()
            .into(),
        ))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(data))) => {
                    let event: Value = serde_json::from_str(&data)?;
                    if event["type"] == "error" {
                        return Err(provider_error(
                            &event,
                            "Realtime session configuration failed",
                        ));
                    }
                    if event["type"] == "session.updated" {
                        if let Some(turn_detection) =
                            event["session"]["audio"]["input"].get("turn_detection")
                            && settings.server_vad != (turn_detection["type"] == "server_vad")
                        {
                            bail!("Provider rejected realtime turn detection settings");
                        }
                        return Ok(());
                    }
                }
                Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await?,
                Some(Ok(Message::Close(_))) | None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "Realtime connection closed during setup",
                    )
                    .into());
                }
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| {
        eyre!(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Realtime session setup timed out"
        ))
    })??;

    // Coalesce device callbacks and pace setup backlogs at recording speed.
    // Some compatible gateways bound their queue by messages, not audio duration.
    const PACKET_BYTES: usize = 24_000; // 500 ms, PCM16 at 24 kHz
    let mut resampler = Pcm24k::default();
    let mut pending_pcm = VecDeque::new();
    let mut next_packet = tokio::time::Instant::now();
    let mut turn_samples = 0;
    let mut expected_commits = 0;
    let mut transcripts = TranscriptTurns::default();
    let mut audio_finished = false;
    let mut stopping_vad = false;
    let mut active_speech = None;
    let mut final_commits = None;
    let mut finish_deadline = None;
    loop {
        if let Some(expected) = final_commits {
            if transcripts.committed.len() > expected {
                bail!("Unexpected realtime commit");
            }
            if transcripts.committed.len() == expected && transcripts.all_completed() {
                let text = transcripts.final_text();
                let _ = socket.close(None).await;
                return nonempty(text);
            }
        }
        if finish_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            bail!("Realtime final transcript deadline expired");
        }
        if audio_finished && pending_pcm.is_empty() && !stopping_vad && final_commits.is_none() {
            finish_deadline =
                Some(tokio::time::Instant::now() + Duration::from_secs(settings.deadline_seconds));
            if settings.server_vad {
                // Drain the detector without synthetic silence. Its acknowledgement
                // follows all automatic commits; only an open speech turn needs commit.
                socket.send(Message::Text(json!({
                    "type": "session.update",
                    "session": {"type": "transcription", "audio": {"input": {"turn_detection": null}}}
                }).to_string().into())).await?;
                stopping_vad = true;
            } else {
                if turn_samples > 0 {
                    if turn_samples < 2400 {
                        socket.send(Message::Text(json!({
                            "type": "input_audio_buffer.append",
                            "audio": BASE64_STANDARD.encode(vec![0u8; (2400 - turn_samples) * 2])
                        }).to_string().into())).await?;
                    }
                    socket
                        .send(Message::Text(
                            json!({"type": "input_audio_buffer.commit"})
                                .to_string()
                                .into(),
                        ))
                        .await?;
                    expected_commits += 1;
                }
                final_commits = Some(expected_commits);
            }
        }
        tokio::select! {
            message = input.recv(), if !audio_finished && pending_pcm.len() < PACKET_BYTES => {
                let (samples, finish) = match message {
                    Some(AudioMessage::Samples(samples)) => (samples, false),
                    Some(AudioMessage::Finish(samples)) => (samples, true),
                    None => bail!("Realtime recording cancelled"),
                };
                pending_pcm.extend(resampler.push(&samples));
                if finish {
                    pending_pcm.extend(resampler.finish());
                    audio_finished = true;
                }
            }
            _ = tokio::time::sleep_until(next_packet), if pending_pcm.len() >= PACKET_BYTES || (audio_finished && !pending_pcm.is_empty()) => {
                let count = pending_pcm.len().min(PACKET_BYTES);
                let packet: Vec<u8> = pending_pcm.drain(..count).collect();
                socket.send(Message::Text(json!({
                    "type": "input_audio_buffer.append", "audio": BASE64_STANDARD.encode(&packet)
                }).to_string().into())).await?;
                // Never catch up with a burst after a slow send or handshake.
                next_packet = tokio::time::Instant::now() + Duration::from_secs_f64(count as f64 / 48_000.0);
                if !settings.server_vad {
                    turn_samples += count / 2;
                    if turn_samples >= 24_000 * 120 {
                        socket.send(Message::Text(json!({"type":"input_audio_buffer.commit"}).to_string().into())).await?;
                        expected_commits += 1;
                        turn_samples = 0;
                    }
                }
            }
            event = socket.next() => {
                match event {
                    Some(Ok(Message::Text(data))) => {
                        let event: Value = serde_json::from_str(&data)?;
                        match event["type"].as_str() {
                            Some("error" | "conversation.item.input_audio_transcription.failed") => return Err(provider_error(&event, "Realtime transcription failed")),
                            Some("input_audio_buffer.speech_started") => {
                                active_speech = Some(item_id(&event)?.to_owned());
                            }
                            Some("input_audio_buffer.committed") => {
                                let id = item_id(&event)?;
                                if active_speech.as_deref() == Some(id) { active_speech = None; }
                                transcripts.commit(id);
                            }
                            Some("session.updated") if stopping_vad && final_commits.is_none() => {
                                if event["session"]["audio"]["input"].get("turn_detection").is_some_and(|value| !value.is_null()) {
                                    continue;
                                }
                                let mut expected = transcripts.committed.len();
                                if active_speech.is_some() {
                                    socket.send(Message::Text(json!({"type":"input_audio_buffer.commit"}).to_string().into())).await?;
                                    expected += 1;
                                }
                                final_commits = Some(expected);
                            }
                            Some("conversation.item.input_audio_transcription.delta" | "conversation.item.input_audio_transcription.snapshot" | "conversation.item.input_audio_transcription.completed") => {
                                transcripts.update(&event, observe)?;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await?,
                    Some(Ok(Message::Close(_))) | None => return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "Realtime connection closed before final transcript").into()),
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)), if final_commits.is_some() || stopping_vad => {}
        }
    }
}

fn provider_error(event: &Value, description: &str) -> color_eyre::eyre::Report {
    // Expose diagnostic codes, not provider messages that can echo private input.
    let code = event["error"]["code"].as_str().unwrap_or_default();
    if !code.is_empty()
        && code.len() <= 80
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        eyre!("{description} ({code})")
    } else {
        eyre!("{description}")
    }
}

fn item_id(event: &Value) -> Result<&str> {
    event["item_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| eyre!("Realtime event has no item ID"))
}

#[derive(Default)]
struct TranscriptTurns {
    committed: Vec<String>,
    observed: Vec<String>,
    text: HashMap<String, (String, bool)>,
}

impl TranscriptTurns {
    fn commit(&mut self, id: &str) {
        if !self.committed.iter().any(|previous| previous == id) {
            self.committed.push(id.to_owned());
        }
    }

    fn update(&mut self, event: &Value, observe: &mut dyn FnMut(&str)) -> Result<()> {
        let id = item_id(event)?;
        if !self.text.contains_key(id) {
            self.observed.push(id.to_owned());
        }
        let (text, completed) = self.text.entry(id.to_owned()).or_default();
        if *completed {
            return Ok(());
        }
        if event["type"] == "conversation.item.input_audio_transcription.delta" {
            text.push_str(event["delta"].as_str().unwrap_or_default());
        } else {
            *text = event["transcript"].as_str().unwrap_or_default().to_owned();
            *completed = event["type"] == "conversation.item.input_audio_transcription.completed";
        }
        if self
            .text
            .values()
            .map(|(text, _)| text.len())
            .sum::<usize>()
            > MAX_RESPONSE_BYTES
        {
            bail!("Realtime transcript exceeds 2 MiB");
        }
        let preview = self
            .committed
            .iter()
            .chain(
                self.observed
                    .iter()
                    .filter(|id| !self.committed.contains(id)),
            )
            .filter_map(|id| self.text.get(id))
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        observe(&preview);
        Ok(())
    }

    fn all_completed(&self) -> bool {
        self.committed
            .iter()
            .all(|id| self.text.get(id).is_some_and(|(_, completed)| *completed))
    }

    fn final_text(&self) -> String {
        self.committed
            .iter()
            .filter_map(|id| self.text.get(id))
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Stateful linear interpolation from authoritative 16 kHz samples to PCM16 24 kHz.
#[derive(Default)]
struct Pcm24k {
    previous: Option<f32>,
    inputs: usize,
    outputs: usize,
}

impl Pcm24k {
    fn push(&mut self, samples: &[f32]) -> Vec<u8> {
        let mut output = Vec::with_capacity(samples.len() * 3);
        for &sample in samples {
            let index = self.inputs;
            self.inputs += 1;
            if let Some(previous) = self.previous {
                while self.outputs * 2 <= index * 3 {
                    let fraction = (self.outputs * 2 - (index - 1) * 3) as f32 / 3.0;
                    write_pcm(&mut output, previous + (sample - previous) * fraction);
                    self.outputs += 1;
                }
            } else {
                write_pcm(&mut output, sample);
                self.outputs += 1;
            }
            self.previous = Some(sample);
        }
        output
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        while self.outputs < self.inputs * 3 / 2 {
            write_pcm(&mut output, self.previous.unwrap_or_default());
            self.outputs += 1;
        }
        output
    }
}

fn write_pcm(output: &mut Vec<u8>, sample: f32) {
    output.extend_from_slice(&((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_queue_pressure_defers_audio_for_retry_without_cancelling() {
        let (audio, mut input) = channel(1);
        let (_sender, result) = mpsc::sync_channel(1);
        let mut session = RealtimeSession {
            audio,
            result,
            cancelled: Arc::new(AtomicBool::new(false)),
            offered_samples: 0,
        };
        let samples = (0..3200)
            .map(|index| index as f32 / 3200.0)
            .collect::<Vec<_>>();
        session.offer(&samples);
        assert_eq!(session.offered_samples(), 1600);
        assert!(!session.cancelled.load(Ordering::Acquire));
        let AudioMessage::Samples(first) = input.try_recv().unwrap() else {
            panic!("expected audio")
        };
        assert_eq!(first, samples[..1600]);
        session.offer(&samples[session.offered_samples()..]);
        let AudioMessage::Samples(second) = input.try_recv().unwrap() else {
            panic!("expected audio")
        };
        assert_eq!(second, samples[1600..]);
        assert_eq!(session.offered_samples(), samples.len());
    }

    #[test]
    fn realtime_connection_failure_preserves_worker_error_instead_of_claiming_queue_pressure() {
        let (audio, input) = channel(1);
        let (sender, result) = mpsc::sync_channel(1);
        drop(input);
        sender
            .send(Err("Realtime session configuration failed".into()))
            .unwrap();
        let mut session = RealtimeSession {
            audio,
            result,
            cancelled: Arc::new(AtomicBool::new(false)),
            offered_samples: 0,
        };
        session.offer(&[0.25; 1600]);
        let error = session
            .finish(
                &[0.25; 1600],
                &AtomicBool::new(false),
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "Realtime session configuration failed");
    }

    #[test]
    fn realtime_resampling_is_independent_of_audio_delivery_chunks() {
        let audio = (0..16001)
            .map(|index| (index as f32 * 0.01).sin())
            .collect::<Vec<_>>();
        let mut whole = Pcm24k::default();
        let mut expected = whole.push(&audio);
        expected.extend(whole.finish());
        let mut fragmented = Pcm24k::default();
        let mut actual = Vec::new();
        for chunk in audio.chunks(137) {
            actual.extend(fragmented.push(chunk));
        }
        actual.extend(fragmented.finish());
        assert_eq!(actual, expected);
        assert_eq!(actual.len() / 2, audio.len() * 3 / 2);
    }
}
