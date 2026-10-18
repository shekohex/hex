use gpui::{AnyElement, App, Context, Entity, EventEmitter, Render, Window, div, prelude::*, px};

use crate::cloud::{ConnectionSettings, Thinking, TranscriptionMode, TranscriptionSettings};
use crate::desktop_ui::{
    error_message, header_button, pane_body, pane_content, pane_header_with_action,
    segmented_control, segmented_item, settings_panel, settings_row, settings_section_label,
    toggle,
};
use crate::linux_context::ScreenContextMode;
use crate::linux_pipeline::{CleanupBackend, PipelineMode};
use crate::linux_settings::ProviderConfiguration;
use crate::text_input::TextInput;

struct ConnectionInputs {
    url: Entity<TextInput>,
    environment: Entity<TextInput>,
    file: Entity<TextInput>,
}

impl ConnectionInputs {
    fn new(settings: &ConnectionSettings, cx: &mut Context<ProviderEditor>) -> Self {
        Self {
            url: input("API base URL", &settings.base_url, cx),
            environment: input("API key environment variable", &settings.api_key_env, cx),
            file: input(
                "Owner-only key file (optional)",
                &settings
                    .api_key_file
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                cx,
            ),
        }
    }

    fn read(&self, cx: &App) -> ConnectionSettings {
        ConnectionSettings {
            base_url: self.url.read(cx).text().trim().into(),
            api_key_env: self.environment.read(cx).text().trim().into(),
            api_key_file: match self.file.read(cx).text().trim() {
                "" => None,
                path => Some(path.into()),
            },
        }
    }

    fn render(&self) -> Vec<AnyElement> {
        vec![
            field("API base URL", &self.url),
            field("Key environment variable", &self.environment),
            field("Key file", &self.file),
        ]
    }
}

struct SpeechInputs {
    connection: ConnectionInputs,
    model: Entity<TextInput>,
    language: Entity<TextInput>,
    hints: Entity<TextInput>,
    deadline: Entity<TextInput>,
}

struct ModeInputs {
    connection: ConnectionInputs,
    name: Entity<TextInput>,
    applications: Entity<TextInput>,
    model: Entity<TextInput>,
    instructions: Entity<TextInput>,
    thinking_budget: Entity<TextInput>,
    _model_subscription: gpui::Subscription,
    deadline: Entity<TextInput>,
    ocr_languages: Entity<TextInput>,
}

impl ModeInputs {
    fn new(mode: &PipelineMode, cx: &mut Context<ProviderEditor>) -> Self {
        let model = input("Model ID; OpenCode may use Automatic", &mode.model, cx);
        let model_subscription =
            cx.subscribe(&model, |_, _, _: &crate::text_input::Changed, cx| {
                cx.notify();
            });
        Self {
            connection: ConnectionInputs::new(&mode.connection, cx),
            name: input("Mode name", &mode.name, cx),
            applications: input(
                "Application names, separated by commas",
                &mode.applications.join(", "),
                cx,
            ),
            model,
            thinking_budget: input(
                "Thinking tokens",
                &match mode.thinking {
                    Thinking::Budget(tokens) => tokens,
                    _ => 1024,
                }
                .to_string(),
                cx,
            ),
            _model_subscription: model_subscription,
            instructions: cx
                .new(|cx| TextInput::multiline(cx, "Cleanup instructions", &mode.prompt)),
            deadline: input("Seconds", &mode.deadline_seconds.to_string(), cx),
            ocr_languages: input(
                "Tesseract languages, e.g. eng+ara",
                &mode.context.ocr_languages,
                cx,
            ),
        }
    }
}

pub struct ProviderEditor {
    configuration: ProviderConfiguration,
    speech: TranscriptionSettings,
    cloud_enabled: bool,
    speech_inputs: SpeechInputs,
    selected_mode: Option<usize>,
    mode_inputs: ModeInputs,
    error: Option<String>,
    service_error: Option<String>,
    busy: bool,
    submitted: Option<ProviderConfiguration>,
    saved: bool,
}

