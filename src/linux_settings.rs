use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use x11rb::protocol::xproto::ModMask;
use x11rb::rust_connection::RustConnection;

use crate::linux_input::{
    Keymap, XK_ALT_L, XK_ALT_R, XK_CONTROL_L, XK_CONTROL_R, XK_SHIFT_L, XK_SHIFT_R, XK_SUPER_L,
    XK_SUPER_R,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct LinuxSettings {
    pub schema_version: u32,
    pub dictation_hotkey: LinuxHotkey,
    pub double_tap_lock: bool,
    #[serde(default = "legacy_paste_with_shift")]
    pub paste_with_shift: bool,
    pub sound_effect_volume: f32,
    pub release_microphone_while_idle: bool,
    pub pause_media_while_recording: bool,
    pub transcription: crate::transcription_models::TranscriptionSelection,
    pub cloud_transcription: Option<crate::cloud::TranscriptionSettings>,
    pub processing: crate::linux_pipeline::PipelineSettings,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderConfiguration {
    pub cloud_transcription: Option<crate::cloud::TranscriptionSettings>,
    pub processing: crate::linux_pipeline::PipelineSettings,
}

impl ProviderConfiguration {
    pub fn validate(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > 96 * 1024 {
            return Err(eyre!("Provider configuration exceeds 96 KiB"));
        }
        if let Some(cloud) = &self.cloud_transcription {
            cloud.validate()?;
        }
        self.processing.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LinuxHotkey {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}

impl Default for LinuxSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            dictation_hotkey: LinuxHotkey::default(),
            double_tap_lock: true,
            paste_with_shift: false,
            sound_effect_volume: 0.5,
            release_microphone_while_idle: false,
            pause_media_while_recording: false,
            transcription: crate::transcription_models::TranscriptionSelection::default(),
            cloud_transcription: None,
            processing: Default::default(),
        }
    }
}

fn legacy_paste_with_shift() -> bool {
    // Existing X11 settings predate this preference and always used Ctrl-Shift-V.
    true
}

impl Default for LinuxHotkey {
    fn default() -> Self {
        Self {
            control: false,
            alt: true,
            shift: false,
            super_key: false,
            key: "space".into(),
        }
    }
}

impl LinuxSettings {
    pub fn load() -> Result<Self> {
        let path = settings_path()?;
        if !path.exists() {
            let Some(seed_path) = std::env::var_os("HEX_INITIAL_SETTINGS_FILE") else {
                return Ok(Self::default());
            };
            let mut seed = Vec::new();
            File::open(&seed_path)?
                .take(112 * 1024 + 1)
                .read_to_end(&mut seed)?;
            if seed.len() > 112 * 1024 {
                return Err(eyre!("Initial Linux settings exceed 112 KiB"));
            }
            let settings = Self::from_initial_settings(&seed)?;
            settings.save()?;
            return Ok(settings);
        }
        let settings: Self = serde_json::from_slice(&fs::read(&path)?)
            .wrap_err_with(|| format!("could not parse {}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    fn from_initial_settings(seed: &[u8]) -> Result<Self> {
        fn merge(defaults: &mut serde_json::Value, seed: serde_json::Value) {
            match (defaults, seed) {
                (serde_json::Value::Object(defaults), serde_json::Value::Object(seed)) => {
                    for (key, value) in seed {
                        merge(
                            defaults.entry(key).or_insert(serde_json::Value::Null),
                            value,
                        );
                    }
                }
                (defaults, seed) => *defaults = seed,
            }
        }

        let seed: serde_json::Value = serde_json::from_slice(seed)?;
        if !seed.is_object() {
            return Err(eyre!("Initial Linux settings must be a JSON object"));
        }
        let mut defaults = serde_json::to_value(Self::default())?;
        merge(&mut defaults, seed);
        let settings: Self = serde_json::from_value(defaults)?;
        settings.validate()?;
        Ok(settings)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > 112 * 1024 {
            return Err(eyre!("Linux settings exceed the service snapshot limit"));
        }
        self.dictation_hotkey.validate()?;
        crate::transcription_models::validate(&self.transcription)?;
        if let Some(cloud) = &self.cloud_transcription {
            cloud.validate()?;
        }
        self.processing.validate()?;
        if !(0.0..=1.0).contains(&self.sound_effect_volume) {
            return Err(eyre!("Sound volume must be between zero and one"));
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = settings_path()?;
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)?;
        let partial = path.with_extension("json.partial");
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&partial)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(partial, &path)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }
}

impl LinuxHotkey {
    pub fn validate(&self) -> Result<()> {
        if self.key.trim().is_empty() {
            return Err(eyre!("Linux hotkeys require a non-modifier key"));
        }
        Ok(())
    }

    pub fn label(&self) -> String {
        self.keycaps().join("+")
    }

    pub fn keycaps(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.control {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.super_key {
            parts.push("Super".to_string());
        }
        let key = if self.key == "space" {
            "Space".into()
        } else {
            self.key.to_ascii_uppercase()
        };
        parts.push(key);
        parts
    }

    pub(crate) fn modifier_mask(
        &self,
        connection: &RustConnection,
        keymap: &Keymap,
    ) -> Result<ModMask> {
        let mut result = ModMask::default();
        for (enabled, symbols) in [
            (self.control, &[XK_CONTROL_L, XK_CONTROL_R][..]),
            (self.alt, &[XK_ALT_L, XK_ALT_R][..]),
            (self.shift, &[XK_SHIFT_L, XK_SHIFT_R][..]),
            (self.super_key, &[XK_SUPER_L, XK_SUPER_R][..]),
        ] {
            if enabled {
                result |= keymap.modifier_for(connection, symbols)?;
            }
        }
        Ok(result)
    }
}

fn settings_path() -> Result<PathBuf> {
    Ok(crate::app_paths::support_dir()?.join("linux-settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_settings_merge_nested_fields_with_fresh_defaults() {
        let settings = LinuxSettings::from_initial_settings(
            br#"{"dictation_hotkey":{"alt":false,"super_key":true},"double_tap_lock":false}"#,
        )
        .unwrap();
        assert_eq!(settings.dictation_hotkey.label(), "Super+Space");
        assert!(!settings.double_tap_lock);
        assert!(!settings.paste_with_shift);
        assert_eq!(settings.sound_effect_volume, 0.5);
        assert_eq!(
            LinuxSettings::from_initial_settings(b"{}").unwrap(),
            LinuxSettings::default()
        );
    }

    #[test]
    fn invalid_initial_settings_are_rejected() {
        for seed in [
            "null",
            "[]",
            "{",
            r#"{"sound_effect_volume":2}"#,
            r#"{"dictation_hotkey":{"key":""}}"#,
            r#"{"processing":{"default_mode":{"deadline_seconds":0}}}"#,
        ] {
            assert!(
                LinuxSettings::from_initial_settings(seed.as_bytes()).is_err(),
                "{seed}"
            );
        }
    }

    #[test]
    fn default_binding_is_alt_space() {
        assert_eq!(
            LinuxSettings::default().dictation_hotkey.label(),
            "Alt+Space"
        );
    }

    #[test]
    fn modifier_only_bindings_are_rejected() {
        let binding = LinuxHotkey {
            key: String::new(),
            ..LinuxHotkey::default()
        };
        assert!(binding.validate().is_err());
    }

    #[test]
    fn standalone_function_keys_are_accepted() {
        let binding = LinuxHotkey {
            alt: false,
            key: "f12".into(),
            ..LinuxHotkey::default()
        };
        assert_eq!(binding.label(), "F12");
        assert!(binding.validate().is_ok());
    }

    #[test]
    fn settings_without_transcription_keep_the_default_selection() {
        let settings: LinuxSettings = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "platform": "x11",
                "dictation_hotkey": {
                    "control": false,
                    "alt": true,
                    "shift": false,
                    "super_key": false,
                    "key": "space"
                },
                "double_tap_lock": true
            }"#,
        )
        .unwrap();

        assert_eq!(
            settings.transcription,
            crate::transcription_models::TranscriptionSelection::default()
        );
    }

    #[test]
    fn legacy_settings_are_shared_between_display_sessions() {
        let legacy = r#"{"platform":"x11","double_tap_lock":false}"#;
        let settings: LinuxSettings = serde_json::from_str(legacy).unwrap();
        assert!(!settings.double_tap_lock);
        assert!(settings.paste_with_shift);
        // Display selection is runtime state, not a preference that survives logout.
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("platform").is_none());
        assert_eq!(
            serde_json::from_value::<LinuxSettings>(saved).unwrap(),
            settings
        );
    }

    #[test]
    fn fresh_settings_use_standard_paste_and_preserve_an_explicit_choice() {
        let settings = LinuxSettings::default();
        assert!(!settings.paste_with_shift);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            serde_json::from_value::<LinuxSettings>(saved).unwrap(),
            settings
        );
        let legacy: LinuxSettings = serde_json::from_str(r#"{"paste_with_shift":false}"#).unwrap();
        assert!(!legacy.paste_with_shift);
    }

    #[test]
    fn recording_volume_defaults_for_new_and_legacy_settings_and_round_trips() {
        assert_eq!(LinuxSettings::default().sound_effect_volume, 0.5);
        let legacy: LinuxSettings = serde_json::from_str(r#"{"platform":"x11"}"#).unwrap();
        assert_eq!(legacy.sound_effect_volume, 0.5);
        for volume in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let settings = LinuxSettings {
                sound_effect_volume: volume,
                ..Default::default()
            };
            settings.validate().unwrap();
            let saved = serde_json::to_vec(&settings).unwrap();
            assert_eq!(
                serde_json::from_slice::<LinuxSettings>(&saved).unwrap(),
                settings
            );
        }
    }

    #[test]
    fn invalid_sound_volume_is_rejected_before_saving() {
        for volume in [-0.1, 1.1, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let settings = LinuxSettings {
                sound_effect_volume: volume,
                ..Default::default()
            };
            assert!(settings.save().is_err());
        }
    }
}
