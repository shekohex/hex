use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::*;

struct RecordedRequest {
    headers: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break offset + 4;
        }
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or_default();
    while bytes.len() < header_end + length {
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    RecordedRequest {
        headers,
        body: bytes[header_end..header_end + length].to_vec(),
    }
}

fn server(
    body: String,
    streaming: bool,
) -> (
    ConnectionSettings,
    mpsc::Receiver<RecordedRequest>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, request) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = sender.send(read_request(&mut stream));
        if streaming {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            // Deliberately split UTF-8, SSE lines, JSON, and events across transport chunks.
            for chunk in body.as_bytes().chunks(7) {
                write!(stream, "{:x}\r\n", chunk.len()).unwrap();
                stream.write_all(chunk).unwrap();
                stream.write_all(b"\r\n").unwrap();
            }
            stream.flush().unwrap();
            // No chunked terminator: client must complete on protocol event, not EOF.
            let mut byte = [0];
            let _ = stream.read(&mut byte);
        } else {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (
        ConnectionSettings {
            base_url: format!("http://{address}/custom/v1"),
            api_key_env: String::new(),
            api_key_file: None,
        },
        request,
        worker,
    )
}

#[test]
fn recorded_audio_uses_multipart_pcm_without_backend_padding() {
    let (connection, request, worker) = server(json!({"text":"مرحبا HEX"}).to_string(), false);
    let settings = TranscriptionSettings {
        connection,
        model: "whisper-1".into(),
        language: "ar".into(),
        prompt: "HEX".into(),
        ..Default::default()
    };
    assert_eq!(
        transcribe(
            &settings,
            &vec![0.25; 16_000],
            &AtomicBool::new(false),
            |_| {}
        )
        .unwrap(),
        "مرحبا HEX"
    );
    let request = request.recv().unwrap();
    assert!(
        request
            .headers
            .starts_with("POST /custom/v1/audio/transcriptions ")
    );
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("authorization")
    );
    let start = request
        .body
        .windows(4)
        .position(|bytes| bytes == b"RIFF")
        .unwrap();
    let mut wav = hound::WavReader::new(Cursor::new(&request.body[start..])).unwrap();
    assert_eq!(wav.spec().sample_rate, 16_000);
    assert_eq!(wav.samples::<i16>().count(), 16_000);
    assert!(String::from_utf8_lossy(&request.body).contains("name=\"language\"\r\n\r\nar"));
    worker.join().unwrap();
}

#[test]
fn current_transcription_models_send_language_lists_not_legacy_language() {
    let (connection, request, worker) = server(json!({"text":"مرحبا"}).to_string(), false);
    let settings = TranscriptionSettings {
        connection,
        language: "ar".into(),
        ..Default::default()
    };
    transcribe(&settings, &[0.25; 1600], &AtomicBool::new(false), |_| {}).unwrap();
    let request = request.recv().unwrap();
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("name=\"languages[]\"\r\n\r\nar"));
    assert!(!body.contains("name=\"language\""));
    worker.join().unwrap();
}

