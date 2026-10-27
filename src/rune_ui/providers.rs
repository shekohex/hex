//! Linux provider editor. Drafts commit together after validation; credentials
//! remain environment-variable or owner-only file references.
use std::collections::HashMap;

use color_eyre::eyre::{Result, eyre};
use rune::{El, TextEdit, col, edit_access, row};
use rune_kit::{TextArea, TextInput};

use super::Ui;
use super::controls::{
    ButtonKind, Paint, button, note, section, segmented, setting_row, toggle_row,
};
use crate::cloud::{ConnectionSettings, Thinking, TranscriptionMode, TranscriptionSettings};
use crate::linux_context::ScreenContextMode;
use crate::linux_pipeline::{CleanupBackend, PipelineMode, ProviderConfiguration};

#[derive(Default)]
pub(super) struct Providers {
    pub(super) open: bool,
    last: Option<crate::linux_last_dictation::Snapshot>,
    last_text: TextEdit,
    replacements: Vec<(TextEdit, TextEdit)>,
    configuration: ProviderConfiguration,
    speech: TranscriptionSettings,
    cloud_enabled: bool,
    selected: usize,
    fields: HashMap<&'static str, TextEdit>,
    error: Option<String>,
}

impl Providers {
    fn mode(&self) -> &PipelineMode {
        if self.selected == 0 {
            &self.configuration.processing.default_mode
        } else {
            &self.configuration.processing.modes[self.selected - 1]
        }
    }

    fn mode_mut(&mut self) -> &mut PipelineMode {
        if self.selected == 0 {
            &mut self.configuration.processing.default_mode
        } else {
            &mut self.configuration.processing.modes[self.selected - 1]
        }
    }

    fn put(&mut self, key: &'static str, value: impl Into<String>) {
        self.fields.insert(key, TextEdit::new(value.into()));
    }

    fn value(&self, key: &'static str) -> &str {
        self.fields.get(key).map_or("", |edit| edit.text.as_str())
    }

    fn connection(&self, prefix: &str) -> ConnectionSettings {
        let (url, environment, file) = if prefix == "speech" {
            ("speech-url", "speech-env", "speech-file")
        } else {
            ("mode-url", "mode-env", "mode-file")
        };
        ConnectionSettings {
            base_url: self.value(url).trim().into(),
            api_key_env: self.value(environment).trim().into(),
            api_key_file: match self.value(file).trim() {
                "" => None,
                path => Some(path.into()),
            },
        }
    }

    pub(super) fn load(&mut self, settings: &crate::app_settings::AppSettings) {
        self.configuration = ProviderConfiguration {
            cloud_transcription: settings.cloud_transcription.clone(),
            processing: settings.processing.clone(),
        };
        self.cloud_enabled = settings.cloud_transcription.is_some();
        self.speech = settings.cloud_transcription.clone().unwrap_or_default();
        self.selected = 0;
        self.error = None;
        let speech = self.speech.clone();
        self.put("speech-url", speech.connection.base_url);
        self.put("speech-env", speech.connection.api_key_env);
        self.put(
            "speech-file",
            speech
                .connection
                .api_key_file
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        );
        self.put("speech-model", speech.model);
        self.put("speech-language", speech.language);
        self.put("speech-prompt", speech.prompt);
        self.put("speech-deadline", speech.deadline_seconds.to_string());
        self.put("speech-retries", speech.max_retries.to_string());
        self.load_mode();
    }

    fn load_mode(&mut self) {
        let mode = self.mode().clone();
        self.put("mode-name", mode.name);
        self.put("mode-applications", mode.applications.join(", "));
        self.put("mode-url", mode.connection.base_url);
        self.put("mode-env", mode.connection.api_key_env);
        self.put(
            "mode-file",
            mode.connection
                .api_key_file
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        );
        self.put("mode-model", mode.model);
        self.put("mode-prompt", mode.prompt);
        self.put("mode-deadline", mode.deadline_seconds.to_string());
        self.put(
            "mode-budget",
            match mode.thinking {
                Thinking::Budget(tokens) => tokens,
                _ => 1024,
            }
            .to_string(),
        );
        self.put("mode-ocr", mode.context.ocr_languages);
        self.replacements = mode
            .replacements
            .into_iter()
            .map(|rule| {
                (
                    TextEdit::new(rule.matched_phrase),
                    TextEdit::new(rule.output),
                )
            })
            .collect();
    }

