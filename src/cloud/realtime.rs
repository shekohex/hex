use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

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
    result: Receiver<Result<String, SessionFailure>>,
    settings: TranscriptionSettings,
    cancelled: Arc<AtomicBool>,
    offered_samples: usize,
}

#[derive(Debug)]
struct SessionFailure {
    message: String,
    retryable: bool,
}
impl std::fmt::Display for SessionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for SessionFailure {}
impl From<&str> for SessionFailure {
    fn from(message: &str) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

pub struct RealtimeOutcome {
    pub result: Result<String>,
    pub retries: u8,
}

fn retryable(error: &color_eyre::eyre::Report) -> bool {
    use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
    if let Some(failure) = error.downcast_ref::<SessionFailure>() {
        return failure.retryable;
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        return matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::ConnectionRefused
        );
    }
    match error.downcast_ref::<Error>() {
        Some(Error::Http(response)) => {
            response.status().as_u16() == 429 || response.status().is_server_error()
        }
        Some(
            Error::Io(_)
            | Error::Tls(_)
            | Error::ConnectionClosed
            | Error::AlreadyClosed
            | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake),
        ) => true,
        _ => false,
    }
}

impl RealtimeSession {
    pub fn start(
        settings: TranscriptionSettings,
        shutdown: Arc<AtomicBool>,
        mut observe: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        let retry_settings = settings.clone();
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
            .map_err(|error| SessionFailure {
                retryable: retryable(&error),
                message: error.to_string(),
            });
            let _ = sender.try_send(result);
        });
        Self {
            audio,
            result,
            settings: retry_settings,
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

    #[cfg(test)]
    pub fn finish(
        self,
        samples: &[f32],
        shutdown: &AtomicBool,
        deadline: Duration,
    ) -> Result<String> {
        self.finish_with_metrics(samples, shutdown, deadline).result
    }

    /// Called by the output worker, never by the microphone loop.
    pub fn finish_with_metrics(
        self,
        samples: &[f32],
        shutdown: &AtomicBool,
        deadline: Duration,
    ) -> RealtimeOutcome {
        let settings = self.settings.clone();
        let started = Instant::now();
        let mut initial = Some(self);
        let mut attempted_retries = 0;
        let result = (|| -> Result<String> {
            for retries in 0..=settings.max_retries {
                let remaining = deadline.saturating_sub(started.elapsed());
                if shutdown.load(Ordering::Acquire) {
                    bail!("Dictation cancelled");
                }
                if remaining.is_zero() {
                    bail!("Realtime final transcript deadline expired");
                }
                attempted_retries = retries;
                let session = initial.take().unwrap_or_else(|| {
                    Self::start(settings.clone(), Arc::new(AtomicBool::new(false)), |_| {})
                });
                match session.finish_attempt(samples, shutdown, remaining) {
                    Ok(text) => return Ok(text),
                    Err(error) if retries < settings.max_retries && retryable(&error) => {
                        tracing::warn!(
                            retry = retries + 1,
                            limit = settings.max_retries,
                            "retrying realtime connection with retained audio"
                        );
                        let delay = Duration::from_millis(250 * (1u64 << retries.min(3)));
                        run_cancellable(
                            shutdown,
                            deadline.saturating_sub(started.elapsed()),
                            async {
                                tokio::time::sleep(delay).await;
                                Ok(())
                            },
                        )?;
                    }
                    Err(error) => return Err(error),
                }
            }
            unreachable!("bounded retry loop always returns")
        })();
        RealtimeOutcome {
            result,
            retries: attempted_retries,
        }
    }

    fn finish_attempt(
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
                    Some(Ok(Message::Close(_))) | None => return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "Realtime connection closed before final transcript").into()),
                    Some(Err(error)) => return Err(error.into()),
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
    fn realtime_queue_pressure_defers_audio_for_retry_without_cancelling() {
        let (audio, mut input) = channel(1);
        let (_sender, result) = mpsc::sync_channel(1);
        let mut session = RealtimeSession {
            audio,
            result,
            settings: Default::default(),
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
            settings: Default::default(),
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

    fn accept_with_deadline(listener: &std::net::TcpListener) -> std::net::TcpStream {
        listener.set_nonblocking(true).unwrap();
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(started.elapsed() < Duration::from_secs(5), "missing retry");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn fixture_settings(address: std::net::SocketAddr, max_retries: u8) -> TranscriptionSettings {
        TranscriptionSettings {
            connection: super::super::ConnectionSettings {
                base_url: format!("http://{address}/v1"),
                api_key_env: String::new(),
                api_key_file: None,
            },
            model: "fixture".into(),
            mode: super::super::TranscriptionMode::Realtime,
            max_retries,
            ..Default::default()
        }
    }

    #[test]
    fn reconnect_replays_full_recording_after_partial_audio_disconnect() {
        use tokio_tungstenite::tungstenite::accept;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let settings = fixture_settings(listener.local_addr().unwrap(), 2);
        let (disconnected, partial_seen) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let mut first = accept(accept_with_deadline(&listener)).unwrap();
            first.read().unwrap();
            first
                .send(Message::text(r#"{"type":"session.updated"}"#))
                .unwrap();
            let event: Value =
                serde_json::from_str(first.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(event["type"], "input_audio_buffer.append");
            drop(first);
            disconnected.send(()).unwrap();
            let mut second = accept(accept_with_deadline(&listener)).unwrap();
            second.read().unwrap();
            second
                .send(Message::text(r#"{"type":"session.updated"}"#))
                .unwrap();
            let mut received = Vec::new();
            loop {
                let event: Value =
                    serde_json::from_str(second.read().unwrap().to_text().unwrap()).unwrap();
                match event["type"].as_str().unwrap() {
                    "input_audio_buffer.append" => received.extend(
                        BASE64_STANDARD
                            .decode(event["audio"].as_str().unwrap())
                            .unwrap(),
                    ),
                    "input_audio_buffer.commit" => {
                        second
                            .send(Message::text(
                                r#"{"type":"input_audio_buffer.committed","item_id":"retry-1"}"#,
                            ))
                            .unwrap();
                        second.send(Message::text(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"retry-1","transcript":"Recovered."}"#)).unwrap();
                        assert!(
                            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
                        );
                        return received;
                    }
                    _ => panic!("unexpected event"),
                }
            }
        });
        let audio = (0..16_001)
            .map(|index| (index as f32 * 0.01).sin())
            .collect::<Vec<_>>();
        let mut session =
            RealtimeSession::start(settings, Arc::new(AtomicBool::new(false)), |_| {});
        session.offer(&audio[..1600]);
        partial_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        let outcome =
            session.finish_with_metrics(&audio, &AtomicBool::new(false), Duration::from_secs(5));
        assert_eq!(outcome.result.unwrap(), "Recovered.");
        assert_eq!(outcome.retries, 1);
        let mut pcm = Pcm24k::default();
        let mut expected = pcm.push(&audio);
        expected.extend(pcm.finish());
        assert_eq!(server.join().unwrap(), expected);
    }

    fn rejected_connections(
        status: u16,
        max_retries: u8,
        expected_attempts: usize,
        deadline: Duration,
        cancel: bool,
    ) -> RealtimeOutcome {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let settings = fixture_settings(listener.local_addr().unwrap(), max_retries);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..expected_attempts {
                let mut stream = accept_with_deadline(&listener);
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
            }
            if cancel {
                server_shutdown.store(true, Ordering::Release);
            }
            // Keep endpoint alive while the client completes, to detect extra attempts.
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        });
        let outcome = RealtimeSession::start(settings, shutdown.clone(), |_| {})
            .finish_with_metrics(&[0.25; 1600], &shutdown, deadline);
        server.join().unwrap();
        outcome
    }

    #[test]
    fn transient_connection_retries_respect_zero_and_configured_limit() {
        for retries in [0, 2] {
            let outcome = rejected_connections(
                503,
                retries,
                usize::from(retries) + 1,
                Duration::from_secs(5),
                false,
            );
            assert!(outcome.result.is_err());
            assert_eq!(outcome.retries, retries);
        }
    }

    #[test]
    fn rejected_credentials_never_retry() {
        let outcome = rejected_connections(401, 2, 1, Duration::from_secs(5), false);
        assert!(outcome.result.is_err());
        assert_eq!(outcome.retries, 0);
    }

    #[test]
    fn cancellation_and_shared_deadline_stop_backoff_before_another_attempt() {
        for cancel in [false, true] {
            let started = Instant::now();
            let outcome = rejected_connections(429, 10, 1, Duration::from_millis(100), cancel);
            assert!(outcome.result.is_err());
            assert_eq!(outcome.retries, 0);
            assert!(started.elapsed() < Duration::from_secs(1));
        }
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
