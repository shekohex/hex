#!/usr/bin/env python3
"""Display-free service/IPC regression. Never uses personal settings or a microphone."""
import json
import os
from pathlib import Path
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())


def wait_for(check, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.025)
    raise AssertionError("timed out waiting for the service")


with tempfile.TemporaryDirectory(prefix="hex-service-", dir=os.environ.get("TMPDIR")) as directory:
    root = Path(directory)
    support = root / "data"
    alsa = root / "alsa.conf"
    alsa.write_text("# No physical audio devices in this test.\n")
    seed = root / "initial-settings.json"
    seed.write_text(json.dumps({"dictation_hotkey": {"key": "f12"},
                                "double_tap_lock": False, "sound_effect_volume": 0.25}))
    env = os.environ | {"HEX_APPLICATION_SUPPORT_DIR": str(support), "ALSA_CONFIG_PATH": str(alsa),
                        "HEX_INITIAL_SETTINGS_FILE": str(seed)}
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    path = support / "service.sock"
    children = []

    def launch():
        child = subprocess.Popen([binary, "service"], env=env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        children.append(child)
        return child

    def connection():
        client = socket.socket(socket.AF_UNIX)
        client.settimeout(4)
        client.connect(str(path))
        return client

    def exact(client, size):
        data = b""
        while len(data) < size:
            chunk = client.recv(size - len(data))
            assert chunk, "service disconnected"
            data += chunk
        return data

    def call(client, request="Snapshot", version=1):
        body = json.dumps({"version": version, "request": request}).encode()
        client.sendall(struct.pack(">I", len(body)) + body)
        size, = struct.unpack(">I", exact(client, 4))
        return json.loads(exact(client, size))

    def snapshot():
        try:
            with connection() as client:
                return call(client)["state"]
        except (OSError, AssertionError):
            return None

    try:
        service = launch()
        state = wait_for(snapshot)
        assert state["pid"] == service.pid
        assert state["desktop"]["listener"]["status"] == "Model required"
        assert state["desktop"]["listener"]["warning"] is None
        assert state["desktop"]["update_status"] == "Unavailable"
        assert state["settings"]["dictation_hotkey"]["key"] == "f12"
        assert state["settings"]["dictation_hotkey"]["alt"] is True
        assert state["settings"]["double_tap_lock"] is False
        assert state["settings"]["paste_with_shift"] is False
        assert state["settings"]["sound_effect_volume"] == 0.25
        assert state["settings"]["processing"]["default_mode"]["thinking"] == "default"
        assert json.loads((support / "linux-settings.json").read_text()) == state["settings"]
        assert stat.S_IMODE((support / "linux-settings.json").stat().st_mode) == 0o600
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        assert stat.S_IMODE(support.stat().st_mode) == 0o700

        duplicate = launch()
        assert duplicate.wait(timeout=5) != 0
        assert snapshot()["pid"] == service.pid

        # A Settings client can leave and reconnect without replacing the service.
        with connection() as client:
            assert call(client)["state"]["pid"] == service.pid
            assert call(client, {"SetVolume": 0.0})["error"] is None
        assert snapshot()["pid"] == service.pid
        assert json.loads((support / "linux-settings.json").read_text())["sound_effect_volume"] == 0

        # Provider configuration persists through the service without local models,
        # network calls, or touching a real device. This listener is already idle.
        providers = {
            "cloud_transcription": {
                "connection": {"base_url": "http://127.0.0.1:9/v1", "api_key_env": ""},
                "model": "fixture-transcriber", "mode": "realtime",
            },
            "processing": {"default_mode": {
                "enabled": True, "backend": "responses", "model": "fixture-cleanup",
                "thinking": "low",
                "prompt": "Keep my custom instructions. Preserve العربية.",
                "context": {"screen": "ocr", "ocr_languages": "eng"},
            }},
        }
        with connection() as client:
            assert call(client, {"ConfigureProviders": providers})["error"] is None
        state = wait_for(lambda: (s := snapshot()) and not s["editing"]
                         and s["settings"]["cloud_transcription"] and s)
        assert state["settings"]["processing"]["default_mode"]["prompt"] == providers["processing"]["default_mode"]["prompt"]
        assert json.loads((support / "linux-settings.json").read_text())["processing"]["default_mode"]["thinking"] == "low"
        assert state["settings"]["sound_effect_volume"] == 0
        assert stat.S_IMODE((support / "linux-settings.json").stat().st_mode) == 0o600
        invalid = providers | {"cloud_transcription": providers["cloud_transcription"] | {"model": ""}}
        with connection() as client:
            assert call(client, {"ConfigureProviders": invalid})["error"]
        assert snapshot()["settings"] == state["settings"]
        invalid_thinking = providers | {"processing": {"default_mode":
            providers["processing"]["default_mode"] | {"backend": "gemini", "thinking": "max"}}}
        with connection() as client:
            assert call(client, {"ConfigureProviders": invalid_thinking})["error"]
        assert snapshot()["settings"] == state["settings"]

        # Disconnecting an uncommitted focused shortcut capture releases only that edit.
        with connection() as client:
            assert call(client, "CaptureShortcut")["state"]["capturing"]
            with connection() as other:
                assert call(other, "CancelShortcut")["error"]
        wait_for(lambda: (s := snapshot()) and not s["editing"])
        assert service.poll() is None

        with connection() as client:
            assert call(client, version=999)["error"]
        with connection() as client:
            client.sendall(struct.pack(">I", 128 * 1024 + 1))
            assert client.recv(1) == b""
        # A stalled request cannot stall other clients or live runtime state.
        with connection() as stalled:
            stalled.sendall(b"\x00")
            assert snapshot()["pid"] == service.pid
            assert stalled.recv(1) == b""

        status = subprocess.run([binary, "status"], env=env, check=True, capture_output=True, text=True)
        status_state = json.loads(status.stdout)
        assert status_state["pid"] == service.pid
        assert status_state["desktop"]["listener"]["warning"] is None

        # Crash leaves a socket, not a permanent instance lock. No stale state is reported.
        service.kill()
        service.wait(timeout=5)
        assert subprocess.run([binary, "status"], env=env, capture_output=True).returncode != 0
        # Nix defaults seed once: even a missing seed cannot override app changes.
        seed.unlink()
        service = launch()
        state = wait_for(snapshot)
        assert state["pid"] == service.pid
        assert state["settings"]["sound_effect_volume"] == 0
        assert state["settings"]["dictation_hotkey"]["key"] == "f12"
        assert state["settings"]["double_tap_lock"] is False
        assert state["settings"]["cloud_transcription"]["mode"] == "realtime"
        assert state["settings"]["processing"]["default_mode"]["prompt"] == providers["processing"]["default_mode"]["prompt"]
        assert state["settings"]["processing"]["default_mode"]["context"]["screen"] == "ocr"
        assert state["settings"]["processing"]["default_mode"]["thinking"] == "low"
        subprocess.run([binary, "stop"], env=env, check=True, capture_output=True)
        assert service.wait(timeout=8) == 0
        assert not path.exists()
        assert (support / "linux-settings.json").exists()
        # Custom data roots must never start/restart the user's real installed unit.
        fakebin = root / "bin"
        fakebin.mkdir()
        marker = root / "unexpected-systemctl"
        fake = fakebin / "systemctl"
        fake.write_text(f'#!/bin/sh\ntouch "{marker}"\nexit 0\n')
        fake.chmod(0o755)
        isolated_env = env | {"DISPLAY": ":invalid", "PATH": f'{fakebin}:{env["PATH"]}'}
        for command in ("start", "restart"):
            assert subprocess.run([binary, command], env=isolated_env, capture_output=True).returncode != 0
        assert not marker.exists()
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)

print("Linux service IPC/lifecycle checks passed.")
