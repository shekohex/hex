use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use base64::{Engine, prelude::BASE64_STANDARD};
use color_eyre::eyre::{Result, bail, eyre};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Sender, channel};
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
    admission_failed: bool,
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
            admission_failed: false,
        }
    }

    pub fn offer(&mut self, samples: &[f32]) {
        if self.admission_failed {
            return;
        }
        for chunk in samples.chunks(1600) {
            if self
                .audio
                .try_send(AudioMessage::Samples(chunk.to_vec()))
                .is_err()
            {
                self.admission_failed = true;
                self.cancelled.store(true, Ordering::Release);
                return;
            }
            self.offered_samples += chunk.len();
        }
    }

    pub fn offered_samples(&self) -> usize {
        self.offered_samples
    }

    /// Called by the output worker, never by the microphone loop.
    pub fn finish(
        self,
        samples: &[f32],
        shutdown: &AtomicBool,
        deadline: Duration,
    ) -> Result<String> {
        if self.admission_failed {
            bail!("Realtime audio queue overflowed or connection failed; nothing pasted");
        }
        if samples.len() < self.offered_samples {
            bail!("Realtime capture boundary changed; nothing pasted");
        }
        let tail = samples[self.offered_samples..].to_vec();
        let audio = self.audio.clone();
        run_cancellable(shutdown, deadline, async {
            audio
                .send(AudioMessage::Finish(tail))
                .await
                .map_err(|_| eyre!("Realtime connection closed"))?;
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
    url.query_pairs_mut().append_pair("intent", "transcription");
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
    .map_err(|_| eyre!("Realtime connection timed out"))??;
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
                        "turn_detection": null
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
                        bail!("Realtime session configuration failed");
                    }
                    if event["type"] == "session.updated" {
                        return Ok(());
                    }
                }
                Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await?,
                Some(Ok(Message::Close(_))) | None => {
                    bail!("Realtime connection closed during setup")
                }
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| eyre!("Realtime session setup timed out"))??;

    let mut resampler = Pcm24k::default();
    let mut turn_samples = 0;
    let mut expected_commits = 0;
    let mut turns = Vec::new();
    let mut transcripts = HashMap::new();
    let mut finishing = false;
    let mut finish_deadline = None;
    loop {
        if finishing
            && turns.len() == expected_commits
            && turns.iter().all(|id| transcripts.contains_key(id))
        {
            let text = turns
                .iter()
                .filter_map(|id| transcripts.get(id))
                .cloned()
                .collect::<Vec<String>>()
                .join(" ");
            let _ = socket.close(None).await;
            return nonempty(text);
        }
        if finish_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            bail!("Realtime final transcript deadline expired");
        }
        tokio::select! {
            message = input.recv(), if !finishing => {
                let (samples, finish) = match message {
                    Some(AudioMessage::Samples(samples)) => (samples, false),
                    Some(AudioMessage::Finish(samples)) => (samples, true),
                    None => bail!("Realtime recording cancelled"),
                };
                let mut pcm = resampler.push(&samples);
                if finish { pcm.extend(resampler.finish()); }
                // Commit bounded turns while recording; keep finals in commit order.
                for chunk in pcm.chunks(24_000 * 2 / 10) {
                    socket.send(Message::Text(json!({
                        "type": "input_audio_buffer.append",
                        "audio": BASE64_STANDARD.encode(chunk)
                    }).to_string().into())).await?;
                    turn_samples += chunk.len() / 2;
                    if turn_samples >= 24_000 * 120 {
                        socket.send(Message::Text(json!({"type":"input_audio_buffer.commit"}).to_string().into())).await?;
                        expected_commits += 1;
                        turn_samples = 0;
                    }
                }
                if finish {
                    // OpenAI requires at least 100 ms in a committed audio buffer.
                    if turn_samples > 0 && turn_samples < 2400 {
                        socket.send(Message::Text(json!({
                            "type": "input_audio_buffer.append",
                            "audio": BASE64_STANDARD.encode(vec![0u8; (2400 - turn_samples) * 2])
                        }).to_string().into())).await?;
                    }
                    if turn_samples > 0 {
                        socket.send(Message::Text(json!({"type":"input_audio_buffer.commit"}).to_string().into())).await?;
                        expected_commits += 1;
                    }
                    finishing = true;
                    finish_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(settings.deadline_seconds));
                }
            }
            event = socket.next() => {
                match event {
                    Some(Ok(Message::Text(data))) => {
                        let event: Value = serde_json::from_str(&data)?;
                        match event["type"].as_str() {
                            Some("error" | "conversation.item.input_audio_transcription.failed") => bail!("Realtime transcription failed"),
                            Some("input_audio_buffer.committed") => {
                                let id = event["item_id"].as_str().ok_or_else(|| eyre!("Realtime commit has no item ID"))?;
                                if !turns.iter().any(|previous| previous == id) { turns.push(id.to_owned()); }
                                if turns.len() > expected_commits { bail!("Unexpected realtime commit"); }
                            }
                            Some("conversation.item.input_audio_transcription.delta") => {
                                if let Some(delta) = event["delta"].as_str() { observe(delta); }
                            }
                            Some("conversation.item.input_audio_transcription.completed") => {
                                let id = event["item_id"].as_str().ok_or_else(|| eyre!("Realtime transcript has no item ID"))?;
                                let text = event["transcript"].as_str().unwrap_or_default().to_owned();
                                transcripts.insert(id.to_owned(), text);
                                if transcripts.len() > expected_commits { bail!("Unexpected realtime transcript"); }
                                if transcripts.values().map(String::len).sum::<usize>() > MAX_RESPONSE_BYTES {
                                    bail!("Realtime transcript exceeds 2 MiB");
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await?,
                    Some(Ok(Message::Close(_))) | None => bail!("Realtime connection closed before final transcript"),
                    Some(Err(_)) => bail!("Realtime connection failed"),
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)), if finishing => {}
        }
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
    fn realtime_queue_pressure_cancels_instead_of_dropping_audio() {
        let (audio, _input) = channel(1);
        let (_sender, result) = mpsc::sync_channel(1);
        let mut session = RealtimeSession {
            audio,
            result,
            cancelled: Arc::new(AtomicBool::new(false)),
            offered_samples: 0,
            admission_failed: false,
        };
        session.offer(&vec![0.25; 3200]);
        assert!(session.cancelled.load(Ordering::Acquire));
        assert!(
            session
                .finish(
                    &vec![0.25; 3200],
                    &AtomicBool::new(false),
                    Duration::from_secs(1)
                )
                .unwrap_err()
                .to_string()
                .contains("queue overflowed")
        );
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
