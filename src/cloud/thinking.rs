use color_eyre::eyre::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::GenerationProtocol;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    #[default]
    Default,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Budget(u32),
}

impl Thinking {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Off => "Off",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "X-high",
            Self::Max => "Max",
            Self::Budget(_) => "Token budget",
        }
    }

    pub fn options(protocol: GenerationProtocol, model: &str) -> &'static [Self] {
        use Thinking::*;
        if protocol != GenerationProtocol::Gemini {
            return &[Default, Off, Minimal, Low, Medium, High, XHigh, Max];
        }
        let model = model.trim_start_matches("models/");
        if model.starts_with("gemini-2.5-") {
            return if model.contains("-pro") {
                &[Default, Budget(1024)]
            } else {
                &[Default, Off, Budget(1024)]
            };
        }
        if model.starts_with("gemini-3.1-pro") {
            &[Default, Low, High]
        } else if model.starts_with("gemini-3.8-") || model.starts_with("gemini-4-") {
            &[Default, Low, Medium, High]
        } else {
            // Aliases and custom gateways determine their own model support.
            &[Default, Minimal, Low, Medium, High]
        }
    }

    pub fn validate(self, protocol: GenerationProtocol, model: &str) -> Result<()> {
        let choices = Self::options(protocol, model);
        if let Self::Budget(tokens) = self {
            let (minimum, maximum) = if model.contains("-pro") {
                (128, 32768)
            } else if model.contains("flash-lite") {
                (512, 24576)
            } else {
                (1, 24576)
            };
            if !choices
                .iter()
                .any(|choice| matches!(choice, Self::Budget(_)))
                || !(minimum..=maximum).contains(&tokens)
            {
                bail!("Thinking token budget must be {minimum}–{maximum} for a Gemini 2.5 model");
            }
        } else if !choices.contains(&self) {
            bail!(
                "{} thinking is unavailable for this cleanup provider/model",
                self.label()
            );
        }
        Ok(())
    }

    pub(super) fn effort(self) -> Option<&'static str> {
        match self {
            Self::Default | Self::Budget(_) => None,
            Self::Off => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    pub(super) fn gemini_config(self) -> Value {
        match self {
            Self::Off => json!({"thinkingBudget": 0}),
            Self::Budget(tokens) => json!({"thinkingBudget": tokens}),
            _ => json!({"thinkingLevel": self.effort()}),
        }
    }
}