impl EventEmitter<ProviderConfiguration> for ProviderEditor {}

impl ProviderEditor {
    pub fn new(configuration: ProviderConfiguration, cx: &mut Context<Self>) -> Self {
        let cloud_enabled = configuration.cloud_transcription.is_some();
        let speech = configuration
            .cloud_transcription
            .clone()
            .unwrap_or_default();
        let speech_inputs = SpeechInputs {
            connection: ConnectionInputs::new(&speech.connection, cx),
            model: input("Transcription model ID", &speech.model, cx),
            language: input(
                "Language hint, e.g. en; empty detects language",
                &speech.language,
                cx,
            ),
            hints: input("Recognition hints (optional)", &speech.prompt, cx),
            deadline: input("Seconds", &speech.deadline_seconds.to_string(), cx),
        };
        let mode_inputs = ModeInputs::new(&configuration.processing.default_mode, cx);
        Self {
            configuration,
            speech,
            cloud_enabled,
            speech_inputs,
            selected_mode: None,
            mode_inputs,
            error: None,
            service_error: None,
            busy: false,
            submitted: None,
            saved: false,
        }
    }

    pub fn observe_service(
        &mut self,
        settings: &crate::linux_settings::LinuxSettings,
        busy: bool,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.busy = busy;
        self.service_error = error;
        if let Some(submitted) = &self.submitted {
            if !busy
                && settings.cloud_transcription == submitted.cloud_transcription
                && settings.processing == submitted.processing
            {
                self.submitted = None;
                self.saved = true;
            } else if self.service_error.is_some() {
                self.submitted = None;
            }
        }
        cx.notify();
    }

    fn mode(&self) -> &PipelineMode {
        self.selected_mode
            .map_or(&self.configuration.processing.default_mode, |index| {
                &self.configuration.processing.modes[index]
            })
    }

    fn mode_mut(&mut self) -> &mut PipelineMode {
        match self.selected_mode {
            None => &mut self.configuration.processing.default_mode,
            Some(index) => &mut self.configuration.processing.modes[index],
        }
    }

    fn commit_mode(&mut self, cx: &App) -> color_eyre::Result<()> {
        let name = self.mode_inputs.name.read(cx).text().trim().to_owned();
        let applications = self
            .mode_inputs
            .applications
            .read(cx)
            .text()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        let model = self.mode_inputs.model.read(cx).text().trim().to_owned();
        let prompt = self.mode_inputs.instructions.read(cx).text().to_owned();
        let deadline = self
            .mode_inputs
            .deadline
            .read(cx)
            .text()
            .trim()
            .parse::<u64>()
            .map_err(|_| color_eyre::eyre::eyre!("Cleanup deadline must be a number of seconds"))?;
        let connection = self.mode_inputs.connection.read(cx);
        let thinking = if matches!(self.mode().thinking, Thinking::Budget(_)) {
            Thinking::Budget(
                self.mode_inputs
                    .thinking_budget
                    .read(cx)
                    .text()
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        color_eyre::eyre::eyre!("Thinking budget must be a token count")
                    })?,
            )
        } else {
            self.mode().thinking
        };
        let ocr_languages = self
            .mode_inputs
            .ocr_languages
            .read(cx)
            .text()
            .trim()
            .to_owned();
        let mode = self.mode_mut();
        mode.name = name;
        mode.applications = applications;
        mode.model = model;
        mode.thinking = thinking;
        mode.prompt = prompt;
        mode.deadline_seconds = deadline;
        mode.connection = connection;
        mode.context.ocr_languages = ocr_languages;
        Ok(())
    }

    fn select_mode(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if let Err(error) = self.commit_mode(cx) {
            self.error = Some(error.to_string());
            return;
        }
        self.selected_mode = index;
        self.mode_inputs = ModeInputs::new(self.mode(), cx);
        self.error = None;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> color_eyre::Result<ProviderConfiguration> {
            self.commit_mode(cx)?;
            if self.cloud_enabled {
                self.speech.connection = self.speech_inputs.connection.read(cx);
                self.speech.model = self.speech_inputs.model.read(cx).text().trim().into();
                self.speech.language = self.speech_inputs.language.read(cx).text().trim().into();
                self.speech.prompt = self.speech_inputs.hints.read(cx).text().into();
                self.speech.deadline_seconds = self
                    .speech_inputs
                    .deadline
                    .read(cx)
                    .text()
                    .trim()
                    .parse()
                    .map_err(|_| {
                        color_eyre::eyre::eyre!(
                            "Transcription deadline must be a number of seconds"
                        )
                    })?;
            }
            self.configuration.cloud_transcription =
                self.cloud_enabled.then(|| self.speech.clone());
            self.configuration.validate()?;
            Ok(self.configuration.clone())
        })();
        match result {
            Ok(configuration) => {
                self.error = None;
                self.saved = false;
                self.submitted = Some(configuration.clone());
                cx.emit(configuration);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }
}

