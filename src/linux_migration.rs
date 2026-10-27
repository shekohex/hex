//! One-time migration of the Linux beta's `linux-settings.json` into the
//! shared `settings.json`. Only [`legacy_settings`] reads this format.

use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::app_settings::{AppSettings, HotkeyBinding};
use crate::linux_input::LinuxHotkey;

#[derive(Deserialize, Serialize)]
#[serde(default)]
struct LegacySettings {
    dictation_hotkey: LinuxHotkey,
    double_tap_lock: bool,
    shortcut_mode: crate::dictation::ShortcutMode,
    /// Files that predate this preference always pasted with Ctrl-Shift-V.
    paste_with_shift: bool,
    sound_effect_volume: f32,
    transcription: crate::transcription_models::TranscriptionSelection,
    release_microphone_while_idle: bool,
    pause_media_while_recording: bool,
    cloud_transcription: Option<crate::cloud::TranscriptionSettings>,
    processing: crate::linux_pipeline::PipelineSettings,
}

impl Default for LegacySettings {
    fn default() -> Self {
        Self {
            dictation_hotkey: LinuxHotkey::default(),
            double_tap_lock: true,
            shortcut_mode: crate::dictation::ShortcutMode::Hold,
            paste_with_shift: true,
            sound_effect_volume: 0.5,
            release_microphone_while_idle: false,
            pause_media_while_recording: false,
            cloud_transcription: None,
            processing: Default::default(),
            transcription: crate::transcription_models::TranscriptionSelection::default(),
        }
    }
}

impl LegacySettings {
    fn apply_to(self, settings: &mut AppSettings) {
        settings.dictation_hotkey = HotkeyBinding::from(&self.dictation_hotkey);
        settings.double_tap_lock = self.double_tap_lock;
        settings.shortcut_mode = self.shortcut_mode;
        settings.paste_with_shift = self.paste_with_shift;
        // Muting keeps the remembered volume for when sounds return.
        let volume = self.sound_effect_volume;
        settings.sound_effects = volume > 0.0;
        if volume > 0.0 && volume <= 1.0 {
            settings.sound_effect_volume = volume;
        }
        settings.transcription = self.transcription;
        settings.release_microphone_while_idle = self.release_microphone_while_idle;
        if self.pause_media_while_recording {
            settings.recording_audio_behavior =
                crate::app_settings::RecordingAudioBehavior::PauseMedia;
        }
        settings.cloud_transcription = self.cloud_transcription;
        settings.processing = self.processing;
    }
}

/// Nix's original Linux seed shape, merged with fresh-install defaults.
/// Existing shared or legacy settings always win before this is called.
pub(crate) fn initial_settings(bytes: &[u8]) -> Result<AppSettings> {
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
    let seed: serde_json::Value = serde_json::from_slice(bytes)?;
    if !seed.is_object() {
        return Err(color_eyre::eyre::eyre!(
            "Initial Linux settings must be a JSON object"
        ));
    }
    let fresh = LegacySettings {
        paste_with_shift: false,
        ..Default::default()
    };
    let mut defaults = serde_json::to_value(fresh)?;
    merge(&mut defaults, seed);
    let legacy: LegacySettings = serde_json::from_value(defaults)?;
    legacy.dictation_hotkey.validate()?;
    if !(0.0..=1.0).contains(&legacy.sound_effect_volume) {
        return Err(color_eyre::eyre::eyre!(
            "Sound volume must be between zero and one"
        ));
    }
    crate::transcription_models::validate(&legacy.transcription)?;
    let mut settings = AppSettings::default();
    legacy.apply_to(&mut settings);
    settings.validate_providers()?;
    Ok(settings)
}