    fn project_mode(&mut self) -> Result<()> {
        let mut mode = self.mode().clone();
        mode.name = self.value("mode-name").into();
        mode.applications = self
            .value("mode-applications")
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        mode.connection = self.connection("mode");
        mode.model = self.value("mode-model").trim().into();
        mode.prompt = self.value("mode-prompt").into();
        mode.deadline_seconds = self
            .value("mode-deadline")
            .trim()
            .parse()
            .map_err(|_| eyre!("Cleanup deadline must be seconds"))?;
        if matches!(mode.thinking, Thinking::Budget(_)) {
            mode.thinking = Thinking::Budget(
                self.value("mode-budget")
                    .trim()
                    .parse()
                    .map_err(|_| eyre!("Thinking budget must be tokens"))?,
            );
        }
        mode.context.ocr_languages = self.value("mode-ocr").trim().into();
        mode.replacements = self
            .replacements
            .iter()
            .map(
                |(phrase, output)| crate::text_replacements::TextReplacement {
                    matched_phrase: phrase.text.clone(),
                    output: output.text.clone(),
                },
            )
            .collect();
        *self.mode_mut() = mode;
        Ok(())
    }

    fn project(&mut self) -> Result<ProviderConfiguration> {
        self.project_mode()?;
        if self.cloud_enabled {
            let mut speech = self.speech.clone();
            speech.connection = self.connection("speech");
            speech.model = self.value("speech-model").trim().into();
            speech.language = self.value("speech-language").trim().into();
            speech.prompt = self.value("speech-prompt").into();
            speech.deadline_seconds = self
                .value("speech-deadline")
                .trim()
                .parse()
                .map_err(|_| eyre!("Transcription deadline must be seconds"))?;
            speech.max_retries = self
                .value("speech-retries")
                .trim()
                .parse()
                .map_err(|_| eyre!("Retries must be between 0 and 10"))?;
            self.configuration.cloud_transcription = Some(speech);
        } else {
            self.configuration.cloud_transcription = None;
        }
        self.configuration.validate()?;
        Ok(self.configuration.clone())
    }
}

fn field(ui: &Ui, p: Paint, key: &'static str, label: &str, multiline: bool) -> El {
    let edit = &ui.providers.fields[key];
    let input = if multiline {
        TextArea::new(edit)
            .rows(4)
            .width(None)
            .on_edit(edit_access(move |ui: &mut Ui| {
                ui.providers.fields.get_mut(key).unwrap()
            }))
            .build(p.k)
            .key("input")
    } else {
        TextInput::new(edit)
            .width(Some(300.0))
            .on_edit(edit_access(move |ui: &mut Ui| {
                ui.providers.fields.get_mut(key).unwrap()
            }))
            .build(p.k)
            .key("input")
    };
    if multiline {
        col()
            .key(key)
            .padding(12.0)
            .gap(6.0)
            .child(note("label", label, p.text()))
            .child(input)
    } else {
        setting_row(p, key, label, None, input)
    }
}

fn connection_fields(ui: &Ui, p: Paint, speech: bool) -> Vec<El> {
    let keys = if speech {
        ["speech-url", "speech-env", "speech-file"]
    } else {
        ["mode-url", "mode-env", "mode-file"]
    };
    keys.into_iter()
        .zip([
            "API base URL",
            "Key environment variable",
            "Private key file",
        ])
        .map(|(key, label)| field(ui, p, key, label, false))
        .collect()
}

