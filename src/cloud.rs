//! Outbound provider protocols. Audio capture and paste never run here.
mod realtime;
#[cfg(test)]
mod tests;
mod thinking;

use std::future::Future;
use std::io::{Cursor, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::{Engine, prelude::BASE64_STANDARD};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use reqwest::{Client, Response, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

pub use realtime::RealtimeSession;
pub use thinking::Thinking;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const AUDIO_CHUNK_SAMPLES: usize = 16_000 * 120;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ConnectionSettings {
    pub base_url: String,
    pub api_key_env: String,
    pub api_key_file: Option<PathBuf>,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            api_key_file: None,
        }
    }
}

impl ConnectionSettings {
    pub fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.base_url).wrap_err("Invalid provider base URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "Provider base URL must be an HTTP(S) URL without credentials, query, or fragment"
            );
        }
        if self.base_url.len() > 2048 || self.api_key_env.len() > 128 {
            bail!("Provider connection setting is too long");
        }
        if self
            .api_key_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            bail!("Provider key file path must be absolute");
        }
        Ok(())
    }

    fn endpoint(&self, route: &str) -> Result<Url> {
        self.validate()?;
        let mut url = Url::parse(&format!("{}/", self.base_url.trim_end_matches('/')))?;
        url = url.join(route)?;
        Ok(url)
    }

    fn api_key(&self) -> Result<String> {
        if let Some(path) = &self.api_key_file {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .wrap_err("Could not open provider key file")?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.len() > 16 * 1024
            {
                bail!("Provider key file must be an owner-only regular file, at most 16 KiB");
            }
            let mut key = String::new();
            file.take(16 * 1024 + 1).read_to_string(&mut key)?;
            return validated_key(key);
        }
        if self.api_key_env.is_empty() {
            // Explicitly empty credentials support local compatible servers.
            return Ok(String::new());
        }
        validated_key(
            std::env::var(&self.api_key_env)
                .map_err(|_| eyre!("Provider API key environment variable is not set"))?,
        )
    }
}

