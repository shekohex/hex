# HEX Feature Map

What can the user do, how do they reach it, and what would prove it works?
This maps existing behavior. [ROADMAP.md](../../ROADMAP.md) owns future work;
[AGENTS.md](../../AGENTS.md) owns implementation invariants.

Initial source baseline: September 1, 2026, `9e9da53aa9ed`. Public macOS release:
[2.2.0](../releases/2.2.0.md), with custom-domain downloads, a GitHub DMG mirror,
and model-download recovery guidance. The initial map was source-only. Listed checks are
locators unless an executed result is explicitly recorded, as in the
[keyboard-layout verification](recovery.md#keyboard-layout-resolution).

## Product Map

```ts
HEX                                      // macOS release
├── Get ready                            // install, permissions, local model
├── Dictate                              // hold OR double-tap lock
├── Use the result                       // paste, Send, paste-last, History
├── Adjust behavior                      // shortcut, microphone, recording preferences
├── Process text                         // context-selected Modes
├── Opt in                               // Voice Action, Commands, Meetings: separately off
├── Recover                              // prove the next attempt works
└── Maintain the app                     // menu bar, login, updates, quit
```

[Dictation](dictation.md), [Voice Commands](commands.md),
[Voice Action](voice-action.md), [Meetings](meetings.md), and [Recovery](recovery.md)
have detailed maps.
The other capabilities stay here until they need their own verification recipe.

## Get Ready

Install the signed macOS app with `brew install --cask anomalyco/tap/hex` or the
[manual DMG](../../README.md#install-hex). The cask uses a versioned, checksum-pinned
DMG, requires Apple silicon/macOS 15+, and keeps Sparkle updates enabled. It does
not require Bun or OpenCode, launch the app automatically, migrate the legacy
Swift app, or forcibly replace an existing app destination. Homebrew uninstall
leaves runtime settings, models, and retained data intact.

```ts
Download Hex                              // setup.download-recovery
├── Website / README / Homebrew -> downloads.hex.kitlangton.dev -> R2 bytes
├── Alternative download: GitHub -> app-v<version> -> identical signed DMG
└── Older app cannot reach its update feed -> manually install the DMG

Prepare local models                      // separate network dependency
├── Dictation -> huggingface.co and its download CDN
└── Commands -> download.moonshine.ai
    └── Transfer fails -> host-specific guidance -> restore access -> Retry
```

The website shows the pinned macOS version and platform requirements beside its
download links. App mirrors use explicit `app-v…` release tags rather than GitHub's
repository-wide latest release, which may refer to the SDK. The legacy `r2.dev`
endpoint remains available for installed clients. New builds use the custom host
for Sparkle and Linux updates; the signed Linux payload and verification key are
unchanged. Hosting does not guarantee access through every corporate policy.

The `.dev` site's decorative wind-up teeth have feet and hop around desktop
margins, or above the copy on narrow screens. A warm spotlight fades in first
(1.4–2.4 seconds). After three seconds hidden, the toy hops in from the left to
the center above the headline; the light fades as roaming begins. The canvas passes clicks through;
the website has no microphone button or microphone access. Reduced motion reveals
still light and toy poses at their respective delays. Rounded teeth, teal irises,
enamel, lacquer, and metal materials use local softbox reflections and soft shadows.
Its Open Graph image uses the same model; see
[site authoring](../../site/README.md). Desktop/mobile animation frames were
inspected, and reduced-motion canvas captures were pixel-identical four seconds
apart in Chromium. These checks do not exercise a live microphone.

Sources: [website](../../site/src/App.tsx), [mirror publisher](../../scripts/publish-app-mirror.sh),
[macOS release publisher](../../scripts/release-app.sh),
[Linux installer](../../scripts/install-linux-release.sh), and
[Linux updater](../../src/linux_updater.rs). The macOS publisher verifies the
downloaded R2 artifacts and GitHub mirror before publishing the appcast.
Model transfer failures in [transcription_models.rs](../../src/transcription_models.rs)
and [moonshine.rs](../../src/moonshine.rs) explain the source host, retry, and
separate prerequisites. These messages do not diagnose a particular firewall
or establish a successful subsequent model download.
Executed hosting checks and deployment details are recorded in
[Download Hosting](../downloads.md). In-app feed defaults and recovery copy ship
with 2.1.17; the initial 2.1.16 mirror preserves that release's original bytes.

```ts
Launch Hex                               // setup
  -> Grant Microphone / Input Monitoring / Accessibility
  -> Choose language and local model
  -> Ready for ordinary dictation         // no Bun, OpenCode, or Commands required

Settings > Local transcription
  -> Prepare candidate
     ├── Success -> Save selection
     └── Failure -> Preserve previous selection and show error

Permissions revoked later
  -> Settings shows the missing access and repair action
```

Apple silicon, macOS 15+. Choices come from the compiled model/language catalog;
Apple Speech is currently excluded from the desktop picker.

```ts
Choose a supported language              // setup.language-conditioning; macOS + Linux picker
├── Parakeet v3 -> Detect language from audio; selected language does not constrain output
└── Whisper -> Use selected language to guide decoding; accuracy is not guaranteed
```

Parakeet v3 remains available for Portuguese and its other existing language
choices. Its card explains the limitation and suggests Whisper for
language-guided transcription. This does not change saved selections or expand
the Auto picker choices. Multilingual audio support is not language conditioning:
the pinned v3 GGUF has no `stt.parakeet.prompt.num_prompts` metadata, which gates
language prompting in `transcribe-cpp-sys` 0.1.3.

Sources: [model catalog](../../src/transcription_models.rs),
[shared picker](../../src/desktop_transcription_picker.rs), and the runtime
options in [parakeet.rs](../../src/parakeet.rs) and
[linux_transcriber.rs](../../src/linux_transcriber.rs). The regression
`parakeet_v3_supports_portuguese_without_language_conditioning` checks that the
Portuguese selection remains valid and available, sends no runtime language
hint, and contrasts Whisper's `pt` hint. It does not run inference.
The regression was observed failing with the old metadata and passing after the
correction. The macOS release picker preview built, but screenshot capture was
blocked by missing Screen Recording access; visual layout remains unverified.

**Unreproduced recognition report:** [#68](https://github.com/anomalyco/hex/issues/68)
reports Portuguese speech misrecognized as English. Source and pinned-model
metadata inspection establish the prompting limitation, not the cause or repair
of those transcripts. No v3 audio reproduction or transcription-quality fix has
been demonstrated.

Checks start in [onboarding.rs](../../src/onboarding.rs),
[transcription.rs](../../src/transcription.rs), and
[transcription_models.rs](../../src/transcription_models.rs). Setup/picker previews
prove rendering, not clean-account permission grants or model preparation.

```ts
Menu bar > Transcription Model            // setup.quick-model-switch; macOS
├── Downloaded models -> Last selected language shown beside each model
├── Choose -> Verify installed artifact -> Prewarm -> Save selection
│   ├── Success -> Checkmark moves; Settings reflects the saved selection
│   └── Failure -> Previous selection remains; open Settings for the error
├── Preparing -> Cancel Model Switch      // never cancels dictation/output jobs
└── Manage Models… -> Settings picker      // downloads and language changes
```

The menu does not open Settings merely to switch models, download a missing model,
or replace a corrupt artifact. Each model retains one last-used language/hints
selection in `transcription_recents`; the current selection takes precedence.
Older settings start with no remembered alternatives. Select an alternative's
language in Settings once to establish it. No additional model is loaded at startup
just to populate the menu, and the checkmark means selected, not live-worker readiness.

[transcription_preparation.rs](../../src/transcription_preparation.rs) is shared
by Settings and the menu bar: one worker plus one replaceable pending choice.
Closing Settings does not abandon preparation. A superseded/cancelled completion
cannot save, and delivered menu controls are drained before accepting completions.
The desktop root commits through an existing editor to retain unrelated unsaved
settings, or through persisted settings when no window exists.

Sources: [status_item.rs](../../src/status_item.rs),
[meeting_watcher.rs](../../src/meeting_watcher.rs), and
[app_window.rs](../../src/app_window.rs). Key checks are
`preparation_has_one_worker_and_only_keeps_the_latest_pending_choice`,
`cancelling_a_completed_unpolled_preparation_never_commits_it`,
`model_switches_remember_language_and_hints_without_changing_other_settings`, and
`failed_model_selection_save_preserves_selection_recents_and_unsaved_edits` in
the preparation owner and [app_settings.rs](../../src/app_settings.rs).
Additional checks cover worker failure/retry, installed-only checksum verification,
and menu choice/tag projection. These do not establish a physical menu click or
an installed-app switch. Remembering a language does not change the language
conditioning limitations described above.

**Observed September 4, 2026 (2.1.16):** all 455 Rust tests
passed in debug and release, with ten opt-in tests skipped per profile. All twelve
keyboard-layout child scenarios passed per profile; formatting, strict
all-target/all-feature Clippy in both profiles, and the release build passed.
The Cohere fixture inference check was run separately; see
[dictation model windows](dictation.md#ongoing-jobs-and-output). Native menu
interaction, installed-app switching, and Linux runtime behavior were not exercised.

```ts
Settings picker > Downloaded model card   // setup.delete-model; macOS
├── Delete -> "Delete <size>?" -> Delete again
│   ├── Success -> Model file, receipt, and partial download removed; card offers Download
│   └── Download in progress -> Nothing removed; "Model could not be deleted." with reason
├── Selected dictation model (any language) -> No Delete
├── Preparing or Apple Speech model -> No Delete
└── Language change, model choice, or closing the picker -> Confirmation cleared
```

Deletion takes the shared model-directory download lock without waiting, so it
cannot race a Settings, menu-bar, or local API download. The menu bar drops a
deleted model because it reads installation state when it builds the menu. Linux
Settings does not offer deletion: the service owns model files and has no delete
request yet.

Sources: [shared picker](../../src/desktop_transcription_picker.rs),
`remove_installed` in [transcription_models.rs](../../src/transcription_models.rs),
and [app_window.rs](../../src/app_window.rs). Checks are
`removing_a_model_deletes_its_artifacts_unless_a_download_is_active` (real files
and lock) and `model_deletion_confirms_first_and_spares_the_dictation_model`
(preview state, no disk access). Preview: `voice-control preview
transcription-picker --model-state installed --confirm-model-deletion`.

**Known gap:** `is_installed` checks size, not checksum. Not every native-load
path requires verification. Picker Active means selected/installed, not live
worker readiness; a later worker-load failure does not roll back saved selection.

## Dictate

```ts
Saved shortcut                           // dictation; Option by default
├── Hold -> Speak -> Release
├── Tap, tap -> Speak hands-free -> Press again   // Double-tap to lock enabled
└── Escape
    ├── Capture active -> Cancel that capture
    └── Otherwise -> Cancel newest cancellable job

Settings > Double-tap only               // optional for key bindings
  -> Disable single-hold activation
  -> Keep the double-tap hands-free path
```

The [detailed map](dictation.md) covers timing, successive captures, output
commitment, named checks, and native-proof gaps. Optional
[spoken entry and controls](commands.md#spoken-dictation-controls) belong to Commands.
The macOS CLI entry is `hex listen`, with an optional authoritative `--device`.
Do not start it beside an existing listener.

## Use The Result

```ts
Completed output                         // output
├── Paste -> Current focus, in submission order
├── Spoken Send -> Paste, then Enter      // requires Commands and voice capture
├── Paste Last Dictation                  // menu or Option-Shift-V / saved binding
│   └── Last completed dictation, not Voice Action
└── History
    ├── Inspect / search / copy / delete
    ├── Retention -> Seven days by default, always hard-capped
    ├── Off -> Stop new entries; preserve existing entries
    └── Clear all -> Explicit deletion
```

Checks in [paste.rs](../../src/paste.rs), [parakeet.rs](../../src/parakeet.rs), and
[history.rs](../../src/history.rs) cover output and retention boundaries.
They do not prove that an arbitrary target consumed its clipboard contents.
Send is not an atomic target-app transaction.

**Easy to misread:** History Off does not stop diagnostic logging. Diagnostic
logs are separate and currently unbounded on disk. See the
[privacy guide](../../README.md#privacy-and-local-data).

Opening or reopening Settings does not read the diagnostic event log. Commands
and developer Activity load it on demand through `AppWindow::reload_events`.
The shared `EventReader` streams a file-length snapshot in 16 KiB chunks rather
than copying the entire log into memory. It retains up to 1,024 event rows and
the latest context, even when that context predates the retained rows.
Diagnostic records over 1 MiB are skipped through their terminating newline;
the original log and dictation output remain intact. Partial UTF-8 records carry
over to the next refresh, and truncation or file replacement resets the reader.

Checks: `activity_log_is_only_read_when_an_observing_pane_needs_it` in
[app_window.rs](../../src/app_window.rs) and the `reader_*` regressions in
[events.rs](../../src/events.rs). These exercise synthetic files, not an
installed Settings session. A first Commands/Activity refresh still scans the
full log snapshot; disk rotation and background parsing remain separate work.

Observed September 26, 2026: the reader and pane-loading regressions passed.
An optimized isolated harness using the production reader and a synthetic
64 MiB log reduced peak process RSS from about 204 MB to 2.4 MB; refresh took
254 ms before and 222 ms after. This is reader evidence, not a native Settings
latency measurement.

## Adjust Behavior

```ts
Settings                                 // settings
├── Dictation shortcut -> Capture a replacement binding
├── Microphone -> Automatic or saved device
│   ├── Automatic -> Compiled preferences, then the macOS default
│   │   └── Default is a Bluetooth headset -> Built-in mic when present // avoids the call profile
│   └── Open menu covers the controls beneath it; click elsewhere dismisses
├── Microphone mode
│   ├── Keep ready (fast) -> Open while idle; pre-roll available
│   └── Release when idle -> Open on press; no pre-roll; startup delay
│       └── Commands enabled? -> Confirm turning Commands off
├── While dictating -> Mute / Pause media / Do nothing
│   ├── Intentional capture only, not ordinary shortcut chords
│   └── Pause/resume only players already reported running by macOS
├── Transcribe during pauses -> On by default; next hotkey capture // dictate.incremental
└── Sound volume -> Immediate feedback setting; zero suppresses tones

Intentional recording                    // recording.environment
├── Acquire native macOS no-idle-sleep assertion
│   └── Failure -> Warn and continue recording
└── Finish/cancel -> Release the assertion
    // no caffeinate child process or executable-policy dialog
```

Successful settings saves persist and project changes at safe runtime boundaries.
Immediate settings controls commit a candidate before changing the saved UI
choice or applying dependent native/history effects. A failed shortcut save
stays in capture instead of showing Saved; failed retention saves keep the old
choice and surface the error. Failed debounced text saves retain their drafts
for a subsequent edit or close-time flush. The regressions
`failed_shortcut_save_stays_in_capture_and_preserves_pending_edits` and
`failed_retention_save_keeps_previous_choice_and_reports_the_error` in
[app_window.rs](../../src/app_window.rs) inject persistence failure in an
in-memory GPUI window; they do not simulate disk failure in an installed app.
Persistence, conflict, and ownership checks live in
[app_settings.rs](../../src/app_settings.rs),
[recording_environment.rs](../../src/recording_environment.rs), and
[audio.rs](../../src/audio.rs). Settings previews do not prove physical device
switching or native mute support; muting is best-effort, not universal. The
recording-environment ownership checks cover overlapping acquisition and release,
not a managed-device policy or native power assertion. The pause-script regression
checks that inactive players are absent before AppleScript application resolution;
when no supported player is running, acquisition skips AppleScript entirely.
The script check does not prove subprocess avoidance or exercise Music, Spotify,
or VLC through Automation. An opt-in native
assertion smoke exercises IOKit without starting microphone capture.

**Fixed after 2.1.22:** Automatic selection opened a Bluetooth headset when it was
the macOS default input, which switches AirPods and similar headsets into a
low-quality call profile for all audio, including music
([#106](https://github.com/anomalyco/hex/issues/106),
[#88](https://github.com/anomalyco/hex/issues/88)). It now prefers a built-in
microphone and keeps the headset only when none exists; an explicitly saved
headset is still opened. `automatic_input_prefers_built_in_over_a_bluetooth_default`
in [audio.rs](../../src/audio.rs) covers the choice, not CoreAudio's transport
report or a connected headset.

**Fixed after 2.1.20:** through 2.1.20 the microphone menu was painted without
occluding the panel, so hovering or clicking a device also reached the setting
under the pointer ([#94](https://github.com/anomalyco/hex/issues/94)), and it
only closed by choosing or reopening. The menu is now anchored to its button,
occludes the controls beneath it, and closes on any outside mouse-down.
`microphone_picker_occludes_the_controls_beneath_it` in
[app_window.rs](../../src/app_window.rs) renders the production menu over a
hoverable, clickable control and fails without the occlusion; it does not prove
a native pointer session. The recognition-hints editor also now uses the compact
multiline height, so it no longer overlaps the shortcut row for Whisper models.
Screen Recording was unavailable, so that layout is unverified visually.

**Easy to misread:** an open microphone is not an active recording. Sleeping
Commands still needs open input; it is not Release when idle.

## Process Text With Modes

```ts
Modes                                    // modes
├── Select a row -> Edit that mode        // does NOT manually activate it
└── Foreground context -> Select processing
    ├── Matching Brave website
    ├── Otherwise matching application
    └── Otherwise Global

Selected mode                            // replaces Global, does not layer over it
  -> Corrections
  -> OpenCode transformation             // optional provider-backed rewrite
  -> Transformations                     // ordered text chain
  -> Paste or Send

Stage fails -> Preserve its input        // failed chain discards partial results
```

Built-in text transformations run natively. Custom transformations require the
managed Bun workspace, not Commands enabled. OpenCode rewriting sends documented
context to the configured provider; speech transcription stays local.

The built-ins are Lowercase, SpongeBob case, and No trailing punctuation. The
last removes only the final contiguous run of sentence marks (including
full-width marks), preserving internal punctuation, trailing whitespace, and
closing quotes/brackets. Punctuation inside a final closing quote is preserved.
Select it after Lowercase for lowercase messages without the final period.
The `no_trailing_punctuation_*` and `built_in_transformations_chain_in_selected_order`
tests in [personal_commands.rs](../../src/personal_commands.rs) cover text behavior
and native dispatch, not a live custom-host or target-app paste.

Selection and stage checks start in
[dictation_processor.rs](../../src/dictation_processor.rs),
[text_replacements.rs](../../src/text_replacements.rs), and
[personal_commands.rs](../../src/personal_commands.rs). They do not prove live
provider availability or real application/Brave context changes.

GUI executable discovery also checks `~/.vite-plus/bin/opencode2` when it is
absent from the inherited PATH. `standard_install_location_finds_opencode_outside_the_gui_path`
in [dictation_processor.rs](../../src/dictation_processor.rs) covers this with
a temporary executable; it does not run an installed Vite Plus shim or provider.

`ordinary_processing_snapshots_modes_before_stage_and_keeps_transformations` in
[parakeet.rs](../../src/parakeet.rs) checks processing-time profile acquisition,
conditional Processing-stage emission, and retained transformation selection.
It uses controlled profiles, not a live provider or custom transformation host.

`mode_rows_preserve_edits_across_add_delete_and_supersede_old_debounces` in
[app_window.rs](../../src/app_window.rs) covers mode-editor changes and stale
debounced saves with one row owning its draft and inputs. It does not exercise
native typing or foreground mode activation.

Application activations compare the picker's bundle name with the foreground
application's localized name. When Finder shows all filename extensions, the
picker name arrives as `Ghostty.app`; [context.rs](../../src/context.rs) strips
that suffix on both sides and [app_settings.rs](../../src/app_settings.rs)
rewrites already persisted activations on load, checked by
`application_matching_ignores_finder_bundle_extensions` and
`loading_strips_finder_bundle_extensions_from_mode_applications`.

**Fixed in 2.1.14:** through 2.1.13, a mode saved with that Finder
preference enabled never activates and every dictation falls back to Global.
Update to repair saved selections automatically. For older builds, the workaround
is to quit HEX, remove the `.app` suffix from
`dictation_processing.modes[].applications` in `settings.json`, then relaunch.

## Use Voice Action

[Voice Action](voice-action.md) maps the separate opt-in, hold-only shortcut,
optional selection, local transcript-to-OpenCode-to-paste path, and cancellation
boundary. It owns the mode-bypass/last-result rules, checks, and
[cold modifier-order gap](voice-action.md#cold-modifier-order).

## Speak Commands

[Voice Commands](commands.md) maps the command opt-in and separate Moonshine
model, awake/sleep state, contextual catalog, personal config, and
[spoken dictation controls](commands.md#spoken-dictation-controls).
It owns the checks and [host/startup gaps](commands.md#gaps-and-constraints);
actual capture and output remain in [Dictation](dictation.md).

## Record Meetings

[Meetings (beta)](meetings.md) maps the Settings opt-in, call detection and the
focus-free offer, menu-bar recording state, rolling transcription, echo
suppression, and the Markdown copy in the user's transcript folder.

## Recover And Maintain

```ts
Cancellation / interruption / failure    // recovery
  -> Scoped stop, fallback, or error
  -> Retry or fresh gesture
  -> Next capture succeeds               // this is the proof, not a cleared error

Menu bar and Settings                    // maintenance
├── Settings / Paste Last -> Reachable with Dock icon hidden
├── Launch at Login -> macOS owns registration state
├── Check for Updates -> Signed packaged-app Sparkle lifecycle
└── Quit -> Orderly shutdown
```

```ts
Launch Hex                               // maintain.startup
├── Setup incomplete / permissions or model missing -> Open setup or Settings
├── Show Dock icon on -> Open Settings
└── Show Dock icon off + usable menu-bar item -> No startup window
    ├── Dictation starts independently of the Settings window
    └── Menu-bar Settings / Finder or Spotlight reopen -> Open or focus the window

Menu-bar installation fails -> Show Dock icon and open the window // recovery access

Show Dock icon off                       // maintain.dock-policy
├── Settings window open -> Dock icon stays // hiding it would deactivate HEX
└── Settings window closes -> Menu-bar only
```

```ts
Settings > Launch at login               // maintain.login-item
├── Open -> Checking until macOS returns the initial state
├── Refresh / toggle / Open Settings -> One persistent background worker
│   └── One active request; latest pending user action precedes the next refresh
├── Failed toggle -> Show error and reconcile with actual macOS state when available
└── Close Settings -> Release channels without waiting for an active native call
```

[login_item.rs](../../src/login_item.rs) serializes ServiceManagement calls on one
window-owned worker, with bounded request and response channels and an autorelease
pool per operation. Settings reads responses without blocking every 500 ms;
refreshes do not accumulate while macOS is busy. The toggle is unavailable until
the first successful status lookup. macOS owns registration state; no login
Boolean is persisted. Preview controls never start the worker or call macOS.

The worker tests in `login_item.rs` exercise thread reuse, nonblocking polling,
latest-action coalescing, action priority, and closing during a blocked call.
`login_item_response_reconciles_optimistic_toggle_and_unknown_state` in
[app_window.rs](../../src/app_window.rs) checks initial status, error rollback
including redraw, unchanged errors, error-only clearing, approval, and external
enablement. Native `RequiresApproval` and error transitions remain unverified;
the simulated checks do not establish Developer ID signed distribution behavior.

Observed September 23, 2026, on `d1930b5` after the final simplifications: the
full debug suite passed 486 tests and the release suite passed 485, each with
twelve opt-in tests ignored and all twelve keyboard-layout child scenarios
passing. Formatting, strict debug all-target/all-feature Clippy, and diff
whitespace checks passed.

Computer-use checks on an ad-hoc signed temporary bundle of the optimized build
confirmed shortcut capture/cancellation, Settings/Modes navigation, registration
and unregistration, the final state after three consecutive toggle clicks, and
refresh after removing that bundle's login item in macOS Settings. The temporary
item was left disabled/removed; no new microphone, input, or Accessibility grants
were made. A five-second `sample` profile found `SMAppService` frames on the
`login-item` worker and none on the main thread. This is sampled-stack evidence,
not an input-to-frame latency measurement or a signed-release smoke test.

Closing Settings left the app process alive with zero `login-item` threads;
reopening produced one new worker. An earlier apparent surviving worker came
from computer-use observation reopening the window. Cmd+Q and Quit HEX each
terminated the process. Local profiles are `/tmp/hex-login-worker-current.sample.txt`
and `/tmp/hex-login-cycle-{before,closed-no-observe,reobserved}.sample.txt`.
The tested temporary executable's SHA-256 is
`d1a3be1e219e7371726273937eeef6c7e16434637e4297ae183f9bdaf4d28b04`.
No Developer ID signing identity was available for a new distribution build.

The existing **Show Dock icon** preference controls quiet startup; there is no
additional launch-window setting. This applies to normal and login launches,
does not change login registration, and does not hide an already open window
when the preference changes. Any open Settings window keeps the Dock icon: macOS
deactivates an app asynchronously when it becomes menu-bar only, which dropped
the window behind other applications ([#103](https://github.com/anomalyco/hex/issues/103)).
Turning the preference off therefore takes effect when Settings closes. Explicit previews always open their requested pane.

Checks in [meeting_watcher.rs](../../src/meeting_watcher.rs):
`dockless_startup_stays_quiet_only_when_setup_and_menu_bar_are_ready` and
`dock_visible_startup_always_opens_the_app` cover the startup decision. The
existing `on_reopen` and status-item `OpenSettings` paths bypass that decision.
These checks do not establish signed-app login behavior or native Finder/Spotlight
reopening; those still need an installed-app smoke test. This addresses the
menu-bar-only case in [#64](https://github.com/anomalyco/hex/issues/64), not an
independent window preference for users keeping the Dock icon visible.

**Executed September 3, 2026:** 441 Rust tests passed (nine opt-in tests ignored),
along with all twelve keyboard-layout child scenarios, formatting, and Clippy.
The release Settings preview built and launched, but window screenshot capture
failed; no visual or installed-app startup verification is claimed.

```ts
Update available                         // maintain.updates
├── Sidebar Update -> Sparkle update dialog
└── Menu bar > Check for Updates -> Same dialog

Remind Me Later -> Dismiss this offer -> Manual update -> Reopen the offer

DisableUpdates = true at launch          // maintain.managed-updates
├── Sparkle never loads -> No background or manual checks, downloads, or installs
├── App menu -> Check for Updates omitted
└── Menu bar -> Disabled "Updates Managed by Your Organization"
```

Administrators set the `DisableUpdates` Boolean in the `com.kitlangton.hex2`
domain, normally through a configuration profile
([#100](https://github.com/anomalyco/hex/issues/100)). HEX reads the effective
`NSUserDefaults` value once at launch, so a forced profile value wins over a local
one and a change applies after relaunch. Speech-model downloads are unaffected.
No automated check covers it; the read is a single preference lookup in
[sparkle.rs](../../src/sparkle.rs).

The sidebar uses window-local deferred action dispatch in
[app_window.rs](../../src/app_window.rs). The executed
`sidebar_update_click_dispatches_only_outside_preview` regression clicks the
production control in an active in-memory GPUI window: zero actions before the
fix, one afterward, and no action in a preview. It does not start Sparkle.

**Fixed in 2.1.13:** older releases through 2.1.12 used app-global dispatch from
inside the sidebar click, losing the action while the active window was borrowed.
The click regression passed in debug and optimized release builds. The menu-bar
path was exercised on the installed local 2.1.11 build and opened the native
Software Update window; no installation was performed. That path remains the
workaround for older versions, including after Remind Me Later. A repaired
installed-sidebar click remains unverified.

The [historical release verification](release-verification.md) records publication
evidence through 2.1.20, including signing, artifact checks, CI results, and skipped
native checks. Those release-specific results do not verify the current working
tree, installed-app behavior, or physical Linux compatibility; retain each entry's
stated limits when using it as evidence.

[Recovery](recovery.md) separates supported recovery from known defects.
[login_item.rs](../../src/login_item.rs), [status_item.rs](../../src/status_item.rs),
[sparkle.rs](../../src/sparkle.rs), the
[identity test](../../scripts/test-app-identity.sh), and
[app validation](../../scripts/validate-app.sh) are maintenance evidence starting
points. Preview badges do not prove installed-app login registration or a signed
update from an older supported build.

## Other Platforms And Consumers

```ts
Linux beta                               // not macOS feature parity
├── hex start -> Per-user service -> Alt-Space capture -> Transcribe -> Paste
├── hex app -> Settings client; closing it leaves the runtime running
├── Recording sounds -> Shortcut press / capture stop / active cancellation
├── Release microphone while idle (opt-in) -> Cold open on shortcut -> Close after capture
│   └── No pre-roll; start tone signals ready; late/cancelled opens are fenced
├── Pause media (opt-in) -> Intentional capture -> Hold through cleanup/paste -> Resume owned MPRIS sessions
├── X11 -> No tray or recording HUD; service status and sounds remain available
├── Wayland -> evdev + compositor protocols; keys observed, not suppressed
│   └── Mouse-classified nodes excluded; live modifiers reused for paste
├── GNOME 49/50 Wayland -> HEX Shell adapter; no evdev access or XWayland fallback
│   └── Owned shortcut press/release + clipboard paste + focused-window context + HUD
├── Escape -> Cancel capture, otherwise newest unfinished output before insertion
│   └── X11 grab conflict -> Try plain Escape; otherwise warn without stopping dictation
├── Microphone failure -> Listener exits, not macOS automatic recovery
│   └── Overload (Xrun) glitch -> Logged; not a failure
└── Paste -> Retain transcript clipboard, not restore previous contents
```

No Linux Commands, Voice Action, retained History, or meetings.
Linux cloud transcription and Modes follow the
[provider guide](../linux.md#cloud-dictation-and-cleanup).
`hex listen` instead prints Moonshine transcripts. The wlroots Wayland path needs
explicit broad input-device access; physical reconnect and click-through still need native
evidence. Its [smoke](../../scripts/test-wayland-paste.sh) explicitly isolates
`HEX_APPLICATION_SUPPORT_DIR` and checks that Settings can exit without stopping
the service.

```ts
Linux Settings > Models & Processing      // dictate.providers.linux
├── Local (default) -> Downloaded model
├── Cloud / Stream -> Compatible audio upload, optional response SSE
└── Realtime -> Intentional capture -> Paced 500 ms audio -> Ordered final turns
    ├── Server VAD (optional, default off) -> Pauses submit turns while capture continues
    │   └── Release -> Drain -> Disable VAD -> Commit open turn + ACK -> Clear + ACK -> All finals
    ├── Live HUD -> Replaceable snapshots / deltas -> Latest 120 characters; never partial paste
    │   └── GNOME: updated extension must load; older extension keeps state-only HUD
    ├── Temporary queue pressure -> Retain unsent capture suffix -> Retry / drain at release
    ├── Live/stream failure -> Full recording -> Same provider/model, normal batch upload
    │   └── Up to N batch attempts; default 2, configurable 0–10; no WS/SSE retry
    └── Failed batch recovery -> No partial paste

Processing Modes                         // modes.providers.linux
├── Application match -> Mode replaces Global
├── Corrections -> Optional OpenCode / Chat / Responses / Gemini cleanup
├── Direct API thinking -> Per-mode provider/model choices; Default omits parameters
│   └── Gemini 2.5 -> Native token budget; Off only where supported
└── Editable instructions + model + deadline -> Ordered final paste
    └── Failed / empty / incomplete cleanup -> Corrected transcript

Mode context (off by default)             // modes.context.linux
├── Clipboard / accessible active field -> Per-recording bounded snapshot
├── Local OCR -> Window -> Tesseract -> Text only to cleanup
└── Screenshot -> Window image to direct vision-capable cleanup
    └── Unavailable source -> Omit; no whole-desktop or image-upload fallback
```

Linux additions leave macOS Modes and local SDK contracts unchanged. Gemini is
cleanup-only. Remote selection skips local-model readiness; choosing a local
model returns to local transcription. Old settings remain local, cleanup/context
off. Credentials are environment/file references, never key values in IPC.

Sources: [provider protocols](../../src/cloud.rs),
[live session](../../src/cloud/realtime.rs),
[pipeline](../../src/linux_pipeline.rs),
[context](../../src/linux_context.rs), and
[editor](../../src/linux_provider_settings.rs).
[Wire tests](../../src/cloud/tests.rs) cover multipart PCM, three native cleanup
formats, SSE framing/finalization, cancellation, live WebSocket audio, and
OCR-mode image exclusion. Live PCM checks compare the emitted prefix against
the final unpadded clip at 16/44.1/48 kHz; realtime checks preserve the unaccepted
suffix under bounded queue pressure and retain commit order when final transcripts
arrive reversed. A delayed-setup WebSocket regression fills the queue with 20 ms
capture deliveries, then verifies the entire ordered PCM ramp and one final result.
Queue pressure no longer cancels a recording: capture retries its retained suffix,
and the output worker drains any remainder at release under the existing deadline.
Queue capacity remains fixed; network waits stay off the capture thread.
The delayed-setup regression failed with `Realtime audio queue overflowed; nothing
pasted` before correction and passed after it. The six realtime checks, full debug
suite (263 passed, 16 opt-in checks skipped), strict all-target Clippy, and formatting
passed on October 18, 2026. These are loopback-fixture checks, not live-gateway or
physical speech-to-paste proof.

Server VAD and live previews are opt-in/provider-dependent additions in
[live session](../../src/cloud/realtime.rs), [HUD](../../src/linux_desktop.rs), and
[GNOME adapter](../../gnome/extension.js). VAD does not end physical capture.
The stop handshake drains real audio and waits for automatic turns plus any open
manual commit; completed turns are never committed again. Clearing uncommitted
leftovers waits for the pending manual commit acknowledgement, and finishing waits
for the clear acknowledgement as well as every final result. Snapshot revisions
replace draft text, and late results cannot replace a newer capture's HUD.
The delayed-setup pacing regression reproduced an 82-message burst for three
seconds of audio before correction. Wire checks cover paced/coalesced full PCM,
VAD stop mid-speech/between turns, and reversed final delivery. GNOME mocked checks
cover preview replacement, lease isolation, byte bounds, hide/lock clearing, and
focus-free labels. These are not native visual proof. The ignored
`realtime_configured_gateway_synthetic_probe` requires an explicit generated PCM
fixture and calls the configured WebSocket directly, without batch recovery;
it does not capture a microphone or exercise paste.
Observed October 18, 2026: direct gateway probes with generated speech succeeded
at 7.2 seconds (four preview updates) and 21.6 seconds (12 updates, eight before
paced audio finished), with server VAD enabled and no batch recovery. The latter
finished in 25.95 seconds including connection/setup and audio delivery. These
prove this gateway's WebSocket path and live event delivery, not native recording,
HUD appearance, or paste. The 121-second manual-turn ordering wire test is opt-in
because recording-speed pacing makes it take two minutes; run explicitly after
transport changes rather than adding that delay to every package build.
October 19 follow-up: the VAD stop fixture reproduced a missing buffer-clear
handshake, then passed after clear/ack was added behind the manual-commit ack fence.
Per-session process logs now distinguish connection/setup, release admission, audio
drain, and final-event wait, preserve initial failure reasons before batch fallback,
and distinguish Escape cancellation from listener shutdown. These are metadata-only
logs; existing Settings finalization timing remains end-to-end.

**Unresolved native latency report:** a 13.94-second real recording took 8.33 seconds
in finalization and 3.95 seconds in cleanup, then pasted successfully without retry.
Generated speech against the currently configured LAN endpoint did not reproduce
that delay: 21.6 seconds with pauses finalized in 558 ms; 14 seconds of continuous
speech had 101 ms of drain and 83 ms of final-event wait. These inputs are not the
user's recording and do not prove its cause or a latency fix. Original WebSocket
causes for older fallback attempts were not retained by the old diagnostic logs.
With the instrumented native service, a subsequent 17.11-second user recording
completed five VAD turns with no retry: 825 ms finalization, 2,533 ms cleanup,
163 ms paste, 3,522 ms total. Connection/setup took 14 ms during capture and the
final-event wait was four milliseconds. This is one successful native capture,
not proof that the earlier intermittent failure or long wait has been eliminated.

Debug verification: 279 tests passed, 18 opt-in checks skipped; the long ordering
check passed separately before becoming opt-in. Eight GNOME mocked tests,
formatting, and strict all-target/all-feature Clippy passed. A final HUD ownership
regression also passed after fencing an older listener's hide operation.

Realtime connection URLs include `intent=transcription` and the selected `model`;
gateway routing must happen before the session update. The live-audio wire check
rejects a missing query model. A separate closed-audio-channel regression checks
that the worker's connection/setup error survives instead of becoming a false
queue-overflow message. Both checks failed before correction. An authenticated
gateway probe, without audio, failed with its default-model routing before the
query change and received `session.updated` with the selected-model query. This
establishes routing/session setup, not live transcription or paste.
Helper checks bound pipes; opt-in Tesseract checks
synthetic pixels only. These are not live provider, physical capture/paste,
desktop Accessibility, or screen-OCR-quality proof. OpenCode accepts OCR text,
not image upload. Wayland needs compatible screenshot support and focused-window
geometry; captured rectangles may include overlapping content.

```ts
Linux Settings > Last dictation          // dictate.retry.linux
├── Accepted recording -> Replace one RAM-only audio/raw/context/result record
│   └── Failed ASR -> Keep audio and original context for another attempt
├── Retry transcription -> Current provider/model, normal batch upload -> Cleanup -> Preview
├── Retry cleanup -> Saved raw text + original context -> Current cleanup -> Preview
├── Textarea -> Full result, selection/scrolling/local edits -> Copy current text
│   └── Cleanup retries still start from saved raw text, not textarea edits
└── Clear / service exit -> Drop retained record; Settings close does not clear it

Latest result + structured logs          // dictate.timings.linux
├── Audio length + automatic retry count
├── Queue -> Transcription/finalization -> Context wait -> Cleanup -> Paste
└── Total since release/submission; failed jobs retain measured stages
```

Sources: [RAM owner](../../src/linux_last_dictation.rs),
[worker](../../src/linux_dictation.rs), [Settings](../../src/linux_app.rs),
[transport](../../src/linux_service.rs), and [events](../../src/events.rs).
The service retains at most 32 MiB of 16 kHz float audio (about eight minutes),
2 MiB each of raw/final text, and the existing bounded original context. Longer
recordings still transcribe but have no audio retry. The shared multiline
`TextInput` shows full retained text with selection, scrolling, and local editing;
Copy reads its edited text. Service refreshes preserve edits until a new result
or explicit retry replaces them. Textarea edits never replace the saved raw input.
Retrying needs a running listener and
uses the same bounded ordered worker queue. Provider-setting edits preserve the
RAM record; the next retry uses new settings. Replacing a record fences old
completions, retries, and Clear actions by ID. Pending original context is shared
with the RAM record before queue admission, so queue rejection or listener
shutdown cannot discard it. Only the output worker resolves that snapshot;
capture/IPC never wait for context. This is not persistent History.
IPC requests stay capped at 128 KiB; replies allow 16 MiB for JSON-escaped text,
with the existing peer-UID checks and I/O deadlines. Audio/context never cross IPC.

A failed realtime/streaming attempt recovers through `/audio/transcriptions`
using the entire recording and `response_format=json`; it never reconnects the
WebSocket or repeats `stream=true`. This includes unsupported streaming contracts
and missing completion events. The same configured provider/model is used. A
batch attempt retries only transient transport failures, HTTP 429/5xx, not permanent
credentials/model/configuration errors. Manual remote transcription retries start
with batch directly; local retries remain local. Zero disables automatic recovery.
Initial attempt has its configured deadline; recovery gets one fresh configured
budget shared across batch attempts/backoff. Cancellation stops both. Cleanup
receives only the complete successful transcript, original capture context, and
current processing settings; no partial output is pasted.

Realtime timing measures finalization (including batch recovery) after worker
admission, not overlapping live ASR. Manual retry uses transcription timing,
not realtime finalization. Context timing measures remaining wait, not capture
work. Skipped stages are absent; cleanup fallback retains latency/error. Manual
retry skips paste and cleanup-only retry skips ASR. Normal paste failure still
exposes generated text.

Checks in [cloud tests](../../src/cloud/tests.rs) disconnect or send an explicit
server transcription failure after partial live audio and require the next request to be a normal multipart POST containing every
original PCM sample. That regression failed with `retry must upload batch, got
GET /v1/realtime` before correction and passed after. Streaming failure checks
then run cleanup and inspect its input for the complete batch transcript plus
retained original clipboard, active-field, OCR, and application context. Other
checks cover direct manual batch retries, retry limits (including zero),
permanent HTTP 401, cancellation, fresh recovery budget after initial timeout,
and shared recovery deadline/backoff. [RAM-owner tests](../../src/linux_last_dictation.rs)
cover lifetime, failed-ASR and unprocessed-job context retention, raw rather than enhanced cleanup
input, stale-ID fencing, and audio cap. Transport/events tests cover large bounded
replies and failed-stage timing serialization. These are loopback/state checks;
native new-button interaction and live-gateway fallback remain unverified.
Observed October 18, 2026: full debug suite passed (278 tests, 16 opt-in checks
skipped), strict all-target/all-feature Clippy and formatting passed.

Thinking presets persist per mode and survive restart; older settings retain
provider defaults. Chat and Responses expose Off through Max; native Gemini uses
levels or 2.5 token budgets, never an invented Off-to-Minimal mapping. Changing
backend resets thinking to Default. OpenCode remains unchanged. Known invalid
Gemini combinations are rejected; aliases/gateways still require model support.
Wire, compatibility, and persistence regressions are in [cloud tests](../../src/cloud/tests.rs)
and [pipeline tests](../../src/linux_pipeline.rs). They cover streaming and ordinary
requests, omitted defaults, native fields, and invalid budgets, not live-provider
acceptance or native interaction with the new controls.

Idle release and media pause are independent persisted Linux Behavior settings,
both off by default. [Microphone ownership](../../src/linux_microphone.rs) bounds
cold opens to one, fences old generations, and closes without waiting for output.
[Capture](../../src/linux_dictation.rs) measures cold physical holds independently
of the new stream's timestamp clock. [Media ownership](../../src/linux_media.rs)
uses bounded session-bus MPRIS calls off capture, tracks unique player owners and
track IDs, skips already-paused players, and resumes after output or shutdown.
Unavailable media support never fails dictation. Only exposed MPRIS sessions are
supported, not arbitrary browser tabs or remote media-server APIs.
Fixture checks cover cold release/cancellation and MPRIS pause/resume/track fences;
the opt-in MPRIS wire test must run inside an isolated `dbus-run-session`, never
on the user's desktop bus. Service smoke covers persistence and restart.

An active Linux capture with no PCM callbacks for five seconds now fails with a
microphone recovery error instead of later discarding a truncated, silent clip.
Digital silence still counts as delivered audio; the watchdog does not require
speech. `stalled_capture_reports_failure_instead_of_silent_timeouts` and
`delivered_audio_resets_stall_deadline` cover this distinction.
The opt-in `native_cold_microphone_preserves_repeated_captures` and
`native_warm_microphone_preserves_repeated_captures` checks open the real default
microphone and discard all samples, exercising capture/resampling without a
provider or paste. They require a working, non-muted input and can interrupt
Bluetooth playback. WirePlumber 0.5.17 has an
[upstream Bluetooth profile-switch regression](https://bugs.debian.org/1148124)
that can stall HFP capture; 0.5.18 contains the profile-restore fixes. Keeping the
microphone warm avoids repeated idle profile switches but does not repair an
already-failed Bluetooth transport.

**Observed October 18, 2026, Q30 stall recovery:** repeated A2DP/HFP switching
still stalled on WirePlumber 0.5.18; upgrading alone did not resolve this machine.
After reselecting CVSD then mSBC, keeping the microphone open preserved three
12-second captures with nonzero PCM across idle intervals. Idle release remains
disabled on this installation. These probes neither called a provider nor pasted
text. Debug and packaged release checks passed 262 tests (16 opt-in checks skipped),
with strict all-target Clippy and formatting passing. Physical speech-to-paste
verification after recovery remains pending.

**Executed October 18, 2026, recording controls:** 260 debug Rust tests passed
with 14 opt-in checks skipped. Strict all-target Clippy and formatting passed.
The isolated MPRIS wire test passed separately, including the production worker's
pause acknowledgement, pending-output ownership, changed-track fence, and shutdown
resume. Parallel runs hit existing OpenCode executable-fixture `ETXTBSY` races;
the full suite passed serially, and Nix package checks now use that policy while
compilation still uses the configured core count. Cold Bluetooth opens wait for
the bounded media-pause attempt so WirePlumber cannot take pause ownership first.

**Observed October 18, 2026, Soundcore Life Q30:** BlueZ advertised SBC/AAC A2DP
endpoints and CVSD/mSBC headset profiles. Opening a local, discarded microphone
probe selected mSBC both before and with temporary `MultiProfile=multiple`;
closing capture restored AAC/A2DP. Original BlueZ config was restored. This is
native profile-transition evidence, not subjective playback-quality verification
or proof of the new toggle's physical shortcut path.

**Executed October 18, 2026, thinking controls:** 257 Rust tests passed in debug
and the Nix release package (13 ignored), strict all-target Clippy and formatting
passed, and all 7 GNOME adapter checks passed. Isolated service IPC checked
legacy defaults, saved thinking, invalid-provider rejection without mutation,
and preservation after restart. No live-provider thinking override was sent.

**Observed October 18, 2026, installed Nix/GNOME Wayland build `d4693e2`:** user
confirmed realtime dictation inserts text. Last four successful outputs recorded
2.866–5.728 seconds in Gemini cleanup, without fallback. This establishes the
configured gateway's dictation/paste path, not every provider or new thinking choice.

**Executed October 18, 2026, Linux working tree:** 249 Rust tests passed,
13 opt-in tests ignored. The synthetic Tesseract fixture passed separately.
Strict all-target Clippy, formatting, and the debug build passed. The isolated
service smoke passed provider validation, private persistence, restart recovery,
and custom-instruction/OCR-setting preservation without a display or microphone.
Paid providers, native Settings interaction, desktop Accessibility, and physical
capture/context/paste remain unverified.

```ts
GNOME 49/50 Wayland                       // dictate.gnome; modes.context.gnome
├── Home Manager gnomeIntegration -> Install + enable HEX Shell adapter -> Log out/in
├── Listener connection -> Exclusive lease -> Compositor shortcut press/release
├── Rust capture + providers -> Modifier-neutral clipboard paste
├── Optional context -> Clipboard / AT-SPI field / Focused-window PNG -> OCR or upload
└── Disconnect / disable -> Release grabs + Hide HUD; no automatic permission grants
```

Sources: [Rust adapter](../../src/linux_gnome.rs),
[Shell adapter](../../gnome/extension.js), and [Home Manager](../../nix/hm-module.nix).
The adapter observes only its registered accelerator and reserves Escape while
recording/processing. It rejects context and paste while locked. Conflicting
desktop shortcuts fail explicitly; Settings uses focused shortcut capture.
No unsafe Shell evaluation, whole-desktop fallback, evdev, wtype, or XWayland.
The click-through Shell HUD replaces the wlroots layer-shell overlay.

**GNOME 50 startup/release correction:** installed 50.4 rejected the removed
`affectsInputRegion` chrome parameter, leaving a partially initialized extension
in ERROR while the service could still acquire its D-Bus endpoint. Startup now
uses the nonreactive label without that option and releases partial D-Bus,
signal, and actor resources on failure. Accelerator grabs explicitly request
`TRIGGER_RELEASE`; otherwise Mutter delivers presses but not releases, leaving
hold-to-dictate captures unfinished. Existing observations showed recording
starts without completion. Native-keypress verification of the corrected build
requires a new GNOME login because extension modules are cached. After login on
GNOME 50.4, the corrected extension was ACTIVE without errors. A shortcut-only
probe received physical press/release; the running Rust listener then observed
`started` followed by `discarded` for a short tap. These checks establish native
shortcut/capture delivery without inference, not provider transcription or paste.

`node --test gnome/extension.test.cjs` reproduced both failures with strict
GNOME-50 chrome options and release-flag-aware accelerator mocks, then passed
all seven checks after correction. These tests do not open a microphone or send
provider requests. Terminal insertion separately requires the persisted
Ctrl-Shift-V preference for terminals that do not accept Ctrl-V.

Executed on the Linux working tree: 251 Rust tests, five compositor-mock adapter
checks, and strict all-target Clippy passed. Mocks cover lease ownership, cleanup,
shortcut conflicts, focus changes, balanced key releases, and locked-desktop
rejection. A synthetic PNG also passed native GJS/GdkPixbuf resize, PNG encoding,
and memory-stream checks without screen access. The dotfiles-selected Nix release
package passed the same 251 Rust and five adapter tests plus the isolated service
smoke. Home Manager module checks and dotfiles flake evaluation passed; no NixOS
activation was performed. These do not establish physical GNOME input, clipboard consumption,
window pixel capture, or native HUD behavior. See the
[GNOME setup](../linux.md#native-gnome-wayland).

Wayland discovery checks every event node for the required broad read access,
then excludes udev mouse-classified nodes even when Logitech-style receivers
advertise synthetic keyboard keys. Stable rejected nodes are cached until their
device identity changes. The active keyboard monitor publishes aggregate
modifier state to paste, avoiding two full device scans after every dictation.
`mouse_nodes_are_not_monitored_even_when_the_receiver_advertises_keyboard_keys`
and `live_modifier_snapshot_tracks_edges_and_device_loss` cover those policies;
the nested-compositor smoke remains the native insertion check.

See the [Linux guide](../linux.md), [linux_app.rs](../../src/linux_app.rs),
[linux_wayland_input.rs](../../src/linux_wayland_input.rs), and
[Linux CI](../../.github/workflows/check-linux.yml) for existing checks.
The [direct installer/updater](../../src/linux_updater.rs) and
[installer tests](../../scripts/test-install-linux-release.sh) do not establish
a published, supported-host-validated signed release. [Nix](../nix.md) owns its
package updates; HEX updates only managed direct installs.

```ts
Home Manager                             // settings.linux-nix
├── autostart / gnomeIntegration -> Nix-managed session integration
├── initialSettings -> Missing linux-settings.json -> Merge defaults -> Validate -> Save privately
│   └── Existing settings -> Ignore seed; preserve app edits across rebuilds
└── Nix package -> No in-app update checks/downloads; update flake + rebuild
```

The seed can configure shortcuts, volume, model/provider selection, cleanup
instructions, per-app modes, and context. No plaintext credentials belong in the
Nix store. Invalid seeds block capture through the existing startup-settings
error path. Sources: [Linux settings](../../src/linux_settings.rs),
[Home Manager module](../../nix/hm-module.nix), and [Nix guide](../nix.md).
Checks: `initial_settings_merge_nested_fields_with_fresh_defaults`,
`invalid_initial_settings_are_rejected`, [module checks](../../nix/checks.nix),
and [service smoke](../../scripts/test-linux-service.py) cover seeding,
private persistence, app edits surviving a missing seed on restart, and
unavailable updates in unmanaged/package layouts. These do not exercise an
activated Home Manager session or physical shortcut capture.

```ts
Linux runtime                            // maintain.linux-service
├── hex start / desktop autostart -> systemd --user hex.service; no Settings or tray
├── Settings / CLI -> Same-user, owner-only Unix socket -> Typed commands and snapshots
├── Close/crash Settings -> Keep microphone, accepted jobs, and model preparation
│   └── Uncommitted shortcut capture -> Cancel and restore the prior listening state
├── Service restart -> Client reconnects; uncertain commands are not replayed
└── hex stop / systemctl --user stop hex -> Stop workers and release devices
```

Sources: the service owner and client in [linux_app.rs](../../src/linux_app.rs),
[linux_service.rs](../../src/linux_service.rs), and the managed installer/Nix
user units. `hex status` queries the running service; `--lines` explicitly reads
historical observations. `hex app --hidden` is a compatibility alias for starting
the service. GUI exit does not mean Stop Listening; the explicit listener and
service controls retain separate meanings. No root daemon or new input grants.

Listener starts consume the service-owned settings snapshot; the foreground
`hex dictate` command loads its own snapshot once. Invalid startup settings leave
defaults available only for editing and block capture until a successful settings
commit or a service restart with valid settings. Dismissing the load error does
not authorize recording with defaults. Feedback is initialized at these entry
points, so a delayed listener start cannot reset a newer live volume selection.

Checks: [test-linux-service.py](../../scripts/test-linux-service.py) exercises
real IPC and process lifetime without a display, installed model, or audio device.
`closing_a_client_does_not_stop_normal_dictation` and
`client_disconnect_cancels_its_edit_and_restores_previous_listening` use controlled
runtime workers. The virtual-microphone and Wayland scripts now run separate
service/client processes. These checks do not establish a physical desktop logout,
cross-version signed update, or microphone/target-app success on the user's devices.

`malformed_settings_cannot_start_even_after_dismissing_the_error` in
[linux_app.rs](../../src/linux_app.rs) checks the failure gate and recovery after a
simulated successful settings commit. It has **not been executed on Linux**.
`listener_startup_ignores_the_previous_session_until_a_new_one_arrives` checks the
listener owner's startup observation and stop signal; the exact production owner
and test passed in an isolated standard-library-only Rust harness on macOS. That
harness does not compile the Linux application or prove its runtime integration;
the test has **not been executed on Linux**. These checks do not exercise physical
devices or persistence failure on disk.

The [September 4 service-build evidence](release-verification.md#linux-service-build)
records an Arch/i3 client-independent ownership check, not transcription accuracy
or physical logout, Wayland, or signed cross-version update proof.

```ts
Linux Settings > Sound volume             // dictate.feedback.linux; X11 + Wayland
├── Default / older settings -> 50%
├── Off -> No recording tones
└── 25% / 50% / 75% / 100% -> Save and apply live; preview the start sound

Capture                                  // hex app and hex dictate
├── Shortcut starts capture -> Queue start sound immediately, before audio arrives
├── Finish retained recording -> Stop sound, before transcription/paste
├── Brief discarded tap -> Start sound, but no stop sound or transcription
└── Cancel active capture -> Cancel sound; idle Escape stays silent
```

The shared [feedback player](../../src/feedback.rs) uses the bundled recording
sounds on both platforms; only macOS wake/sleep/error tones use `afplay`.
Decoding happens before the Linux capture loop; the output device opens for
the first tone and is released five minutes after the last one, as described
in [macOS feedback](dictation.md#feedback). Playback admission is bounded and
nonblocking. Output preparation failures are
logged without blocking dictation. Sound-volume saves in
[linux_app.rs](../../src/linux_app.rs) preserve listener ownership; failed saves
leave the previous selection active, and shortcut/model edits temporarily disable
the control to prevent overlapping settings writes. The start sound does not wait
for the 300 ms capture-retention threshold; input delivery and audio-device
latency still apply.

Checks: `bundled_recording_sounds_decode_without_an_audio_device` and
`feedback_admission_never_waits_for_playback` in the player, plus
`recording_volume_defaults_for_new_and_legacy_settings_and_round_trips` and
`invalid_sound_volume_is_rejected_before_saving` in
[linux_settings.rs](../../src/linux_settings.rs). The desktop host checks
`invalid_volume_preserves_settings_and_does_not_stop_the_listener` and
`volume_changes_cannot_overwrite_an_in_flight_settings_edit` cover invalid-volume
and overlapping-edit guards, not real disk failures.
`start_sound_precedes_audio_and_does_not_change_short_tap_discard` in
[linux_dictation.rs](../../src/linux_dictation.rs) exercises the production start
path with a sound spy: immediate feedback without any audio, while a 100 ms tap
still discards. Existing timing checks in [dictation.rs](../../src/dictation.rs)
cover the unchanged intentional-hold boundary. These are not proof of audible
native playback or physical hotkey-to-sound timing.

The [September 4 sound-build evidence](release-verification.md#linux-recording-sounds-builds)
records the initial and immediate-start builds. It does not establish audible
native playback, measured physical key-to-sound latency, or Wayland UI behavior.

Both Linux CI triggers include `tests/**`, and the
[Nix source fileset](../../nix/package.nix) includes those sources. The
`keyboard_layout` harness skips on Linux; packaging it does not provide macOS
layout evidence.

```ts
TypeScript SDK + macOS local service
├── Caller records audio -> Explicit helper -> Raw transcription
├── Running desktop capture API -> Separate microphone ownership protocol
│   └── No silence gate                  // raw model output, unlike hotkey dictation
└── Distribution
    ├── SDK 0.3.0 -> Published
    └── Turnkey signed helper -> Unfinished
```

The [SDK guide](../../sdk/typescript/README.md),
[service contract](../specs/local-transcription-service.md), and
[SDK tests](../../sdk/typescript/test) distinguish those paths. Helper-only service
mode is not a physical hotkey/microphone test. Embedded/packaged service smokes
now expect API 2, matching service/SDK compatibility checks. Helper packaging
requires an explicit signing team and notarization profile, with the same
Developer ID identity validation as the app; identity fixtures do not establish
a signed helper distribution. Low-level blocking-peek and
FIN/half-close limitations in [local_api.rs](../../src/local_api.rs) must not be
generalized to every SDK cancellation path.

Model progress and dictation levels share bounded SSE framing in
[sse.ts](../../sdk/typescript/src/sse.ts). In-memory
[SDK regressions](../../sdk/typescript/test/sse.test.ts) cover transport-chunk
independence, line/event limits, callback cancellation, reader cleanup, and
bounded level observations. A CR-delimited terminal event is also checked with
an open response body, so completion does not depend on EOF or subsequent data.
These checks do not establish live service or
microphone behavior.

## Developer And Prototype Surfaces

```ts
macOS debug runtime                      // not release navigation
├── Meetings -> Always available; release builds need the Settings opt-in
└── Activity / HUD Lab / hex dev -> Observation and developer controls

Desktop previews -> Rendered fixtures    // not proof capture/download/update happened
iOS prototype -> Record/transcribe/copy and keyboard handoff
Unmerged Windows / deferred features -> Not part of this map
```

Sources: [meeting.rs](../../src/meeting.rs),
[meeting_watcher.rs](../../src/meeting_watcher.rs),
[developer_control.rs](../../src/developer_control.rs),
[capture-preview.sh](../../scripts/capture-preview.sh), [iOS guide](../../ios/README.md).
iOS physical-device proof and temporary-WAV failure cleanup remain gaps;
best-effort deletion is not guaranteed deletion.

`incremental_projection_matches_snapshot_after_updates_and_partial_writes` in
[meeting_live.rs](../../src/meeting_live.rs) compares the incremental transcript
reader with snapshot projection across updates, partial writes, and source
ordering. This checks filesystem/projection behavior, not native meeting capture.

`production_navigation_keeps_commands_available_as_an_opt_in` and
`developer_navigation_appends_developer_panes_in_order` in
[app_window.rs](../../src/app_window.rs) check exact pane availability and order
from the developer-feature flag and the Meetings opt-in; they do not exercise
native navigation clicks.

Desktop previews use a deterministic microphone list. Their settings controls do
not change global shortcut-capture suspension or Dock policy, or play preview
sounds. The
in-memory editor regressions use these fixtures; they do not establish native
device discovery or installed-app behavior.

`default_paste_hotkeys_follow_build_capabilities` in
[app_settings.rs](../../src/app_settings.rs) checks the production default runtime
and settings projections: Paste Last remains available in both build profiles;
meeting paste is present in debug or after the Meetings opt-in. `meeting_paste_is_only_active_in_developer_builds`
in [suppression.rs](../../src/suppression.rs) checks the corresponding constructed
hotkey event, including ordinary-chord discard in release. These replace an
unconditional debug-only expectation and a test-only copy of the reservation rule;
they do not establish native keyboard delivery or meeting insertion.

`structural_event_equality_preserves_serialized_identity` in
[events.rs](../../src/events.rs) covers Activity's full-event equality across
event variants, equal timestamps, non-ASCII text, and optional/error payloads.
It does not exercise native Activity selection or scrolling.

**Executed September 3, 2026, source simplification pass:** 444 Rust tests passed
in both debug and release (nine opt-in tests ignored in each), with all twelve
keyboard-layout child scenarios passing in each profile. Formatting and strict
Clippy passed in both profiles. These checks cover the working-tree cleanup, not
a new published app; no installed-app, microphone, or native Linux test was run.

## September 22, 2026 Cleanup Verification

The working-tree cleanup passed 481 Rust tests in debug and 480 in release, with
twelve opt-in tests ignored in each profile and all twelve keyboard-layout child
scenarios passing per profile. Strict all-target/all-feature Clippy, formatting,
app/helper signing-input guards, and changed shell-script syntax checks passed.
The command SDK passed 52 tests; the public SDK passed 56; both typechecks and
builds passed. A packed public SDK imported both Promise and Effect entry points
from an isolated consumer.

The isolated embedded-service smoke passed API 2 handoff, health/models routes,
absence of discovery-file publication, and shutdown on host-lease closure. It did
not open a microphone or run inference. No signed helper artifact, installed-app
settings flow, provider, physical capture/paste, or Linux integration was tested.
The Linux listener-owner harness and source review do not replace Linux CI.
The optimized Modes preview built and launched, but screenshot capture failed
with `could not create image from window`; native visual layout is unverified.

## Grow The Map

```ts
One feature file
├── Sub-features -> Small action/outcome diagrams with stable IDs
├── Entry points -> Every user route and its prerequisites
├── Existing checks -> Source/test links and what they do not prove
└── Gotchas -> Exceptions, known defects, missing native proof
```

This is the map-only stage of the verification workflow, not a working driver.
Keep checks beside the behavior and tests with the implementation. Split new
files only when useful; link shared failure behavior to Recovery.

Future evidence should name the behavior ID, entry point, revision/binary,
platform, prerequisites, outcome, and artifact location. Report skipped paths.
Never publish tokens, personal transcripts, or unredacted diagnostics as proof.
This map does not authorize operating a live installation.