/// The legacy `linux-settings.json` in `directory`, converted, for a
/// one-time migration into `settings.json`.
pub(crate) fn legacy_settings(directory: &Path) -> Result<Option<(PathBuf, AppSettings)>> {
    let path = directory.join("linux-settings.json");
    if !path.exists() {
        return Ok(None);
    }
    let legacy: LegacySettings = serde_json::from_slice(&fs::read(&path)?)
        .wrap_err_with(|| format!("could not parse {}", path.display()))?;
    let mut settings = AppSettings::default();
    legacy.apply_to(&mut settings);
    Ok(Some((path, settings)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "hex-linux-migration-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn migrated(legacy: &str) -> AppSettings {
        let mut settings = AppSettings::default();
        serde_json::from_str::<LegacySettings>(legacy)
            .unwrap()
            .apply_to(&mut settings);
        settings
    }

    #[test]
    fn fork_provider_settings_survive_upgrade_and_round_trip() {
        let settings = migrated(
            r#"{
            "release_microphone_while_idle":true,
            "pause_media_while_recording":true,
            "cloud_transcription":{"model":"gateway-asr","mode":"realtime","server_vad":true,"connection":{"base_url":"http://127.0.0.1:9000/v1","api_key_env":"HEX_SPEECH_KEY"}},
            "processing":{"default_mode":{"enabled":true,"backend":"responses","model":"cleanup","thinking":"high","context":{"active_field":true,"screen":"ocr","ocr_languages":"eng+ara"}},"modes":[{"name":"Editor","applications":["code"],"replacements":[{"matched_phrase":"hex app","output":"HEX"}]}]}
        }"#,
        );
        settings.validate_providers().unwrap();
        let cloud = settings.cloud_transcription.as_ref().unwrap();
        assert_eq!(cloud.model, "gateway-asr");
        assert_eq!(cloud.mode, crate::cloud::TranscriptionMode::Realtime);
        assert!(cloud.server_vad);
        assert!(settings.release_microphone_while_idle);
        assert_eq!(
            settings.recording_audio_behavior,
            crate::app_settings::RecordingAudioBehavior::PauseMedia
        );
        assert_eq!(
            settings.processing.default_mode.thinking,
            crate::cloud::Thinking::High
        );
        assert_eq!(settings.processing.modes[0].applications, ["code"]);
        let restored: AppSettings =
            serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
    }

    #[test]
    fn initial_settings_keep_fresh_defaults_and_merge_nested_keys() {
        let settings = initial_settings(br#"{"dictation_hotkey":{"alt":false,"super_key":true},"pause_media_while_recording":true}"#).unwrap();
        assert_eq!(settings.dictation_hotkey.keycaps().join("+"), "Super+Space");
        assert!(!settings.paste_with_shift);
        assert_eq!(settings.sound_effect_volume, 0.5);
        assert_eq!(
            settings.recording_audio_behavior,
            crate::app_settings::RecordingAudioBehavior::PauseMedia
        );
        for invalid in [
            "null",
            "[]",
            "{",
            r#"{"sound_effect_volume":2}"#,
            r#"{"processing":{"default_mode":{"deadline_seconds":0}}}"#,
        ] {
            assert!(initial_settings(invalid.as_bytes()).is_err(), "{invalid}");
        }
    }

    #[test]
    fn legacy_linux_settings_migrate_once_into_settings_json() {
        let directory = scratch_directory("once");
        fs::write(
            directory.join("linux-settings.json"),
            br#"{"schema_version":1,"platform":"x11","dictation_hotkey":{"control":true,"alt":false,"shift":false,"super_key":true,"key":"f12"},"double_tap_lock":false,"shortcut_mode":"toggle","sound_effect_volume":0.25}"#,
        )
        .unwrap();
        let path = directory.join("settings.json");

        crate::app_settings::migrate_legacy_settings(&path).unwrap();

        assert!(!directory.join("linux-settings.json").exists());
        assert!(directory.join("linux-settings.json.migrated").is_file());
        let migrated: AppSettings = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            migrated.dictation_hotkey.keycaps().join("+"),
            "Ctrl+Super+F12"
        );
        assert!(!migrated.double_tap_lock);
        assert_eq!(
            migrated.shortcut_mode,
            crate::dictation::ShortcutMode::Toggle
        );
        // Settings that predate the preference keep the terminal paste.
        assert!(migrated.paste_with_shift);
        assert!(migrated.sound_effects);
        assert_eq!(migrated.sound_effect_volume, 0.25);

        // A second launch reads settings.json and leaves the backup alone.
        fs::write(directory.join("linux-settings.json"), b"{}").unwrap();
        crate::app_settings::migrate_legacy_settings(&path).unwrap();
        assert!(directory.join("linux-settings.json").is_file());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn an_english_beta_moves_to_the_streaming_model_only_when_installed() {
        let directory = scratch_directory("streaming");
        fs::write(directory.join("linux-settings.json"), br#"{"transcription": {"model": "parakeet_v3", "language": "en", "recognition_hints": ""}}"#).unwrap();
        let path = directory.join("settings.json");
        crate::app_settings::migrate_legacy_settings(&path).unwrap();
        let settings = AppSettings::load_from(&path).unwrap();
        use crate::transcription_models::{TranscriptionModelId, definition, is_installed};
        let expected = if is_installed(
            definition(TranscriptionModelId::ParakeetUnifiedEnglish),
            "en",
        ) {
            TranscriptionModelId::ParakeetUnifiedEnglish
        } else {
            TranscriptionModelId::ParakeetV3
        };
        assert_eq!(settings.transcription.model, expected);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn files_that_predate_a_setting_keep_the_beta_defaults() {
        let settings = migrated("{}");
        assert_eq!(settings.dictation_hotkey.keycaps().join("+"), "Alt+Space");
        assert!(settings.double_tap_lock);
        assert_eq!(settings.shortcut_mode, crate::dictation::ShortcutMode::Hold);
        assert!(settings.paste_with_shift);
        assert_eq!(
            settings.transcription,
            crate::transcription_models::TranscriptionSelection::default()
        );
        assert!(!migrated(r#"{"paste_with_shift":false}"#).paste_with_shift);
    }

    #[test]
    fn a_muted_beta_keeps_the_remembered_volume() {
        let settings = migrated(r#"{"sound_effect_volume":0.0}"#);
        assert!(!settings.sound_effects);
        assert_eq!(settings.sound_effect_volume, 0.5);
    }
}
