# Linux Cloud Dictation And Cleanup

Open **General > Cloud providers > Configure…** in Linux Settings. Local transcription remains default.
Cloud selection needs no local model download. GUI and `hex dictate` use saved settings.

```ts
Transcription                            // dictate.providers.linux; independent of cleanup
├── Local -> Existing downloaded model
├── Cloud -> OpenAI-compatible audio/transcriptions upload
├── Stream -> Same upload, streamed transcript response
└── Realtime -> Intentional recording -> Live audio -> Release -> Commit -> Final

Processing Modes                         // modes.providers.linux
├── Matching application -> That mode, replacing Global
└── Otherwise -> Global
    └── Corrections -> Optional cleanup -> One final paste
```

Enter API base URL and exact model ID. Transcription needs the provider's
`/audio/transcriptions` or GA `/realtime?intent=transcription` contract;
Chat Completions compatibility alone does not imply either exists. File-response
streaming is not live audio input. Realtime offers **Server VAD** (off by default;
enable only when the selected provider/model supports it). VAD transcribes turns
at speech pauses while recording; it never ends the shortcut capture or pastes
partial text. At release, HEX drains real audio, disables VAD, waits for its
acknowledgement, commits only an open speech turn, waits for its commit acknowledgement,
clears uncommitted leftovers, then awaits the clear acknowledgement and every final
result. Clear never races ahead of a pending manual commit.
Without VAD, HEX commits bounded 120-second turns. Uploads split into 120-second
requests. **Transcription retries (0–10)** defaults to 2 recovery
attempts. When the first realtime or streaming attempt fails, recovery uploads
the full recording to the same provider's `/audio/transcriptions` endpoint with
`response_format=json`, never another WebSocket or `stream=true` request. This
also handles unsupported streaming contracts. Batch failure retries only transient
transport failures, HTTP 429, or HTTP 5xx; invalid credentials/model errors stop.
Setting 0 disables automatic recovery. No provider/model/local substitution occurs.

The initial attempt has its configured deadline. Batch recovery gets a fresh
configured deadline shared across its attempts and bounded backoff, so a stalled
live request cannot consume recovery time. Escape cancels both phases. Cleanup
and paste run only after a complete transcript; failed recovery pastes no partial
text.
Temporary realtime queue pressure defers unsent audio in the authoritative capture;
subsequent callbacks retry it, and release drains the remaining suffix on the
output worker within the configured deadline. Capture never waits for network I/O,
and the streaming queue stays bounded. Device callbacks are coalesced into 500 ms
PCM packets, paced at recording speed even after slow setup or at release. This
avoids flooding gateways whose queues are bounded by message count. Provider error
codes (not potentially private error messages) remain visible in failures. Per-session
process logs separate connection/setup, release admission, audio drain, and final
result wait; they contain durations/counts, never audio, context, keys, or transcript
text. Batch recovery logs the original failed attempt before replacing its error.
Escape cancellation and listener shutdown have separate diagnostic records.

The click-through GNOME HUD or shared Rune live-text plate shows a UTF-8-safe
512-byte tail of provisional realtime text, including append-only deltas and
replaceable Parakeet snapshots. GNOME uses a fixed 420 logical-pixel notch
(clamped on narrow displays), expanding vertically to at most two caption lines.
New words fade in, older rows scroll upward and dim; reduced motion disables
transitions. The shared Rune plate remains upstream’s single-line implementation.
Compositor mocks cover sizing, scroll/fade requests, Unicode, reduced motion,
and cleanup. Native visual verification is left to the user.
Finals replace drafts; completion order does not change transcript order. Drafts
never enter cleanup or paste. A new capture replaces the preview owner; hiding
clears it. GNOME requires the updated HEX extension to load (log out/in if needed);
older loaded extensions retain working shortcuts and state-only HUDs.

