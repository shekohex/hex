import Clutter from 'gi://Clutter';
import GdkPixbuf from 'gi://GdkPixbuf';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS = 'dev.hex.Gnome';
const PATH = '/dev/hex/Gnome';
const INTERFACE = 'dev.hex.Gnome1';
const MODIFIERS = Clutter.ModifierType.SHIFT_MASK | Clutter.ModifierType.CONTROL_MASK |
    Clutter.ModifierType.MOD1_MASK | Clutter.ModifierType.SUPER_MASK;

export default class HexExtension extends Extension {
    enable() {
        try {
            const [, xml] = GLib.file_get_contents(`${this.path}/interface.xml`);
            this._object = Gio.DBusExportedObject.wrapJSObject(new TextDecoder().decode(xml), this);
            this._object.export(Gio.DBus.session, PATH);
            this._bus = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS,
                Gio.BusNameOwnerFlags.NONE, null, null);
            this._nextLease = 0;
            this._signals = [];
            this._extraShortcuts = new Map();
            this._signals.push(global.display.connect('accelerator-activated', (_display, action) => {
                if (action === this._shortcut)
                    this._emit('Shortcut', '(ub)', [this._lease, true]);
                else if (this._extraShortcuts.has(action))
                    this._emit('ShortcutNamed', '(usb)', [this._lease, this._extraShortcuts.get(action), true]);
                else if (action === this._escape)
                    this._emit('Cancel', '(u)', [this._lease]);
            }));
            this._signals.push(global.display.connect('accelerator-deactivated', (_display, action) => {
                if (action === this._shortcut)
                    this._emit('Shortcut', '(ub)', [this._lease, false]);
                else if (this._extraShortcuts.has(action))
                    this._emit('ShortcutNamed', '(usb)', [this._lease, this._extraShortcuts.get(action), false]);
            }));
            this._modeSignal = Main.sessionMode.connect('updated', () => {
                if (Main.sessionMode.isLocked && this._owner) {
                    this._emit('Cancel', '(u)', [this._lease]);
                    this._showState(0);
                }
            });
            this._label = new St.Label({
                reactive: false, can_focus: false, visible: false,
                style: 'background-color: #171717; color: #eeeeee; border: 1px solid #292929; border-radius: 18px; padding: 9px 18px; font-size: 13px;',
            });
            Main.layoutManager.addChrome(this._label);
        } catch (error) {
            this.disable();
            throw error;
        }
    }

    disable() {
        this._reset();
        for (const signal of this._signals ?? [])
            global.display.disconnect(signal);
        this._signals = [];
        if (this._modeSignal)
            Main.sessionMode.disconnect(this._modeSignal);
        this._modeSignal = 0;
        this._label?.destroy();
        this._label = null;
        this._object?.unexport();
        this._object = null;
        if (this._bus)
            Gio.bus_unown_name(this._bus);
        this._bus = 0;
        this._virtualKeyboard = null;
    }

    _emit(name, signature, values) {
        if (this._owner)
            Gio.DBus.session.emit_signal(this._owner, PATH, INTERFACE, name,
                new GLib.Variant(signature, values));
    }

    async _reply(invocation, signature, operation) {
        try {
            const result = await operation();
            invocation.return_value(new GLib.Variant(signature, result));
        } catch (error) {
            invocation.return_dbus_error('dev.hex.Gnome.Error', error.message);
        }
    }

    _check(lease, invocation, allowLocked = false) {
        if (!this._owner || this._owner !== invocation.get_sender() || lease !== this._lease)
            throw new Error('HEX GNOME session expired');
        if (!allowLocked && (Main.sessionMode.isLocked || Main.sessionMode.isGreeter))
            throw new Error('HEX cannot access a locked desktop');
    }

    _grab(accelerator) {
        const action = global.display.grab_accelerator(accelerator,
            Meta.KeyBindingFlags.IGNORE_AUTOREPEAT | Meta.KeyBindingFlags.TRIGGER_RELEASE);
        if (action !== Meta.KeyBindingAction.NONE)
            Main.wm.allowKeybinding(Meta.external_binding_name_for_action(action), Shell.ActionMode.NORMAL);
        return action;
    }

    _ungrab(action) {
        if (action && action !== Meta.KeyBindingAction.NONE) {
            Main.wm.allowKeybinding(Meta.external_binding_name_for_action(action), Shell.ActionMode.NONE);
            global.display.ungrab_accelerator(action);
        }
    }

    _reset() {
        for (const action of this._extraShortcuts?.keys() ?? []) this._ungrab(action);
        this._extraShortcuts?.clear();
        this._ungrab(this._shortcut);
        this._ungrab(this._escape);
        this._shortcut = 0;
        this._escape = 0;
        this._owner = null;
        this._state = 0;
        this._hudPosition = 0;
        this._transcript = '';
        if (this._label) this._label.text = '';
        this._label?.hide();
        if (this._watch)
            Gio.bus_unwatch_name(this._watch);
        this._watch = 0;
    }

    AcquireAsync([accelerator], invocation) {
        this._reply(invocation, '(u)', () => {
            if (this._owner)
                throw new Error('Another HEX listener owns the GNOME session');
            if (!accelerator || accelerator.length > 256 || Main.sessionMode.isLocked || Main.sessionMode.isGreeter)
                throw new Error('Invalid HEX shortcut');
            const shortcut = this._grab(accelerator);
            if (shortcut === Meta.KeyBindingAction.NONE)
                throw new Error('Dictation shortcut already used by GNOME; choose another shortcut in HEX Settings');
            this._shortcut = shortcut;
            this._owner = invocation.get_sender();
            this._lease = ++this._nextLease;
            this._watch = Gio.bus_watch_name_on_connection(Gio.DBus.session, this._owner,
                Gio.BusNameWatcherFlags.NONE, null, () => this._reset());
            return [this._lease];
        });
    }

    BindShortcutsAsync([lease, pasteLast, voiceAction], invocation) {
        this._reply(invocation, '()', () => {
            this._check(lease, invocation);
            const pending = new Map();
            try {
                for (const [name, accelerator] of [['paste-last', pasteLast], ['voice-action', voiceAction]]) {
                    if (!accelerator) continue;
                    if (accelerator.length > 256) throw new Error('Invalid Hex shortcut');
                    const action = this._grab(accelerator);
                    if (action === Meta.KeyBindingAction.NONE) throw new Error(`${name} shortcut already used by GNOME`);
                    pending.set(action, name);
                }
            } catch (error) {
                for (const action of pending.keys()) this._ungrab(action);
                throw error;
            }
            for (const action of this._extraShortcuts.keys()) this._ungrab(action);
            this._extraShortcuts = pending;
        });
    }

    ReleaseAsync([lease], invocation) {
        this._reply(invocation, '()', () => {
            // Releasing remains possible while locked.
            if (this._owner !== invocation.get_sender() || lease !== this._lease)
                throw new Error('HEX GNOME session expired');
            this._reset();
            return [];
        });
    }

    _showState(state) {
        this._state = state;
        if (!state) {
            this._transcript = '';
            this._label.text = '';
            this._label.hide();
            this._ungrab(this._escape);
            this._escape = 0;
            return;
        }
        if (!this._escape)
            this._escape = this._grab('Escape');
        if (this._hudPosition === 2) { this._label.hide(); return; }
        const title = state === 1 ? 'Recording' : 'Transcribing';
        this._label.text = this._transcript ? `${title}\n${this._transcript}` : title;
        const monitor = Main.layoutManager.primaryMonitor;
        if (monitor) {
            this._label.show();
            const [, width] = this._label.get_preferred_width(-1);
            const [, height] = this._label.get_preferred_height(width);
            const y = this._hudPosition === 1 ? monitor.y + monitor.height - height - 16 : monitor.y + 16;
            this._label.set_position(monitor.x + Math.round((monitor.width - width) / 2), y);
        }
    }

    SetHudAsync([lease, position], invocation) {
        this._reply(invocation, '()', () => {
            this._check(lease, invocation, true);
            if (position > 2) throw new Error('Invalid Hex HUD position');
            this._hudPosition = position;
            if (!Main.sessionMode.isLocked && !Main.sessionMode.isGreeter) this._showState(this._state);
        });
    }

    SetStateAsync([lease, state], invocation) {
        this._reply(invocation, '(b)', () => {
            this._check(lease, invocation, true);
            if (state > 2)
                throw new Error('Invalid HEX indicator state');
            if (Main.sessionMode.isLocked || Main.sessionMode.isGreeter) {
                this._showState(0);
                return [true];
            }
            this._showState(state);
            return [!state || !!this._escape];
        });
    }

    SetTranscriptAsync([lease, text], invocation) {
        this._reply(invocation, '()', () => {
            this._check(lease, invocation);
            if (new TextEncoder().encode(text).length > 512)
                throw new Error('HEX live transcript exceeds its limit');
            // Ignore late updates after hide, and use plain text, never markup.
            if (this._state) {
                this._transcript = text;
                this._showState(this._state);
            }
            return [];
        });
    }

    ModifiersHeldAsync([lease], invocation) {
        this._reply(invocation, '(b)', () => {
            this._check(lease, invocation);
            return [!!(global.get_pointer()[2] & MODIFIERS)];
        });
    }

    PasteAsync([lease, text, withShift], invocation) {
        this._reply(invocation, '()', async () => {
            this._check(lease, invocation);
            if (!text.trim() || new TextEncoder().encode(text).length > 2 * 1024 * 1024)
                throw new Error('Invalid HEX paste text');
            const target = global.display.focus_window;
            if (!target || Main.overview.visible || Main.modalCount || (global.get_pointer()[2] & MODIFIERS))
                throw new Error('Foreground application not ready for paste');
            St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
            await new Promise(resolve => GLib.timeout_add(GLib.PRIORITY_DEFAULT, 60, () => {
                resolve();
                return GLib.SOURCE_REMOVE;
            }));
            this._check(lease, invocation);
            if (global.display.focus_window !== target || Main.overview.visible || Main.modalCount || (global.get_pointer()[2] & MODIFIERS))
                throw new Error('Focus or modifiers changed before paste');
            this._virtualKeyboard ??= global.stage.context.get_backend().get_default_seat()
                .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
            const keys = withShift
                ? [Clutter.KEY_Control_L, Clutter.KEY_Shift_L, Clutter.KEY_v]
                : [Clutter.KEY_Control_L, Clutter.KEY_v];
            const pressed = [];
            try {
                for (const key of keys) {
                    pressed.push(key);
                    this._virtualKeyboard.notify_keyval(GLib.get_monotonic_time(), key, Clutter.KeyState.PRESSED);
                }
            } finally {
                for (const key of pressed.reverse()) {
                    try {
                        this._virtualKeyboard.notify_keyval(GLib.get_monotonic_time(), key, Clutter.KeyState.RELEASED);
                    } catch {
                        // Attempt every release even when the device disappeared.
                    }
                }
            }
            return [];
        });
    }

    ClipboardAsync([lease], invocation) {
        this._reply(invocation, '(s)', () => new Promise((resolve, reject) => {
            try {
                this._check(lease, invocation);
                if (this._clipboardBusy)
                    throw new Error('HEX clipboard request already in progress');
                this._clipboardBusy = true;
                St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_clipboard, text) => {
                    this._clipboardBusy = false;
                    try {
                        this._check(lease, invocation);
                        resolve([(text ?? '').slice(0, 8192)]);
                    } catch (error) { reject(error); }
                });
            } catch (error) { reject(error); }
        }));
    }

    FocusedWindowAsync([lease], invocation) {
        this._reply(invocation, '(ss)', () => {
            this._check(lease, invocation);
            const window = global.display.focus_window;
            if (!window || Main.overview.visible || Main.modalCount)
                return ['', ''];
            return [String(window.get_stable_sequence()), window.get_wm_class() ?? ''];
        });
    }

    CaptureWindowAsync([lease, identity, maximumDimension], invocation) {
        this._reply(invocation, '(ay)', async () => {
            this._check(lease, invocation);
            if (this._capturing)
                throw new Error('HEX window capture already in progress');
            const window = global.display.focus_window;
            if (!window || Main.overview.visible || Main.modalCount || String(window.get_stable_sequence()) !== identity)
                throw new Error('Focused window changed');
            const rectangle = window.get_frame_rect();
            const scale = global.display.get_monitor_scale(window.get_monitor());
            if (rectangle.width * rectangle.height * scale * scale > 8388608 || maximumDimension < 64 || maximumDimension > 2800)
                throw new Error('Focused window exceeds capture limit');
            this._capturing = true;
            const stream = Gio.MemoryOutputStream.new_resizable();
            try {
                const screenshot = new Shell.Screenshot();
                await new Promise((resolve, reject) => {
                    screenshot.screenshot_window(false, false, stream, (source, result) => {
                        try { source.screenshot_window_finish(result); resolve(); }
                        catch (error) { reject(error); }
                    });
                });
                this._check(lease, invocation);
                if (global.display.focus_window !== window)
                    throw new Error('Focused window changed');
                stream.close(null);
                const bytes = stream.steal_as_bytes().get_data();
                if (bytes.length > 16 * 1024 * 1024)
                    throw new Error('Focused window image exceeds capture limit');
                const loader = new GdkPixbuf.PixbufLoader();
                loader.write(bytes);
                loader.close();
                let pixels = loader.get_pixbuf();
                const factor = Math.min(1, maximumDimension / Math.max(pixels.width, pixels.height));
                if (factor < 1)
                    pixels = pixels.scale_simple(Math.max(1, Math.round(pixels.width * factor)),
                        Math.max(1, Math.round(pixels.height * factor)), GdkPixbuf.InterpType.BILINEAR);
                const [success, encoded] = pixels.save_to_bufferv('png', [], []);
                if (!success || encoded.length > 16 * 1024 * 1024)
                    throw new Error('Focused window encoding failed');
                return [encoded];
            } finally {
                stream.close(null);
                this._capturing = false;
            }
        });
    }
}