#[test]
fn file_streaming_uses_final_transcript_without_waiting_for_eof() {
    let body = concat!(
        ": comment\r\n\r\n",
        "data: {\"type\":\"transcript.text.delta\",\"delta\":\"draft\"}\r\n\r\n",
        "data: {\"type\":\"transcript.text.done\",\"text\":\"Final café.\"}\r\n\r\n"
    );
    let (connection, _, worker) = server(body.into(), true);
    let settings = TranscriptionSettings {
        connection,
        mode: TranscriptionMode::Streaming,
        ..Default::default()
    };
    let started = Instant::now();
    let mut observations = Vec::new();
    assert_eq!(
        transcribe(
            &settings,
            &vec![0.25; 8000],
            &AtomicBool::new(false),
            |text| observations.push(text.to_owned())
        )
        .unwrap(),
        "Final café."
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(observations, ["draft", "Final café."]);
    worker.join().unwrap();
}

#[test]
fn thinking_controls_use_native_wire_fields_and_keep_default_requests_unchanged() {
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Responses,
        GenerationProtocol::Gemini,
    ] {
        let settings = GenerationSettings {
            protocol,
            model: "cleanup".into(),
            ..Default::default()
        };
        let (_, body) = generation_request(&settings, "instructions", "text", None);
        for key in ["reasoning_effort", "reasoning", "generationConfig"] {
            assert!(body.get(key).is_none());
        }
    }

    for (protocol, model, thinking, field, expected) in [
        (
            GenerationProtocol::ChatCompletions,
            "cleanup",
            Thinking::Off,
            "/reasoning_effort",
            json!("none"),
        ),
        (
            GenerationProtocol::Responses,
            "cleanup",
            Thinking::XHigh,
            "/reasoning/effort",
            json!("xhigh"),
        ),
        (
            GenerationProtocol::Responses,
            "cleanup",
            Thinking::Max,
            "/reasoning/effort",
            json!("max"),
        ),
        (
            GenerationProtocol::Gemini,
            "gemini-flash-latest",
            Thinking::Low,
            "/generationConfig/thinkingConfig",
            json!({"thinkingLevel":"low"}),
        ),
        (
            GenerationProtocol::Gemini,
            "models/gemini-2.5-flash",
            Thinking::Off,
            "/generationConfig/thinkingConfig",
            json!({"thinkingBudget":0}),
        ),
        (
            GenerationProtocol::Gemini,
            "gemini-2.5-pro",
            Thinking::Budget(1024),
            "/generationConfig/thinkingConfig",
            json!({"thinkingBudget":1024}),
        ),
    ] {
        for streaming in [false, true] {
            let response = match protocol {
                GenerationProtocol::ChatCompletions => {
                    json!({"choices":[{"finish_reason":"stop","message":{"content":"Clean."},"delta":{"content":"Clean."}}]})
                }
                GenerationProtocol::Responses => {
                    json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Clean."}]}]})
                }
                GenerationProtocol::Gemini => {
                    json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"Clean."}]}}]})
                }
            };
            let payload = if !streaming {
                response.to_string()
            } else if protocol == GenerationProtocol::Responses {
                format!(
                    "data: {}\n\n",
                    json!({"type":"response.completed","response":response})
                )
            } else {
                format!("data: {response}\n\ndata: [DONE]\n\n")
            };
            let (connection, request, worker) = server(payload, streaming);
            let settings = GenerationSettings {
                connection,
                protocol,
                model: model.into(),
                thinking,
                stream: streaming,
                ..Default::default()
            };
            assert_eq!(
                generate(
                    &settings,
                    "instructions",
                    "text",
                    None,
                    &AtomicBool::new(false)
                )
                .unwrap(),
                "Clean."
            );
            let body: Value = serde_json::from_slice(&request.recv().unwrap().body).unwrap();
            assert_eq!(body.pointer(field), Some(&expected));
            worker.join().unwrap();
        }
    }
}

#[test]
fn thinking_choices_reject_incompatible_gemini_levels_and_budgets() {
    for (model, thinking) in [
        ("gemini-2.5-flash", Thinking::Low),
        ("gemini-2.5-flash", Thinking::Budget(24577)),
        ("gemini-2.5-flash-lite", Thinking::Budget(511)),
        ("gemini-2.5-pro", Thinking::Off),
        ("gemini-2.5-pro", Thinking::Budget(127)),
        ("gemini-3-flash", Thinking::Off),
        ("gemini-3-flash", Thinking::Max),
        ("gemini-3-flash", Thinking::Budget(1024)),
        ("gemini-3.8-flash", Thinking::Minimal),
        ("gemini-3.1-pro-preview", Thinking::Medium),
    ] {
        assert!(
            thinking
                .validate(GenerationProtocol::Gemini, model)
                .is_err(),
            "{model}: {thinking:?}"
        );
    }
    assert!(
        Thinking::Budget(1024)
            .validate(GenerationProtocol::Responses, "cleanup")
            .is_err()
    );
    for (thinking, expected) in [
        (Thinking::Off, "none"),
        (Thinking::Minimal, "minimal"),
        (Thinking::Low, "low"),
        (Thinking::Medium, "medium"),
        (Thinking::High, "high"),
        (Thinking::XHigh, "xhigh"),
        (Thinking::Max, "max"),
    ] {
        assert!(
            thinking
                .validate(GenerationProtocol::ChatCompletions, "gateway-model")
                .is_ok()
        );
        let settings = GenerationSettings {
            model: "gateway-model".into(),
            thinking,
            ..Default::default()
        };
        assert_eq!(
            generation_request(&settings, "", "", None).1["reasoning_effort"],
            expected
        );
    }
}