pub(super) fn view(ui: &Ui, p: Paint) -> El {
    let mut content = col().key("providers").gap(16.0);
    let control = button(
        p,
        "edit-providers",
        if ui.providers.open {
            "Close"
        } else {
            "Configure…"
        },
        ButtonKind::Secondary,
        |ui: &mut Ui, _| {
            if !ui.providers.open {
                ui.providers.load(&ui.settings);
            }
            ui.providers.open = !ui.providers.open;
        },
    );
    content = content.child(section(
        p,
        "provider-entry",
        "Cloud providers",
        vec![setting_row(
            p,
            "configure-providers",
            "Transcription and cleanup",
            Some("Cloud audio and optional per-application context"),
            control,
        )],
    ));
    if !ui.providers.open {
        return content;
    }
    if let Some(error) = &ui.providers.error {
        content = content.child(note("provider-error", error, p.danger()));
    }
    let mut speech = vec![toggle_row(
        p,
        "cloud-enabled",
        "Cloud transcription",
        None,
        ui.providers.cloud_enabled,
        |ui| {
            ui.providers.cloud_enabled = !ui.providers.cloud_enabled;
        },
    )];
    if ui.providers.cloud_enabled {
        speech.extend(connection_fields(ui, p, true));
        speech.push(field(ui, p, "speech-model", "Model", false));
        speech.push(setting_row(
            p,
            "speech-mode",
            "Transport",
            None,
            segmented(
                p,
                "speech-mode-choice",
                &["Batch", "Streaming", "Realtime"],
                ui.providers.speech.mode as usize,
                |ui, index| {
                    ui.providers.speech.mode = [
                        TranscriptionMode::Batch,
                        TranscriptionMode::Streaming,
                        TranscriptionMode::Realtime,
                    ][index];
                },
            ),
        ));
        speech.push(toggle_row(
            p,
            "server-vad",
            "Server VAD previews",
            None,
            ui.providers.speech.server_vad,
            |ui| ui.providers.speech.server_vad = !ui.providers.speech.server_vad,
        ));
        for (key, label) in [
            ("speech-language", "Language"),
            ("speech-deadline", "Deadline in seconds"),
            ("speech-retries", "Retries"),
        ] {
            speech.push(field(ui, p, key, label, false));
        }
        speech.push(field(ui, p, "speech-prompt", "Recognition hints", true));
    }
    content = content.child(section(p, "cloud-transcription", "Transcription", speech));
    let mode = ui.providers.mode();
    let mut mode_buttons = row().key("mode-choices").max_w(300.0).gap(4.0).flex_wrap();
    for (index, mode) in std::iter::once(&ui.providers.configuration.processing.default_mode)
        .chain(&ui.providers.configuration.processing.modes)
        .enumerate()
    {
        mode_buttons = mode_buttons.child(button(
            p,
            &format!("provider-mode-{index}"),
            &mode.name,
            ButtonKind::Secondary,
            move |ui: &mut Ui, _| match ui.providers.project_mode() {
                Ok(()) => {
                    ui.providers.selected = index;
                    ui.providers.load_mode();
                    ui.providers.error = None;
                }
                Err(error) => ui.providers.error = Some(error.to_string()),
            },
        ));
    }
    mode_buttons = mode_buttons.child(button(
        p,
        "add-provider-mode",
        "Add mode",
        ButtonKind::Secondary,
        |ui: &mut Ui, _| {
            if ui.providers.configuration.processing.modes.len() >= 32 {
                return;
            }
            if let Err(error) = ui.providers.project_mode() {
                ui.providers.error = Some(error.to_string());
                return;
            }
            ui.providers
                .configuration
                .processing
                .modes
                .push(PipelineMode {
                    name: "New mode".into(),
                    ..Default::default()
                });
            ui.providers.selected = ui.providers.configuration.processing.modes.len();
            ui.providers.load_mode();
        },
    ));
    let mut cleanup = vec![setting_row(
        p,
        "provider-mode-list",
        "Processing mode",
        None,
        mode_buttons,
    )];
    cleanup.push(field(ui, p, "mode-name", "Name", false));
    if ui.providers.selected > 0 {
        cleanup.push(field(
            ui,
            p,
            "mode-applications",
            "Applications, comma-separated",
            false,
        ));
        cleanup.push(setting_row(
            p,
            "remove-provider-mode",
            "Remove mode",
            None,
            button(
                p,
                "remove",
                "Remove",
                ButtonKind::Secondary,
                |ui: &mut Ui, _| {
                    ui.providers
                        .configuration
                        .processing
                        .modes
                        .remove(ui.providers.selected - 1);
                    ui.providers.selected = 0;
                    ui.providers.load_mode();
                },
            ),
        ));
    }
    cleanup.push(toggle_row(
        p,
        "cleanup-enabled",
        "Rewrite transcript",
        None,
        mode.enabled,
        |ui| {
            let mode = ui.providers.mode_mut();
            mode.enabled = !mode.enabled;
        },
    ));
    cleanup.push(setting_row(
        p,
        "cleanup-backend",
        "Provider protocol",
        None,
        segmented(
            p,
            "cleanup-protocol",
            &["OpenCode", "Chat", "Responses", "Gemini"],
            mode.backend as usize,
            |ui, index| {
                let mode = ui.providers.mode_mut();
                mode.backend = [
                    CleanupBackend::OpenCode,
                    CleanupBackend::ChatCompletions,
                    CleanupBackend::Responses,
                    CleanupBackend::Gemini,
                ][index];
                mode.thinking = Thinking::Default;
            },
        ),
    ));
    if mode.backend != CleanupBackend::OpenCode {
        cleanup.extend(connection_fields(ui, p, false));
    }
    cleanup.push(field(ui, p, "mode-model", "Cleanup model", false));
    let mut thinking = row()
        .key("thinking-choices")
        .max_w(300.0)
        .gap(4.0)
        .flex_wrap();
    for choice in mode.thinking_options(ui.providers.value("mode-model")) {
        let choice = *choice;
        thinking = thinking.child(button(
            p,
            &format!("thinking-{}", choice.label()),
            choice.label(),
            ButtonKind::Secondary,
            move |ui: &mut Ui, _| ui.providers.mode_mut().thinking = choice,
        ));
    }
    cleanup.push(setting_row(
        p,
        "thinking",
        &format!("Thinking: {}", mode.thinking.label()),
        None,
        thinking,
    ));
    if matches!(mode.thinking, Thinking::Budget(_)) {
        cleanup.push(field(ui, p, "mode-budget", "Thinking tokens", false));
    }
    cleanup.push(toggle_row(
        p,
        "cleanup-stream",
        "Stream cleanup response",
        None,
        mode.stream,
        |ui| {
            let mode = ui.providers.mode_mut();
            mode.stream = !mode.stream;
        },
    ));
    cleanup.push(field(
        ui,
        p,
        "mode-deadline",
        "Cleanup deadline in seconds",
        false,
    ));
    cleanup.push(field(ui, p, "mode-prompt", "Cleanup instructions", true));
    for (index, (phrase, output)) in ui.providers.replacements.iter().enumerate() {
        cleanup.push(
            row()
                .key(format!("provider-correction-{index}"))
                .padding(12.0)
                .gap(6.0)
                .child(
                    TextInput::new(phrase)
                        .placeholder("Spoken phrase")
                        .width(None)
                        .on_edit(edit_access(move |ui: &mut Ui| {
                            &mut ui.providers.replacements[index].0
                        }))
                        .build(p.k)
                        .key("phrase")
                        .grow(1.0)
                        .shrink(),
                )
                .child(
                    TextInput::new(output)
                        .placeholder("Replacement")
                        .width(None)
                        .on_edit(edit_access(move |ui: &mut Ui| {
                            &mut ui.providers.replacements[index].1
                        }))
                        .build(p.k)
                        .key("output")
                        .grow(1.0)
                        .shrink(),
                )
                .child(button(
                    p,
                    "remove-correction",
                    "Remove",
                    ButtonKind::Secondary,
                    move |ui: &mut Ui, _| {
                        ui.providers.replacements.remove(index);
                    },
                )),
        );
    }
    cleanup.push(button(
        p,
        "add-correction",
        "Add correction",
        ButtonKind::Secondary,
        |ui: &mut Ui, _| {
            ui.providers
                .replacements
                .push((TextEdit::default(), TextEdit::default()));
        },
    ));
    cleanup.push(toggle_row(
        p,
        "clipboard-context",
        "Clipboard context",
        None,
        mode.context.clipboard,
        |ui| {
            let context = &mut ui.providers.mode_mut().context;
            context.clipboard = !context.clipboard;
        },
    ));
    cleanup.push(toggle_row(
        p,
        "field-context",
        "Active text field context",
        None,
        mode.context.active_field,
        |ui| {
            let context = &mut ui.providers.mode_mut().context;
            context.active_field = !context.active_field;
        },
    ));
    cleanup.push(setting_row(
        p,
        "screen-context",
        "Focused window context",
        None,
        segmented(
            p,
            "screen-context-choice",
            &["Off", "OCR", "Image"],
            mode.context.screen as usize,
            |ui, index| {
                ui.providers.mode_mut().context.screen = [
                    ScreenContextMode::Off,
                    ScreenContextMode::Ocr,
                    ScreenContextMode::Screenshot,
                ][index];
            },
        ),
    ));
    if mode.context.screen == ScreenContextMode::Ocr {
        cleanup.push(field(ui, p, "mode-ocr", "OCR languages", false));
    }
    content = content.child(section(p, "provider-cleanup", "Cleanup", cleanup));
    content.child(button(
        p,
        "save-providers",
        "Save providers",
        ButtonKind::Secondary,
        |ui: &mut Ui, _| match ui.providers.project() {
            Ok(configuration) => {
                if ui.update(|settings| {
                    settings.cloud_transcription = configuration.cloud_transcription;
                    settings.processing = configuration.processing;
                }) {
                    ui.providers.error = None;
                    ui.providers.open = false;
                } else {
                    ui.providers.error = ui.error.clone();
                }
            }
            Err(error) => ui.providers.error = Some(error.to_string()),
        },
    ))
}