Realtime includes the selected model in both the WebSocket query and session
configuration, so gateways can route before receiving `session.update`. Closed
connections preserve their worker error instead of being reported as queue pressure.
Language hints use the current OpenAI models' `languages` list; legacy and other
compatible models keep the singular `language` contract. See the
[OpenAI transcription contract](https://developers.openai.com/api/docs/guides/transcription).

Cleanup backends: **OpenCode**, **Chat Completions**, **Responses**, native **Gemini**.
Gemini is cleanup-only. OpenCode stays optional and uses existing `opencode2`
provider setup. Its model field accepts `provider/model`, or empty for Automatic;
`VOICE_CONTROL_OPENCODE_CLI` can select an executable. Gemini's base URL defaults
to `https://generativelanguage.googleapis.com/v1beta`.

Each mode has editable instructions, model, deadline, and context choices. New
modes start with the programming-cleanup prompt; saved custom instructions are
preserved. Application matching is exact and case-insensitive: WM_CLASS on X11,
class on Hyprland, app_id/class on Sway, or application names exposed by AT-SPI.
Modes replace Global rather than layering. JSON `replacements` entries support
`matched_phrase` and `output`. Cleanup failure preserves corrected transcription.

### Last Dictation And Timings

Stable behaviors: `dictate.retry.linux`, `dictate.timings.linux`.

**General > Last dictation** retains one recording in service memory, including
failed transcription. New accepted recording replaces it. No audio file or
persistent history is created; Clear or quitting/restarting the service removes
it. Closing Settings or editing provider settings does not remove it.

- **Retry transcription:** batch upload using current speech provider/model, then
  current cleanup (local selection still runs locally). Realtime/Stream settings
  never cause manual retries to open a WebSocket or request streaming.
- **Retry cleanup:** saved raw transcript, current mode/instructions/model; no
  speech-provider call. Both retries reuse original captured context, not Settings.
- Retries run on the existing bounded worker queue, require a running listener,
  and show their result here without pasting. The scrollable, selectable textarea
  shows full retained text. **Copy** uses its current edited text; edits do not
  change the raw transcript used by cleanup retries.
- Audio retention caps at **32 MiB** (about 8 minutes of 16 kHz float PCM).
  Longer recordings still transcribe normally, but only text can be retried.
  Raw/final text each cap at 2 MiB.

Latest result shows audio duration, automatic retry count, queue wait,
transcription, context wait, cleanup, paste, and total time. Structured logs and
NDJSON dictation events contain the same timings, including completed stages on
failure. Skipped stages show “—”. Realtime **Finalize transcript** measures
post-release finalization (including batch recovery), not transcription already
performed while speaking. **Context wait** measures remaining wait, not background
capture duration. **Total** starts at release/job submission and includes queue
wait; manual retry has no paste stage.

### Thinking

Each direct-API processing mode has a **Thinking** control beside its model.
**Default** omits reasoning parameters, preserving provider behavior and older
settings. Changing the cleanup backend resets thinking to Default; Save applies it.

- **Chat / Responses:** Default, Off, Minimal, Low, Medium, High, X-high, Max.
  Off sends `none`; the other presets send their API effort names. Chat uses
  `reasoning_effort`, Responses uses `reasoning.effort`.
- **Gemini:** native `generationConfig.thinkingConfig.thinkingLevel` choices,
  with known model restrictions. Gemini 3/4 do not offer Off. Minimal is not a
  guarantee of no reasoning; newer 3.8/4 models omit Minimal.
- **Gemini 2.5:** Default or Token budget; Flash also offers Off (budget 0).
  Explicit budgets are 1–24,576 for Flash, 512–24,576 for Flash-Lite,
  and 128–32,768 for Pro. No invented low/medium/high-to-token mapping.
- **OpenCode:** unchanged; its own model/variant selection manages reasoning.

Aliases and compatible gateways can support fewer levels than their protocol;
the selected model must accept the requested setting. Invalid known combinations
are rejected on Save. Provider rejection keeps the corrected transcript, following
normal cleanup fallback; HEX never silently substitutes another thinking level.
JSON presets use `thinking: "low"`, `"off"`, `"x_high"`, etc.; a Gemini 2.5 budget
uses `thinking: { "budget": 1024 }`.
See [OpenAI reasoning](https://developers.openai.com/api/docs/guides/reasoning)
and [Gemini thinking](https://ai.google.dev/gemini-api/docs/generate-content/thinking).

### Microphone And Media

Linux **General > Dictation > Advanced** selects microphone lifetime;
**General > Output > While dictating > Pause media** controls MPRIS ownership:

- **Release microphone while idle:** no microphone stream at listener startup.
  Hold the shortcut; opening happens off the capture loop. Wait for the start tone
  before speaking. Cold capture has no pre-roll. Releasing or cancelling before
  audio is ready discards that attempt; late opens cannot enter a newer capture.
  The mic closes after capture, without waiting for transcription/cleanup/paste.
  Off preserves the default warm microphone and pre-roll.
- **Pause media while dictating:** after the 300 ms intentional-hold threshold,
  pause playing MPRIS sessions, including music/video in browsers that expose
  MPRIS. Keep paused through accepted transcription, cleanup, and paste work;
  resume when no intentional recording or output remains. Cancellation, failure,
  and listener shutdown release ownership. Already-paused players are untouched;
  resume only the same process and unchanged track if it is still paused.
  With idle release enabled, HEX owns the pause before opening the headset mic,
  avoiding WirePlumber's independent pause when Bluetooth profiles change.
  Calls are bounded and off the capture loop; unavailable players cannot fail
  dictation. Players without MPRIS and remote-server controls are not supported.

Both options default off; Nix initial settings can seed
`release_microphone_while_idle` and `pause_media_while_recording`.
These do not force system Bluetooth profiles. WirePlumber normally restores A2DP
when the last Bluetooth mic stream closes; another recording app can prevent it.
See [WirePlumber autoswitch](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/settings.html#bluetooth-autoswitch-to-headset-profile).

On the tested Soundcore Life Q30, AAC/SBC A2DP provides playback only, while the
available hands-free codecs are CVSD and mSBC. Native testing with
`MultiProfile=multiple` still switched to mSBC when the mic opened; the temporary
BlueZ override was removed. This option is not a demonstrated duplex-quality fix.

### Credentials

Keys never enter settings or IPC snapshots. Configure an environment-variable
**name**, or an absolute path to a user-owned regular key file with mode `0600`.
Key file takes precedence. Enter secrets using an editor, not argv.
Environment variables must exist in the **service's** environment; terminal exports
do not automatically reach systemd. Use a private `EnvironmentFile` in a
`hex.service` user-unit override, then restart. Leave both fields empty for an
unauthenticated local-compatible server. Requests never follow redirects.

### Context And Local OCR

```ts
Per-recording context                    // modes.context.linux
├── Clipboard / accessible field -> Bounded snapshot
├── OCR -> Focused window -> Local text only
└── Image -> Focused window -> Opted-in vision cleanup
    └── Unavailable source -> Omit; never whole-desktop fallback
```

Context defaults off. Only enabled sources go to cleanup, never the speech model.
Each recording owns its snapshot; context and images are not retained in settings
or diagnostic events.

```ts
Screen context
├── Off -> No window image capture
├── Local OCR -> Focused window pixels -> Tesseract -> Send text only
└── Screenshot -> Focused window pixels -> Send image to vision-capable model
```

OCR works with OpenCode too. Install Tesseract and language data; set **OCR
languages** to installed codes such as `eng` or `eng+ara`. Nix bundles English;
additional data can be supplied through the service's `TESSDATA_PREFIX`.
OCR uses a higher-resolution local image than upload. Pixels use memory/stdin,
not temporary files. OCR runs off the audio thread with bounded I/O and time;
missing/failed optional sources are omitted, never replaced with image upload.

Clipboard context uses the Shell adapter on GNOME, `xclip` on X11, or `wl-paste`
on wlroots Wayland. Focused field
text uses read-only AT-SPI; applications must expose it. Known password fields
are excluded. No synthetic Copy or focus changes. Window capture uses X11 window
pixels, GNOME's focused-window API, or `grim` over a focused-window rectangle on
compatible wlroots Wayland compositors.
Hyprland/Sway use compositor IPC; otherwise AT-SPI must expose active-window
geometry. A Wayland rectangle may include overlapping content. No whole-desktop
fallback. Nix includes clipboard, screenshot, and OCR helpers.

### CLI Configuration

```sh
hex providers show > providers.json
# Edit configuration; credentials remain references, never key values.
hex providers configure providers.json
hex status
```

Configure validates and commits before acknowledging. Provider changes apply to
the listener at its next idle capture boundary; accepted jobs keep their original
settings, while manual retries use current provider settings. No listener restart
or loss of the RAM retry record. The host owns microphone and provider connections;
closing the Rune window never stops them.

Wire tests use loopback HTTP/WebSocket fixtures, not paid APIs or a microphone.
The opt-in `local_ocr_reads_fixture_pixels_without_persisting_an_image` check
uses a synthetic PNG and Tesseract, never the screen. These do not prove live
provider availability, desktop context support, or target-app insertion.

## Native GNOME Wayland

Stable behaviors: `dictate.gnome`, `modes.context.gnome`.

GNOME 49/50 uses the bundled **HEX** Shell extension (`hex@shekohex.dev`). With
Home Manager, enable `programs.hex.gnomeIntegration = true`; log out and back in
after first installation so Shell discovers it. No unsafe Shell mode, XWayland
fallback, root service, or `/dev/input` access is required.

The extension adapts compositor shortcuts, clipboard insertion, focused-window
capture, and the click-through recording/processing indicator. Rust still owns
recording, double-tap locking, cancellation, provider calls, and output order.
The shortcut grab requests release notifications as well as presses; the HUD
uses a nonreactive label without the removed GNOME 50 input-region option.
Failed startup releases partially initialized compositor and D-Bus resources.
Its D-Bus lease belongs to one listener connection; disconnect/disable releases
shortcut grabs and hides the indicator. Locked desktops reject context and paste.
Only the registered shortcut is observed, not arbitrary physical keys.

Choose an unused shortcut in Settings before starting. GNOME often reserves
Alt-Space for its window menu; conflicts fail explicitly rather than changing
desktop bindings. Shortcut capture uses focused Settings input, not evdev.
Paste waits for physical modifiers and keeps the transcript clipboard, like
other Linux backends. No transcript is placed in helper arguments.

OCR and image upload both capture only the focused window, in memory, without
a screen picker on each dictation. Context stays opt-in. Local OCR still runs
through Tesseract; only its text reaches cleanup. No whole-desktop fallback.
Disabling the extension makes the listener fail visibly; Settings remains usable.

Adapter tests use compositor mocks, not the live Shell. Physical shortcut,
context, paste, and HUD behavior still require native verification after login.
After updating extension code, log out/in again: GNOME caches loaded JavaScript
modules, so toggling the extension does not load the corrected source.
API references: [Mutter accelerators](https://gnome.pages.gitlab.gnome.org/mutter/meta/method.Display.grab_accelerator.html),
[focused-window capture](https://gnome.pages.gitlab.gnome.org/gnome-shell/shell/method.Screenshot.screenshot_window.html).


## Upgrade compatibility

The old branch `feat/linux-cloud-pipelines` at `557595a` remains reference-only.
`feat/linux-cloud-latest` started at `00f670a` and now merges fork `main`
`385bb16`.
Shared `settings.json`, Rune UI, local streaming engine, portal/CLI shortcuts,
Voice Action, transformations, and retained History remain upstream-owned.
Provider processing modes replace their own Global mode, without layering.
When provider processing is configured, it owns corrections and cleanup; the
current Modes pane still selects the final TypeScript/built-in transformation
chain. Without provider processing, upstream Modes own all processing stages. Explicitly choosing a prepared local model disables cloud ASR.

`linux-settings.json` migrates once, retaining cloud credentials references,
provider modes/thinking/context, release-microphone, and pause-media preferences.
The backup remains `linux-settings.json.migrated`; a preexisting `settings.json` wins.
Nix seeds retain their original Linux shape and apply only before either settings
file exists. Provider settings changes preserve the RAM-only retry record.

Checks: `cloud::tests` exercises bounded mock HTTP/WebSocket providers, fallback,
cancellation, thinking, OCR payloads, and VAD ordering. `linux_migration::tests`
covers settings conversion and seed defaults. `linux_last_dictation::tests` covers
RAM limits and retry identity. Rune provider-editor tests cover round trips and
invalid drafts. These are not physical microphone, live provider, clipboard target,
or GNOME-session proof. Observed verification belongs in the upgrade report.

## Previous upstream verification defects

The original upgrade baseline `00f670a` reproduces `general_fits_the_default_window_and_hides_advanced_options` failing with `General scrolls: 733 > 691` after the new Update channel row. Later upstream moved the Update channel row into Advanced, then replaced the old no-scroll test during its layout redesign. The fork no longer carries a Nix test exclusion.

Pristine upstream also fails `platform_goldens` and `experiment_goldens`: `mac-settings-dark`, `mac-settings-sides-dark`, and `shortcut-explainer-toggle-{dark,light}`. Their images remain unchanged. Linux/provider goldens intentionally changed with the provider controls; this does not establish native macOS behavior.

Cold-open admission also retains the old single-worker fence: cancelling an
in-flight open releases any current input, retains the pending receiver, and
discards that generation before admitting a newer press's open.
`audio::tests::idle_release_serializes_cold_opens_and_discards_the_cancelled_generation`
uses channel-backed inputs without opening a microphone.

Accepted output keeps Escape registered on X11 and portal backends, not only
GNOME. The input reducer stays idle/idempotent; the dispatcher routes that
explicit cancellation to the newest unfinished output. Explicit retry also
resets local textarea edits without changing the retained raw transcript.

## Latest-main integration (October 29, 2026)

Upstream Wayland paste now waits for shortcut modifiers to settle and chooses
`wtype` only when the compositor advertises virtual keyboards; otherwise it uses
the RemoteDesktop portal. Cloud jobs use that same paster. Portal approval remains
cancellable until actual insertion is committed. GNOME retains its existing
extension-based shortcut/context/paste/HUD adapter, including its physical
modifier check, because the portal does not replace focused-window capture.

Rune's shared GPU HUD replaces the fork's separate recording pill. Cloud drafts
publish through its adjacent live-text plate without requiring the local
`live-text` experiment. Generation fences, 512-byte bounds, and clearing on
idle/shutdown remain; GNOME still receives its original preview API.

Upstream microphone routing/backoff and capture-idle guards are retained. The
fork's cancelled-cold-open serialization and no-audio watchdog remain because
upstream closing still abandons an in-flight open. The fork's owned MPRIS pause
still precedes a Bluetooth cold open and lasts through pending provider output;
upstream's double-tap mute grace is adopted but does not replace track/process
ownership checks. The Speech model Language row shows the cloud language without opening the local
model selector; edit it in Cloud providers. Linux Modes remain visible, including final transformations,
without the new upstream `modes` experiment requirement.

Linux keeps `voice-control` as its existing data directory (including Home
Manager's service environment and CLI discovery), rather than orphaning provider
settings after upstream's rename to `hex`. Legacy settings migration and editable
first-run seeds remain supported. Other operating systems follow upstream paths.
