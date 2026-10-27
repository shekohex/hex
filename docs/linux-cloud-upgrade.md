# Linux cloud upgrade

Upgrade branch: `feat/linux-cloud-latest`, based on upstream `dev` / fork `main`
`00f670a` (October 27, 2026). Original `feat/linux-cloud-pipelines` at `557595a`
remains unchanged as the compatibility/reference branch.

## Port inventory

```ts
Reference branch                         -> Current owner
├── Cloud batch / streamed uploads       -> cloud + current Linux output worker
├── Realtime model routing / pacing      -> cloud::realtime
├── VAD clear/commit handshake           -> cloud::realtime
├── Full-recording batch recovery        -> cloud::transcribe_recording
├── Chat / Responses / Gemini cleanup    -> linux_pipeline
├── Per-mode thinking / corrections      -> linux_pipeline + Rune provider editor
├── Clipboard / field / OCR / images     -> bounded linux_context adapters
├── GNOME HUD / native paste / shortcuts -> GNOME extension + upstream shortcut reducer
├── Release-idle / physical cold hold    -> RecoveringAudioInput + current capture loop
├── Owned MPRIS pause through output     -> original linux_media controller
├── Stalled microphone recovery          -> five-second capture watchdog + recovery owner
├── RAM-only transcription/cleanup retry -> linux_last_dictation + Rune / CLI
├── Phase diagnostics                   -> events + Last dictation
└── Editable Nix first-run seeds         -> Home Manager + shared-settings migration
```

No GPUI, old service/client architecture, or raw evdev backend restored. Upstream
Rune UI, local streaming, Voice Action, History, portal/CLI shortcuts, and current
model/runtime remain. Provider changes apply at idle capture boundaries without
restarting the listener; accepted jobs keep their original settings. Retries use
current providers and original context, never paste automatically.

`linux-settings.json` migrates into private `settings.json`; its original backup
is retained. Existing shared settings win. Seed format remains compatible with
existing Home Manager configuration. No audio is written to disk by this port.

## Observed checks

- Cloud fixtures: 26 passed; the paced 121-second loopback check and configured-gateway probe require explicit runs.
- Strict workspace/all-target Clippy with `capture,onnx-benchmark`: passed.
- Provider editor round trips and explicit Save: passed.
- Legacy cloud/modes/thinking/context and Nix seed migration: passed.
- RAM retry identity/limits, headless cleanup preview, job-local cancellation,
  generation-fenced Unicode previews: passed.
- Isolated display-free service/provider configuration and private persistence:
  passed. No microphone, paid provider, or target-application paste involved.
- Light/dark provider renders inspected; only changed Linux scenes regenerated.
- Nix module, development-shell, and session-readiness checks: passed before
  final package verification.

## Upstream defects retained

Pristine `00f670a` reproduces General's macOS fixture assertion:
`General scrolls: 733 > 691`. The new Update channel row exceeds the existing
no-scroll expectation. Linux package checks skip only this named macOS fixture;
its assertion and product behavior remain unchanged.

Pristine upstream also reproduces `platform_goldens` / `experiment_goldens`
mismatches for `mac-settings-dark`, `mac-settings-sides-dark`, and
`shortcut-explainer-toggle-{dark,light}`. These images remain untouched. Normal
`scripts/check` still reports these baseline failures; they are not green checks.

This environment has `/tmp/.git`, breaking upstream router tests' assumption that
temporary folders have no Git ancestor. Router checks pass with a clean
`/var/tmp` test root. No unrelated temporary Git directory was removed.

Final broad local run: 160 router + 692 app tests passed, with 18 opt-in tests
ignored and the three proven upstream Mac checks explicitly filtered. The full,
unfiltered run reports exactly those same three baseline failures. Synthetic
OCR and private-bus MPRIS checks were subsequently run explicitly and passed.

Additional explicit checks passed: the 121-second paced loopback test preserved
commit order with reversed final responses (121.27 s); local Tesseract OCR read
the synthetic fixture; MPRIS ran only on a private `dbus-run-session` bus. All
10 GNOME compositor mocks passed, including hidden/bottom HUD and extra shortcut
release. The latest debug binary passed the isolated service/provider smoke.
No configured-gateway probe or physical device/paste test was run.

Final cold-open follow-up preserves one in-flight native open across cancelled
shortcuts, discards its stale generation, and stops warm recovery once idle
release applies. The channel-backed regression and full non-baseline suite
passed; strict all-target/all-feature Clippy passed again. No microphone opened.

Output-stage compatibility is verified too: provider corrections/cleanup feed
current Modes transformations; X11/portal retain Escape for pending output; an
explicit retry resets edited preview text while retaining original raw input.
The latest full non-baseline run passed 852 tests (three proven upstream Mac
checks filtered, 18 opt-in tests ignored), plus strict Clippy.