pub(super) fn poll(ui: &mut Ui) -> bool {
    let last = match &ui.service {
        super::Service::Live(backend) => match backend.as_ref() {
            super::host::Backend::Host(host) => host.last_dictation(),
            super::host::Backend::Status(Some(status), _) => status.last_dictation.clone(),
            _ => None,
        },
        _ => return false,
    };
    if last == ui.providers.last {
        return false;
    }
    if last.as_ref().map(|last| (last.id, &last.text))
        != ui.providers.last.as_ref().map(|last| (last.id, &last.text))
    {
        ui.providers.last_text = TextEdit::new(
            last.as_ref()
                .map(|last| last.text.clone())
                .unwrap_or_default(),
        );
    }
    ui.providers.last = last;
    true
}

pub(super) fn last_result(ui: &Ui, p: Paint) -> Option<El> {
    let last = ui.providers.last.as_ref()?;
    let mut rows = vec![
        TextArea::new(&ui.providers.last_text)
            .rows(6)
            .width(None)
            .on_edit(|ui: &mut Ui| &mut ui.providers.last_text)
            .build(p.k)
            .key("last-text"),
    ];
    if let Some(error) = &last.error {
        rows.insert(0, note("last-error", error, p.danger()));
    }
    if let Some(timings) = &last.timings {
        let duration =
            |value: Option<u64>| value.map_or_else(|| "—".into(), |value| format!("{value} ms"));
        let stage = if timings.realtime {
            "Finalize"
        } else {
            "Transcription"
        };
        rows.push(note("last-timings", &format!("Audio {} ms · Queue {} ms · {stage} {} · Context {} · Cleanup {} · Paste {} · Total {} ms · Retries {}", timings.audio_ms, timings.queue_ms, duration(timings.transcription_ms), duration(timings.context_wait_ms), duration(timings.cleanup_ms), duration(timings.paste_ms), timings.total_ms, timings.retries), p.muted()));
    }
    let mut actions = row().key("last-actions").gap(6.0).flex_wrap();
    let text = ui.providers.last_text.text.clone();
    if !text.is_empty() {
        actions = actions.child(button(
            p,
            "copy-last",
            "Copy",
            ButtonKind::Secondary,
            move |_: &mut Ui, tx| tx.copy(text.clone()),
        ));
    }
    if !last.busy {
        for (key, label, cleanup_only, available) in [
            (
                "retry-transcription",
                "Retry transcription",
                false,
                last.audio_available,
            ),
            ("retry-cleanup", "Retry cleanup", true, last.raw_available),
        ] {
            if !available {
                continue;
            }
            let id = last.id;
            actions = actions.child(button(
                p,
                key,
                label,
                ButtonKind::Secondary,
                move |ui: &mut Ui, _| {
                    if let super::Service::Live(backend) = &ui.service
                        && let super::host::Backend::Host(host) = backend.as_ref()
                    {
                        match host.provider_request(crate::linux_service::Request::RetryLast {
                            id,
                            cleanup_only,
                        }) {
                            Ok(()) => {
                                ui.providers.last_text = TextEdit::new(
                                    ui.providers
                                        .last
                                        .as_ref()
                                        .map(|last| last.text.clone())
                                        .unwrap_or_default(),
                                );
                            }
                            Err(error) => ui.error = Some(error.to_string()),
                        }
                    }
                },
            ));
        }
        let id = last.id;
        actions = actions.child(button(
            p,
            "clear-last",
            "Clear",
            ButtonKind::Secondary,
            move |ui: &mut Ui, _| {
                if let super::Service::Live(backend) = &ui.service
                    && let super::host::Backend::Host(host) = backend.as_ref()
                    && let Err(error) =
                        host.provider_request(crate::linux_service::Request::ClearLast { id })
                {
                    ui.error = Some(error.to_string());
                }
            },
        ));
    }
    rows.push(actions);
    Some(section(p, "last-dictation", "Last dictation", rows))
}