impl Render for ProviderEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = self.busy || self.submitted.is_some();
        let save = header_button(if self.busy {
            "Applying…"
        } else if self.submitted.is_some() {
            "Saving…"
        } else if self.saved {
            "Saved"
        } else {
            "Save"
        })
        .id("save-providers")
        .when(blocked, |button| button.opacity(0.5))
        .on_click(cx.listener(move |this, _, _, cx| {
            if !blocked {
                this.save(cx);
            }
        }))
        .into_any_element();
        let speech_modes = segmented_control().children(
            [
                ("Local", None),
                ("Cloud", Some(TranscriptionMode::Batch)),
                ("Stream", Some(TranscriptionMode::Streaming)),
                ("Realtime", Some(TranscriptionMode::Realtime)),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (label, mode))| {
                let selected = mode.map_or(!self.cloud_enabled, |mode| {
                    self.cloud_enabled && self.speech.mode == mode
                });
                segmented_item(selected)
                    .id(("speech-mode", index))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cloud_enabled = mode.is_some();
                        if let Some(mode) = mode {
                            let current = this.speech_inputs.model.read(cx).text().to_owned();
                            let default = match (mode, current.as_str()) {
                                (TranscriptionMode::Realtime, "gpt-transcribe") => {
                                    Some("gpt-live-transcribe")
                                }
                                (
                                    TranscriptionMode::Batch | TranscriptionMode::Streaming,
                                    "gpt-live-transcribe",
                                ) => Some("gpt-transcribe"),
                                _ => None,
                            };
                            if let Some(default) = default {
                                this.speech_inputs
                                    .model
                                    .update(cx, |input, cx| input.set_text(default, cx));
                            }
                            this.speech.mode = mode;
                        }
                        cx.notify();
                    }))
            }),
        );
        let speech = settings_panel()
            .child(settings_row(
                "Transcription",
                "Choose where speech is transcribed",
                speech_modes,
            ))
            .when(self.cloud_enabled, |panel| {
                panel
                    .children(self.speech_inputs.connection.render())
                    .child(field("Model ID", &self.speech_inputs.model))
                    .child(field("Language", &self.speech_inputs.language))
                    .child(field("Recognition hints", &self.speech_inputs.hints))
                    .child(field("Deadline (seconds)", &self.speech_inputs.deadline))
            });
        let mode_names = std::iter::once((None, "Global".to_owned()))
            .chain(
                self.configuration
                    .processing
                    .modes
                    .iter()
                    .enumerate()
                    .map(|(index, mode)| (Some(index), mode.name.clone())),
            )
            .collect::<Vec<_>>();
        let modes = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(
                mode_names
                    .into_iter()
                    .enumerate()
                    .map(|(tag, (index, name))| {
                        header_button(name).id(("pipeline-mode", tag)).on_click(
                            cx.listener(move |this, _, _, cx| this.select_mode(index, cx)),
                        )
                    }),
            )
            .child(
                header_button("+ Mode")
                    .id("add-pipeline-mode")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.configuration.processing.modes.len() < 32 {
                            let index = this.configuration.processing.modes.len();
                            this.configuration.processing.modes.push(PipelineMode {
                                name: format!("Mode {}", index + 1),
                                ..Default::default()
                            });
                            this.select_mode(Some(index), cx);
                        }
                    })),
            );
        let selected_backend = self.mode().backend;
        let backends = div().flex().flex_wrap().gap_2().children(
            [
                ("OpenCode", CleanupBackend::OpenCode),
                ("Chat", CleanupBackend::ChatCompletions),
                ("Responses", CleanupBackend::Responses),
                ("Gemini", CleanupBackend::Gemini),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, backend))| {
                header_button(name)
                    .id(("cleanup-backend", index))
                    .when(selected_backend != backend, |button| button.opacity(0.6))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Err(error) = this.commit_mode(cx) {
                            this.error = Some(error.to_string());
                            return;
                        }
                        let mode = this.mode_mut();
                        if mode.backend != backend {
                            mode.thinking = Thinking::Default;
                        }
                        mode.backend = backend;
                        if backend == CleanupBackend::Gemini
                            && mode.connection.base_url == ConnectionSettings::default().base_url
                        {
                            mode.connection.base_url =
                                "https://generativelanguage.googleapis.com/v1beta".into();
                            mode.connection.api_key_env = "GEMINI_API_KEY".into();
                        } else if backend != CleanupBackend::Gemini
                            && mode.connection.base_url
                                == "https://generativelanguage.googleapis.com/v1beta"
                        {
                            mode.connection = ConnectionSettings::default();
                        }
                        this.mode_inputs = ModeInputs::new(this.mode(), cx);
                        cx.notify();
                    }))
            }),
        );
        let enabled = self.mode().enabled;
        let streaming = self.mode().stream;
        let clipboard = self.mode().context.clipboard;
        let active_field = self.mode().context.active_field;
        let screen_context = self.mode().context.screen;
        let thinking = self.mode().thinking;
        let thinking_choices = self
            .mode()
            .thinking_options(self.mode_inputs.model.read(cx).text());
        let thinking_control = div().w(px(330.0)).flex().flex_wrap().gap_2().children(
            thinking_choices
                .iter()
                .copied()
                .enumerate()
                .map(|(index, choice)| {
                    let selected = choice == thinking
                        || matches!(
                            (choice, thinking),
                            (Thinking::Budget(_), Thinking::Budget(_))
                        );
                    header_button(choice.label())
                        .id(("cleanup-thinking", index))
                        .when(!selected, |button| button.opacity(0.6))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mode_mut().thinking = choice;
                            cx.notify();
                        }))
                }),
        );
        let cleanup = settings_panel()
            .child(
                settings_row(
                    "Cleanup",
                    "Run corrections, then optional LLM cleanup",
                    toggle(if enabled { 1.0 } else { 0.0 }),
                )
                .id("pipeline-enabled")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode_mut().enabled = !enabled;
                    cx.notify();
                })),
            )
            .when(self.selected_mode.is_some(), |panel| {
                panel
                    .child(field("Name", &self.mode_inputs.name))
                    .child(field("Applications", &self.mode_inputs.applications))
            })
            .child(settings_row("Provider", "Cleanup backend", backends))
            .when(selected_backend != CleanupBackend::OpenCode, |panel| {
                panel.children(self.mode_inputs.connection.render())
            })
            .child(field("Model ID", &self.mode_inputs.model))
            .when(selected_backend != CleanupBackend::OpenCode, |panel| {
                panel
                    .child(settings_row(
                        "Thinking",
                        "Default uses provider settings; model support varies",
                        thinking_control,
                    ))
                    .when(matches!(thinking, Thinking::Budget(_)), |panel| {
                        panel.child(field("Thinking tokens", &self.mode_inputs.thinking_budget))
                    })
            })
            .child(field("Deadline (seconds)", &self.mode_inputs.deadline))
            .when(selected_backend != CleanupBackend::OpenCode, |panel| {
                panel.child(
                    settings_row(
                        "Stream response",
                        "Receive cleanup text incrementally; paste once complete",
                        toggle(if streaming { 1.0 } else { 0.0 }),
                    )
                    .id("cleanup-streaming")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mode_mut().stream = !streaming;
                        cx.notify();
                    })),
                )
            })
            .child(
                div()
                    .px_4()
                    .py_3()
                    .child(self.mode_inputs.instructions.clone()),
            )
            .child(
                settings_row(
                    "Clipboard context",
                    "Send text copied before this recording",
                    toggle(if clipboard { 1.0 } else { 0.0 }),
                )
                .id("context-clipboard")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode_mut().context.clipboard = !clipboard;
                    cx.notify();
                })),
            )
            .child(
                settings_row(
                    "Active field context",
                    "Send focused field text exposed by Accessibility",
                    toggle(if active_field { 1.0 } else { 0.0 }),
                )
                .id("context-field")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode_mut().context.active_field = !active_field;
                    cx.notify();
                })),
            )
            .child(
                settings_row(
                    "Screen context",
                    "Local OCR sends text; screenshot sends image",
                    segmented_control().children(
                        [
                            ("Off", ScreenContextMode::Off),
                            ("Local OCR", ScreenContextMode::Ocr),
                            ("Screenshot", ScreenContextMode::Screenshot),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, mode))| {
                            let available = mode != ScreenContextMode::Screenshot
                                || selected_backend != CleanupBackend::OpenCode;
                            segmented_item(screen_context == mode)
                                .id(("screen-context", index))
                                .child(label)
                                .when(!available, |item| item.opacity(0.4))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if available {
                                        this.mode_mut().context.screen = mode;
                                        cx.notify();
                                    }
                                }))
                        }),
                    ),
                )
                .id("context-screen"),
            )
            .when(screen_context == ScreenContextMode::Ocr, |panel| {
                panel.child(settings_row(
                    "OCR languages",
                    "Requires Tesseract and installed language data",
                    div()
                        .w(px(330.0))
                        .min_w_0()
                        .child(self.mode_inputs.ocr_languages.clone()),
                ))
            })
            .when(self.selected_mode.is_some(), |panel| {
                panel.child(settings_row(
                    "Remove mode",
                    "Return matching applications to Global",
                    header_button("Remove")
                        .id("remove-pipeline-mode")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(index) = this.selected_mode.take() {
                                this.configuration.processing.modes.remove(index);
                            }
                            this.mode_inputs = ModeInputs::new(this.mode(), cx);
                            cx.notify();
                        })),
                ))
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("Models & Processing", Some(save)))
            .child(
                pane_body().child(
                    div()
                        .id("provider-settings-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .px_8()
                        .pb_7()
                        .child(
                            div().w_full().flex().justify_center().child(
                                pane_content()
                                    .when_some(
                                        self.error.clone().or(self.service_error.clone()),
                                        |pane, error| {
                                            pane.child(error_message("Provider settings", error))
                                        },
                                    )
                                    .child(settings_section_label("TRANSCRIPTION"))
                                    .child(speech)
                                    .child(settings_section_label("PROCESSING MODES"))
                                    .child(modes)
                                    .child(div().mt_3().child(cleanup)),
                            ),
                        ),
                ),
            )
    }
}

fn input(
    placeholder: &'static str,
    initial: &str,
    cx: &mut Context<ProviderEditor>,
) -> Entity<TextInput> {
    cx.new(|cx| TextInput::new(cx, placeholder, initial))
}

fn field(label: &'static str, input: &Entity<TextInput>) -> AnyElement {
    settings_row(label, "", div().w(px(330.0)).min_w_0().child(input.clone())).into_any_element()
}