#[test]
fn cleanup_protocols_send_instructions_text_and_images_in_native_formats() {
    for (protocol, route, response) in [
        (
            GenerationProtocol::ChatCompletions,
            "chat/completions",
            json!({"choices":[{"finish_reason":"stop","message":{"content":"Clean."}}]}),
        ),
        (
            GenerationProtocol::Responses,
            "responses",
            json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Clean."}]}]}),
        ),
        (
            GenerationProtocol::Gemini,
            "models/cleanup:generateContent",
            json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"hidden","thought":true},{"text":"Clean."}]}}]}),
        ),
    ] {
        let (connection, request, worker) = server(response.to_string(), false);
        let settings = GenerationSettings {
            connection,
            protocol,
            model: "cleanup".into(),
            ..Default::default()
        };
        let image = ContextImage {
            mime_type: "image/png",
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            generate(
                &settings,
                "Do not answer questions.",
                "<TRANSCRIPT>text</TRANSCRIPT>",
                Some(&image),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "Clean."
        );
        let request = request.recv().unwrap();
        assert!(
            request
                .headers
                .starts_with(&format!("POST /custom/v1/{route} "))
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        match protocol {
            GenerationProtocol::ChatCompletions => {
                assert_eq!(body["messages"][0]["content"], "Do not answer questions.");
                assert_eq!(
                    body["messages"][1]["content"][1]["image_url"]["url"],
                    "data:image/png;base64,AQID"
                );
            }
            GenerationProtocol::Responses => {
                assert_eq!(body["instructions"], "Do not answer questions.");
                assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
                assert_eq!(body["store"], false);
            }
            GenerationProtocol::Gemini => {
                assert_eq!(
                    body["systemInstruction"]["parts"][0]["text"],
                    "Do not answer questions."
                );
                assert_eq!(
                    body["contents"][0]["parts"][1]["inlineData"]["data"],
                    "AQID"
                );
            }
        }
        worker.join().unwrap();
    }
}

#[test]
fn cleanup_streams_require_successful_protocol_completion_and_hide_reasoning() {
    for (protocol, body) in [
        (
            GenerationProtocol::ChatCompletions,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Clean.\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            ),
        ),
        (
            GenerationProtocol::Responses,
            concat!(
                "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"secret\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"draft\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Clean.\"}]}]}}\n\n"
            ),
        ),
        (
            GenerationProtocol::Gemini,
            concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"secret\",\"thought\":true}]}}]}\r\r",
                "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"Clean.\"}]}}]}\r\r"
            ),
        ),
    ] {
        let (connection, _, worker) = server(body.into(), true);
        let settings = GenerationSettings {
            connection,
            protocol,
            model: "cleanup".into(),
            stream: true,
            ..Default::default()
        };
        assert_eq!(
            generate(
                &settings,
                "instructions",
                "input",
                None,
                &AtomicBool::new(false)
            )
            .unwrap(),
            "Clean."
        );
        worker.join().unwrap();
    }
    for protocol in [
        StreamProtocol::Chat,
        StreamProtocol::Responses,
        StreamProtocol::Gemini,
    ] {
        let mut stream = TranscriptStream::new(protocol);
        assert!(stream.event(br#"{"error":{"message":"bad"}}"#).is_err());
    }
    assert!(
        TranscriptStream::new(StreamProtocol::Chat)
            .event(br#"{"choices":[{"finish_reason":"length","delta":{"content":"truncated"}}]}"#)
            .is_err()
    );
}

#[test]
fn sse_decoder_accepts_multiline_data_and_all_newline_encodings() {
    for ending in ["\n", "\r\n", "\r"] {
        let input =
            format!("event: message{ending}data: {{{ending}data: \"value\": 1}}{ending}{ending}");
        let mut decoder = SseDecoder::default();
        let events = input
            .bytes()
            .filter_map(|byte| decoder.push(byte).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events, vec![b"{\n\"value\": 1}".to_vec()]);
    }
    let mut decoder = SseDecoder::default();
    assert!((0..=MAX_EVENT_BYTES).any(|_| decoder.push(b'x').is_err()));
}

#[test]
fn cancellation_interrupts_stalled_http_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
            .unwrap();
        let _ = stream.read(&mut [0]);
    });
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    let timer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Release);
    });
    let settings = GenerationSettings {
        connection: ConnectionSettings {
            base_url: format!("http://{address}"),
            api_key_env: String::new(),
            api_key_file: None,
        },
        model: "cleanup".into(),
        ..Default::default()
    };
    let started = Instant::now();
    assert!(
        generate(&settings, "instructions", "input", None, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    timer.join().unwrap();
    worker.join().unwrap();
}

#[test]
fn incomplete_streams_never_become_successful_cleanup() {
    let (connection, _, worker) = server(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into(),
        false,
    );
    let settings = GenerationSettings {
        connection,
        model: "cleanup".into(),
        stream: true,
        ..Default::default()
    };
    assert!(
        generate(
            &settings,
            "instructions",
            "input",
            None,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    worker.join().unwrap();
}

#[test]
#[allow(clippy::result_large_err)] // tungstenite fixes the handshake callback's error type.
fn realtime_sends_audio_before_release_and_commits_exactly_once() {
    use tokio_tungstenite::tungstenite::{Message, accept_hdr};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (audio_seen, audio_received) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = accept_hdr(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                let url = reqwest::Url::parse(&format!("http://fixture{}", request.uri())).unwrap();
                assert_eq!(url.path(), "/v1/realtime");
                let query = url
                    .query_pairs()
                    .collect::<std::collections::HashMap<_, _>>();
                assert_eq!(
                    query.get("intent").map(|value| value.as_ref()),
                    Some("transcription")
                );
                assert_eq!(
                    query.get("model").map(|value| value.as_ref()),
                    Some("gpt-live-transcribe")
                );
                Ok(response)
            },
        )
        .unwrap();
        let update: Value =
            serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(update["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(
            update["session"]["audio"]["input"]["turn_detection"],
            Value::Null
        );
        assert_eq!(
            update["session"]["audio"]["input"]["transcription"]["languages"],
            json!(["ar"])
        );
        assert!(update["session"]["audio"]["input"]["transcription"]["language"].is_null());
        socket
            .send(Message::text(r#"{"type":"session.updated"}"#))
            .unwrap();
        let mut pcm_bytes = 0;
        loop {
            let message = socket.read().unwrap();
            if !message.is_text() {
                continue;
            }
            let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            match event["type"].as_str().unwrap() {
                "input_audio_buffer.append" => {
                    pcm_bytes += BASE64_STANDARD
                        .decode(event["audio"].as_str().unwrap())
                        .unwrap()
                        .len();
                    let _ = audio_seen.try_send(());
                }
                "input_audio_buffer.commit" => {
                    socket
                        .send(Message::text(
                            r#"{"type":"input_audio_buffer.committed","item_id":"turn-1"}"#,
                        ))
                        .unwrap();
                    socket.send(Message::text(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"turn-1","transcript":"Final speech."}"#)).unwrap();
                    return pcm_bytes;
                }
                _ => panic!("unexpected event"),
            }
        }
    });
    let settings = TranscriptionSettings {
        connection: ConnectionSettings {
            base_url: format!("http://{address}/v1"),
            api_key_env: String::new(),
            api_key_file: None,
        },
        model: "gpt-live-transcribe".into(),
        language: "ar".into(),
        mode: TranscriptionMode::Realtime,
        ..Default::default()
    };
    let mut session = RealtimeSession::start(settings, Arc::new(AtomicBool::new(false)), |_| {});
    let audio = vec![0.25; 16_000];
    session.offer(&audio[..8000]);
    audio_received
        .recv_timeout(Duration::from_secs(3))
        .expect("audio must reach provider before release");
    assert_eq!(
        session
            .finish(&audio, &AtomicBool::new(false), Duration::from_secs(5))
            .unwrap(),
        "Final speech."
    );
    assert_eq!(worker.join().unwrap(), 24_000 * 2);
}

#[test]
fn realtime_slow_setup_preserves_fragmented_capture_without_blocking() {
    use tokio_tungstenite::tungstenite::{Message, accept};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (setup_seen, setup_received) = mpsc::sync_channel(1);
    let (allow_setup, setup_allowed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = accept(stream).unwrap();
        socket.read().unwrap();
        setup_seen.send(()).unwrap();
        setup_allowed.recv_timeout(Duration::from_secs(5)).unwrap();
        socket
            .send(Message::text(r#"{"type":"session.updated"}"#))
            .unwrap();
        let mut pcm = Vec::new();
        loop {
            let message = socket.read().unwrap();
            if !message.is_text() {
                continue;
            }
            let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            match event["type"].as_str().unwrap() {
                "input_audio_buffer.append" => pcm.extend(
                    BASE64_STANDARD
                        .decode(event["audio"].as_str().unwrap())
                        .unwrap(),
                ),
                "input_audio_buffer.commit" => {
                    socket
                        .send(Message::text(
                            r#"{"type":"input_audio_buffer.committed","item_id":"turn-1"}"#,
                        ))
                        .unwrap();
                    socket.send(Message::text(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"turn-1","transcript":"All speech retained."}"#)).unwrap();
                    return pcm;
                }
                _ => panic!("unexpected event"),
            }
        }
    });
    let settings = TranscriptionSettings {
        connection: ConnectionSettings {
            base_url: format!("http://{address}/v1"),
            api_key_env: String::new(),
            api_key_file: None,
        },
        model: "fixture-transcription".into(),
        mode: TranscriptionMode::Realtime,
        ..Default::default()
    };
    let mut session = RealtimeSession::start(settings, Arc::new(AtomicBool::new(false)), |_| {});
    setup_received.recv_timeout(Duration::from_secs(5)).unwrap();
    let audio = (0..48_000)
        .map(|index| index as f32 / 48_000.0 - 0.5)
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    // Match production: each callback reoffers the unaccepted capture suffix.
    for available in (320..=audio.len()).step_by(320) {
        session.offer(&audio[session.offered_samples()..available]);
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "capture waited on setup"
    );
    assert!(
        session.offered_samples() < audio.len(),
        "fixture must fill the bounded queue"
    );
    allow_setup.send(()).unwrap();
    let result = session.finish(&audio, &AtomicBool::new(false), Duration::from_secs(5));
    // Join even when the regression fails, so the fixture cannot leak a worker.
    let received = worker.join();
    assert_eq!(result.unwrap(), "All speech retained.");
    let received = received.unwrap();
    assert_eq!(received.len(), audio.len() * 3);
    for (index, sample) in received.chunks_exact(2).enumerate() {
        let actual = i16::from_le_bytes([sample[0], sample[1]]);
        let position = (index as f64 * 2.0 / 3.0).min((audio.len() - 1) as f64);
        let expected = ((position / 48_000.0 - 0.5) * f64::from(i16::MAX)) as i16;
        assert!(
            (i32::from(actual) - i32::from(expected)).abs() <= 1,
            "PCM changed at sample {index}"
        );
    }
}

#[test]
fn realtime_keeps_commit_order_when_final_transcripts_arrive_in_reverse() {
    use tokio_tungstenite::tungstenite::{Message, accept};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut socket = accept(stream).unwrap();
        socket.read().unwrap();
        socket
            .send(Message::text(r#"{"type":"session.updated"}"#))
            .unwrap();
        let mut commits = 0;
        while commits < 2 {
            let message = socket.read().unwrap();
            let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            if event["type"] == "input_audio_buffer.commit" {
                commits += 1;
                socket.send(Message::text(json!({
                    "type": "input_audio_buffer.committed", "item_id": format!("turn-{commits}")
                }).to_string())).unwrap();
            }
        }
        for (item_id, transcript) in [("turn-2", "Second."), ("turn-1", "First.")] {
            socket
                .send(Message::text(
                    json!({
                        "type": "conversation.item.input_audio_transcription.completed",
                        "item_id": item_id, "transcript": transcript
                    })
                    .to_string(),
                ))
                .unwrap();
        }
    });
    let settings = TranscriptionSettings {
        connection: ConnectionSettings {
            base_url: format!("http://{address}/v1"),
            api_key_env: String::new(),
            api_key_file: None,
        },
        model: "live-model".into(),
        mode: TranscriptionMode::Realtime,
        ..Default::default()
    };
    let session = RealtimeSession::start(settings, Arc::new(AtomicBool::new(false)), |_| {});
    let transcript = session
        .finish(
            &vec![0.25; 16_000 * 121],
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(transcript, "First. Second.");
    worker.join().unwrap();
}

#[test]
fn key_files_are_private_and_keys_never_serialize_into_connection_settings() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("hex-provider-key-test-{}", std::process::id()));
    std::fs::write(&path, "test-fixture-key\n").unwrap();
    let settings = ConnectionSettings {
        api_key_file: Some(path.clone()),
        ..Default::default()
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(settings.api_key().is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(settings.api_key().unwrap(), "test-fixture-key");
    assert!(
        !serde_json::to_string(&settings)
            .unwrap()
            .contains("test-fixture-key")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn ocr_context_sends_text_never_image_bytes_and_preserves_custom_instructions() {
    use crate::linux_context::{CaptureContext, ScreenContextMode};
    use crate::linux_pipeline::{CleanupBackend, PipelineSettings};
    let (connection, request, worker) = server(
        json!({"choices":[{"finish_reason":"stop","message":{"content":"Is this API correct?"}}]})
            .to_string(),
        false,
    );
    let mut settings = PipelineSettings::default();
    let mode = &mut settings.default_mode;
    mode.enabled = true;
    mode.backend = CleanupBackend::ChatCompletions;
    mode.model = "cleanup".into();
    mode.connection = connection;
    mode.prompt = "Never answer questions. Preserve Arabic and English.".into();
    mode.context.screen = ScreenContextMode::Ocr;
    let context = CaptureContext {
        screen_text: Some("fn transcribe_audio()".into()),
        screenshot: Some(ContextImage {
            mime_type: "image/png",
            bytes: vec![1, 2, 3],
        }),
        clipboard: Some("clipboard-not-opted-in".into()),
        ..Default::default()
    };
    let (text, observation) =
        settings.process("is this api correct", &context, &AtomicBool::new(false));
    assert_eq!(text, "Is this API correct?");
    assert!(observation.unwrap().fallback.is_none());
    let body = String::from_utf8(request.recv().unwrap().body).unwrap();
    assert!(body.contains("Never answer questions. Preserve Arabic and English."));
    assert!(body.contains("<TRANSCRIPT>"));
    assert!(body.contains("<CONTEXT_INFORMATION>"));
    assert!(body.contains("transcribe_audio"));
    assert!(!body.contains("image_url"));
    assert!(!body.contains("AQID"));
    assert!(!body.contains("clipboard-not-opted-in"));
    worker.join().unwrap();
}

#[test]
fn legacy_transcription_settings_default_to_two_retries_and_reject_excessive_counts() {
    let mut settings: TranscriptionSettings =
        serde_json::from_value(json!({"model": "fixture"})).unwrap();
    assert_eq!(settings.max_retries, 2);
    settings.max_retries = 0;
    settings.validate().unwrap();
    settings.max_retries = 10;
    settings.validate().unwrap();
    settings.max_retries = 11;
    assert!(settings.validate().is_err());
}