fn validated_key(key: String) -> Result<String> {
    let key = key.trim();
    if key.is_empty() || key.len() > 16 * 1024 || key.contains(['\r', '\n']) {
        bail!("Provider API key is empty or invalid");
    }
    Ok(key.into())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionMode {
    #[default]
    Batch,
    Streaming,
    Realtime,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TranscriptionSettings {
    pub connection: ConnectionSettings,
    pub model: String,
    pub language: String,
    pub prompt: String,
    pub mode: TranscriptionMode,
    pub deadline_seconds: u64,
    pub max_retries: u8,
}

impl Default for TranscriptionSettings {
    fn default() -> Self {
        Self {
            connection: ConnectionSettings::default(),
            model: "gpt-transcribe".into(),
            language: String::new(),
            prompt: String::new(),
            mode: TranscriptionMode::Batch,
            deadline_seconds: 120,
            max_retries: 2,
        }
    }
}

impl TranscriptionSettings {
    pub fn validate(&self) -> Result<()> {
        self.connection.validate()?;
        validate_model(&self.model, self.deadline_seconds)?;
        if self.max_retries > 10 {
            bail!("Realtime retries must be between 0 and 10");
        }
        if self.language.len() > 32 || self.prompt.len() > 16 * 1024 {
            bail!("Transcription language or recognition prompt is too long");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationProtocol {
    #[default]
    ChatCompletions,
    Responses,
    Gemini,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct GenerationSettings {
    pub connection: ConnectionSettings,
    pub protocol: GenerationProtocol,
    pub model: String,
    pub thinking: Thinking,
    pub stream: bool,
    pub deadline_seconds: u64,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            connection: ConnectionSettings::default(),
            protocol: GenerationProtocol::ChatCompletions,
            model: String::new(),
            thinking: Thinking::Default,
            stream: false,
            deadline_seconds: 30,
        }
    }
}

impl GenerationSettings {
    pub fn validate(&self) -> Result<()> {
        self.connection.validate()?;
        validate_model(&self.model, self.deadline_seconds)?;
        self.thinking.validate(self.protocol, &self.model)
    }
}

fn validate_model(model: &str, deadline: u64) -> Result<()> {
    if model.trim().is_empty() || model.len() > 256 || model.contains(['\r', '\n']) {
        bail!("Provider model ID is empty or invalid");
    }
    if !(1..=600).contains(&deadline) {
        bail!("Provider deadline must be between 1 and 600 seconds");
    }
    Ok(())
}

fn uses_language_list(model: &str) -> bool {
    ["gpt-transcribe", "gpt-live-transcribe"]
        .iter()
        .any(|name| model == *name || model.starts_with(&format!("{name}-")))
}

pub fn run_cancellable<T>(
    cancelled: &AtomicBool,
    deadline: Duration,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::select! {
                biased;
                _ = cancellation(cancelled) => bail!("Provider request cancelled"),
                result = tokio::time::timeout(deadline, operation) =>
                    result.map_err(|_| eyre!("Provider request deadline expired"))?,
            }
        })
}

async fn cancellation(cancelled: &AtomicBool) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn checked_response(response: Response) -> Result<Response> {
    if !response.status().is_success() {
        // Never echo provider bodies: they may contain credentials or private context.
        bail!(
            "Provider request failed (HTTP {})",
            response.status().as_u16()
        );
    }
    Ok(response)
}

async fn json_response(mut response: Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            bail!("Provider response exceeds 2 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).wrap_err("Provider returned invalid JSON")
}

pub fn transcribe(
    settings: &TranscriptionSettings,
    samples: &[f32],
    cancelled: &AtomicBool,
    mut observe: impl FnMut(&str),
) -> Result<String> {
    settings.validate()?;
    let client = http_client()?;
    // Chunk uploads, not capture: arbitrarily long dictations remain supported.
    let mut output = Vec::new();
    for samples in samples.chunks(AUDIO_CHUNK_SAMPLES) {
        let text = run_cancellable(
            cancelled,
            Duration::from_secs(settings.deadline_seconds),
            transcribe_chunk(&client, settings, samples, &mut observe),
        )?;
        if !text.trim().is_empty() {
            output.push(text.trim().to_owned());
        }
    }
    nonempty(output.join(" "))
}

async fn transcribe_chunk(
    client: &Client,
    settings: &TranscriptionSettings,
    samples: &[f32],
    observe: &mut dyn FnMut(&str),
) -> Result<String> {
    let audio = wav(samples)?;
    let mut form = multipart::Form::new()
        .part(
            "file",
            multipart::Part::bytes(audio)
                .file_name("dictation.wav")
                .mime_str("audio/wav")?,
        )
        .text("model", settings.model.clone());
    if !settings.language.is_empty() {
        let field = if uses_language_list(&settings.model) {
            "languages[]"
        } else {
            "language"
        };
        form = form.text(field, settings.language.clone());
    }
    if !settings.prompt.is_empty() {
        form = form.text("prompt", settings.prompt.clone());
    }
    let streaming = settings.mode == TranscriptionMode::Streaming;
    form = if streaming {
        form.text("stream", "true")
    } else {
        form.text("response_format", "json")
    };
    let mut request = client
        .post(settings.connection.endpoint("audio/transcriptions")?)
        .multipart(form);
    let key = settings.connection.api_key()?;
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = checked_response(request.send().await?).await?;
    if streaming {
        stream_response(response, StreamProtocol::Audio, observe).await
    } else {
        let body = json_response(response).await?;
        nonempty(body["text"].as_str().unwrap_or_default().into())
    }
}

fn wav(samples: &[f32]) -> Result<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(
            &mut bytes,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        for sample in samples {
            writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(bytes.into_inner())
}

/// A bounded PNG/JPEG captured for this recording, never a remote image URL.
#[derive(Clone)]
pub struct ContextImage {
    pub mime_type: &'static str,
    pub bytes: Vec<u8>,
}

pub fn generate(
    settings: &GenerationSettings,
    instructions: &str,
    input: &str,
    image: Option<&ContextImage>,
    cancelled: &AtomicBool,
) -> Result<String> {
    settings.validate()?;
    if image.is_some_and(|image| image.bytes.len() > 4 * 1024 * 1024) {
        bail!("Window screenshot exceeds 4 MiB");
    }
    run_cancellable(
        cancelled,
        Duration::from_secs(settings.deadline_seconds),
        async {
            let (route, body) = generation_request(settings, instructions, input, image);
            let mut request = http_client()?
                .post(settings.connection.endpoint(&route)?)
                .json(&body);
            let key = settings.connection.api_key()?;
            if !key.is_empty() {
                request = if settings.protocol == GenerationProtocol::Gemini {
                    request.header("x-goog-api-key", key)
                } else {
                    request.bearer_auth(key)
                };
            }
            let response = checked_response(request.send().await?).await?;
            let protocol = StreamProtocol::from(settings.protocol);
            if settings.stream {
                stream_response(response, protocol, &mut |_| {}).await
            } else {
                final_generation(&json_response(response).await?, protocol)
            }
        },
    )
}

fn generation_request(
    settings: &GenerationSettings,
    instructions: &str,
    input: &str,
    image: Option<&ContextImage>,
) -> (String, Value) {
    let image_data = image.map(|image| BASE64_STANDARD.encode(&image.bytes));
    let image_url = image
        .zip(image_data.as_ref())
        .map(|(image, data)| format!("data:{};base64,{data}", image.mime_type));
    let (route, mut body) = match settings.protocol {
        GenerationProtocol::ChatCompletions => {
            let mut content = vec![json!({"type": "text", "text": input})];
            if let Some(url) = image_url {
                content.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            (
                "chat/completions".into(),
                json!({
                    "model": settings.model, "stream": settings.stream,
                    "messages": [
                        {"role": "system", "content": instructions},
                        {"role": "user", "content": content}
                    ]
                }),
            )
        }
        GenerationProtocol::Responses => {
            let mut content = vec![json!({"type": "input_text", "text": input})];
            if let Some(url) = image_url {
                content.push(json!({"type": "input_image", "image_url": url}));
            }
            (
                "responses".into(),
                json!({
                    "model": settings.model, "stream": settings.stream, "store": false,
                    "instructions": instructions,
                    "input": [{"role": "user", "content": content}]
                }),
            )
        }
        GenerationProtocol::Gemini => {
            let mut parts = vec![json!({"text": input})];
            if let Some((image, data)) = image.zip(image_data) {
                parts.push(json!({"inlineData": {"mimeType": image.mime_type, "data": data}}));
            }
            let method = if settings.stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            let model = url::form_urlencoded::byte_serialize(
                settings.model.trim_start_matches("models/").as_bytes(),
            )
            .collect::<String>();
            (
                format!("models/{model}:{method}"),
                json!({
                    "systemInstruction": {"parts": [{"text": instructions}]},
                    "contents": [{"role": "user", "parts": parts}]
                }),
            )
        }
    };
    if settings.thinking != Thinking::Default {
        match settings.protocol {
            GenerationProtocol::ChatCompletions => {
                body["reasoning_effort"] = json!(settings.thinking.effort());
            }
            GenerationProtocol::Responses => {
                body["reasoning"] = json!({"effort": settings.thinking.effort()});
            }
            GenerationProtocol::Gemini => {
                body["generationConfig"] =
                    json!({"thinkingConfig": settings.thinking.gemini_config()});
            }
        }
    }
    (route, body)
}

fn nonempty(text: String) -> Result<String> {
    if text.trim().is_empty() {
        bail!("Provider returned no text");
    }
    Ok(text)
}

#[derive(Clone, Copy)]
enum StreamProtocol {
    Audio,
    Chat,
    Responses,
    Gemini,
}

impl From<GenerationProtocol> for StreamProtocol {
    fn from(protocol: GenerationProtocol) -> Self {
        match protocol {
            GenerationProtocol::ChatCompletions => Self::Chat,
            GenerationProtocol::Responses => Self::Responses,
            GenerationProtocol::Gemini => Self::Gemini,
        }
    }
}

fn final_generation(body: &Value, protocol: StreamProtocol) -> Result<String> {
    if !body["error"].is_null() {
        bail!("Provider returned an error");
    }
    let text = match protocol {
        StreamProtocol::Chat => {
            let choice = &body["choices"][0];
            if choice["finish_reason"].as_str() != Some("stop")
                || !choice["message"]["refusal"].is_null()
            {
                bail!("Provider did not complete cleanup");
            }
            choice["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        }
        StreamProtocol::Responses => {
            if body["status"].as_str() != Some("completed") {
                bail!("Provider did not complete cleanup");
            }
            body["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "message" && item["role"] == "assistant")
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter(|part| part["type"] == "output_text")
                .filter_map(|part| part["text"].as_str())
                .collect()
        }
        StreamProtocol::Gemini => {
            let candidate = &body["candidates"][0];
            if candidate["finishReason"].as_str() != Some("STOP") {
                bail!("Provider did not complete cleanup");
            }
            gemini_text(candidate)
        }
        StreamProtocol::Audio => body["text"].as_str().unwrap_or_default().into(),
    };
    nonempty(text)
}

fn gemini_text(candidate: &Value) -> String {
    candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["thought"] != true)
        .filter_map(|part| part["text"].as_str())
        .collect()
}

struct TranscriptStream {
    protocol: StreamProtocol,
    text: String,
    complete: bool,
}

impl TranscriptStream {
    fn new(protocol: StreamProtocol) -> Self {
        Self {
            protocol,
            text: String::new(),
            complete: false,
        }
    }

    fn event(&mut self, data: &[u8]) -> Result<()> {
        if data == b"[DONE]" {
            if !self.complete {
                bail!("Provider stream ended without successful completion");
            }
            return Ok(());
        }
        let event: Value =
            serde_json::from_slice(data).wrap_err("Invalid provider stream event")?;
        if !event["error"].is_null() || event["type"] == "error" {
            bail!("Provider stream returned an error");
        }
        match self.protocol {
            StreamProtocol::Audio => match event["type"].as_str() {
                Some("transcript.text.delta") => self
                    .text
                    .push_str(event["delta"].as_str().unwrap_or_default()),
                Some("transcript.text.done") => {
                    self.text = event["text"].as_str().unwrap_or_default().into();
                    self.complete = true;
                }
                _ => {}
            },
            StreamProtocol::Chat => {
                let choice = &event["choices"][0];
                if !choice["delta"]["refusal"].is_null() {
                    bail!("Provider refused cleanup");
                }
                self.text
                    .push_str(choice["delta"]["content"].as_str().unwrap_or_default());
                if let Some(reason) = choice["finish_reason"].as_str() {
                    if reason != "stop" {
                        bail!("Provider did not complete cleanup");
                    }
                    self.complete = true;
                }
            }
            StreamProtocol::Responses => match event["type"].as_str() {
                Some("response.output_text.delta") => self
                    .text
                    .push_str(event["delta"].as_str().unwrap_or_default()),
                Some("response.completed") => {
                    self.text = final_generation(&event["response"], StreamProtocol::Responses)?;
                    self.complete = true;
                }
                Some("response.failed" | "response.incomplete" | "response.refusal.delta") => {
                    bail!("Provider did not complete cleanup")
                }
                _ => {}
            },
            StreamProtocol::Gemini => {
                let candidate = &event["candidates"][0];
                self.text.push_str(&gemini_text(candidate));
                if let Some(reason) = candidate["finishReason"].as_str() {
                    if reason != "STOP" {
                        bail!("Provider did not complete cleanup");
                    }
                    self.complete = true;
                }
            }
        }
        if self.text.len() > MAX_RESPONSE_BYTES {
            bail!("Provider transcript exceeds 2 MiB");
        }
        Ok(())
    }
}

/// SSE framing independent of TCP chunks, including CR, LF, and CRLF.
#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}

impl SseDecoder {
    fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>> {
        if byte == b'\n' && self.after_cr {
            self.after_cr = false;
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        if byte != b'\n' && byte != b'\r' {
            if self.line.len() + self.data.len() >= MAX_EVENT_BYTES {
                bail!("Provider stream event is too large");
            }
            self.line.push(byte);
            return Ok(None);
        }
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            return Ok((!self.data.is_empty()).then(|| {
                self.data.pop();
                std::mem::take(&mut self.data)
            }));
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            self.data
                .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            self.data.push(b'\n');
        }
        Ok(None)
    }
}

async fn stream_response(
    mut response: Response,
    protocol: StreamProtocol,
    observe: &mut dyn FnMut(&str),
) -> Result<String> {
    let mut decoder = SseDecoder::default();
    let mut transcript = TranscriptStream::new(protocol);
    while let Some(chunk) = response.chunk().await? {
        for byte in chunk {
            if let Some(data) = decoder.push(byte)? {
                transcript.event(&data)?;
                observe(&transcript.text);
                if transcript.complete {
                    return nonempty(transcript.text);
                }
            }
        }
    }
    bail!("Provider stream disconnected before completion")
}