pub(super) fn fixture(ui: &mut Ui) {
    ui.settings.cloud_transcription = Some(TranscriptionSettings {
        model: "gateway-asr".into(),
        mode: TranscriptionMode::Realtime,
        server_vad: true,
        ..Default::default()
    });
    ui.settings.processing.default_mode.backend = CleanupBackend::Responses;
    ui.settings.processing.default_mode.model = "cleanup".into();
    ui.settings.processing.default_mode.enabled = true;
    ui.providers.load(&ui.settings);
    ui.providers.open = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_round_trips_every_provider_setting_without_changing_credentials() {
        let mut settings = crate::app_settings::AppSettings {
            cloud_transcription: Some(TranscriptionSettings {
                model: "gateway-asr".into(),
                mode: TranscriptionMode::Realtime,
                server_vad: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        settings.processing.default_mode.backend = CleanupBackend::Gemini;
        settings.processing.default_mode.model = "gemini-2.5-flash".into();
        settings.processing.default_mode.thinking = Thinking::Budget(2048);
        settings.processing.default_mode.context.screen = ScreenContextMode::Screenshot;
        settings.processing.default_mode.enabled = true;
        let mut editor = Providers::default();
        editor.load(&settings);
        let result = editor.project().unwrap();
        assert_eq!(result.cloud_transcription, settings.cloud_transcription);
        assert_eq!(result.processing, settings.processing);
        editor.put("speech-deadline", "0");
        assert!(editor.project().is_err());
        assert_eq!(settings.cloud_transcription.unwrap().deadline_seconds, 120);
        editor.cloud_enabled = false;
        assert!(editor.project().unwrap().cloud_transcription.is_none());
    }
}
