// Exercise the shipped adapter with compositor mocks, never the user's desktop.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(`${__dirname}/extension.js`, 'utf8')
    .replace(/^import .*;\n/gm, '')
    .replace('export default class HexExtension', 'globalThis.HexExtension = class HexExtension');

function fixture({enable = true} = {}) {
    const signals = new Map();
    const grabs = new Map();
    const grabFlags = new Map();
    const watches = new Map();
    const emitted = [];
    const keys = [];
    const state = {modifiers: 0, clipboard: 'fixture context', captureCount: 0};
    let nextAction = 1;
    const window = {get_stable_sequence: () => 42, get_wm_class: () => 'Editor',
        get_frame_rect: () => ({width: 100, height: 80}), get_monitor: () => 0};
    const display = {
        focus_window: window,
        connect: (name, callback) => { signals.set(name, callback); return name; },
        disconnect: name => signals.delete(name),
        grab_accelerator: (accelerator, flags) => {
            if (state.conflict) return 0;
            const id = nextAction++;
            grabs.set(id, accelerator);
            grabFlags.set(id, flags);
            return id;
        },
        ungrab_accelerator: id => grabs.delete(id),
        shortcutEdge: (action, pressed) => {
            if (pressed || (grabFlags.get(action) & 128))
                signals.get(pressed ? 'accelerator-activated' : 'accelerator-deactivated')(null, action);
        },
        get_monitor_scale: () => 1,
    };
    const Main = {
        wm: {allowKeybinding() {}}, overview: {visible: false}, modalCount: 0,
        sessionMode: {isLocked: false, isGreeter: false, connect: (_name, callback) => {
            state.modeCallback = callback; return 1;
        }, disconnect() {}},
        layoutManager: {primaryMonitor: {x: 0, y: 0, width: 1000}, addChrome(label, options = {}) {
            state.label = label;
            // GNOME 50 layout.js passes these options through strict Params.parse.
            for (const name of Object.keys(options)) {
                if (!['affectsStruts', 'trackFullscreen'].includes(name))
                    throw new Error(`Unrecognized parameter "${name}"`);
            }
            if (state.chromeError) throw new Error(state.chromeError);
        }},
    };
    const context = {
        TextEncoder, TextDecoder, Extension: class {path = __dirname;}, Main,
        Clutter: {ModifierType: {SHIFT_MASK: 1, CONTROL_MASK: 4, MOD1_MASK: 8, SUPER_MASK: 64},
            InputDeviceType: {KEYBOARD_DEVICE: 1}, KeyState: {PRESSED: 1, RELEASED: 0},
            KEY_Control_L: 1, KEY_Shift_L: 2, KEY_v: 3},
        Meta: {KeyBindingFlags: {IGNORE_AUTOREPEAT: 16, TRIGGER_RELEASE: 128}, KeyBindingAction: {NONE: 0},
            external_binding_name_for_action: id => String(id)},
        Shell: {ActionMode: {NORMAL: 1, NONE: 0}, Screenshot: class {
            screenshot_window(frame, cursor, stream, callback) {
                assert.equal(frame, false); assert.equal(cursor, false);
                state.captureCount++;
                setImmediate(() => { state.beforeCaptureFinish?.(); callback(this, {}); });
            }
            screenshot_window_finish() {}
        }},
        Gio: {
            DBus: {session: {emit_signal: (...args) => emitted.push(args)}},
            DBusExportedObject: {wrapJSObject: () => ({export() {state.exported = true;}, unexport() {state.unexported = true;}})},
            BusNameOwnerFlags: {NONE: 0}, BusNameWatcherFlags: {NONE: 0},
            bus_own_name_on_connection: () => {state.busOwned = true; return 1;},
            bus_unown_name() {state.busOwned = false;},
            bus_watch_name_on_connection: (_connection, name, _flags, _appeared, vanished) => {
                watches.set(name, vanished); return name;
            }, bus_unwatch_name: name => watches.delete(name),
            MemoryOutputStream: {new_resizable: () => ({close() {},
                steal_as_bytes: () => ({get_data: () => new Uint8Array([1, 2, 3])})})},
        },
        GLib: {
            Variant: class {constructor(signature, values) {this.signature = signature; this.values = values;}},
            file_get_contents: () => [true, fs.readFileSync(`${__dirname}/interface.xml`)],
            PRIORITY_DEFAULT: 0, SOURCE_REMOVE: false,
            timeout_add: (_priority, _time, callback) => setImmediate(callback),
            get_monotonic_time: () => 1000000,
        },
        St: {
            Label: class {
                constructor(options) {assert.equal(options.reactive, false); assert.equal(options.can_focus, false); this.visible = false;}
                show() {this.visible = true;} hide() {this.visible = false;} destroy() {this.destroyed = true;}
                get_preferred_width() {return [100, 100];} set_position() {}
            },
            ClipboardType: {CLIPBOARD: 1}, Clipboard: {get_default: () => ({
                set_text: (_selection, text) => {state.clipboard = text;},
                get_text: (_selection, callback) => setImmediate(() => callback(null, state.clipboard)),
            })},
        },
        GdkPixbuf: {InterpType: {BILINEAR: 1}, PixbufLoader: class {
            write() {} close() {} get_pixbuf() {return {width: 100, height: 80,
                save_to_bufferv: () => [true, new Uint8Array([1, 2, 3])]};}
        }},
        global: {display, get_pointer: () => [0, 0, state.modifiers], stage: {context: {
            get_backend: () => ({get_default_seat: () => ({create_virtual_device: () => ({
                notify_keyval: (_time, key, pressed) => {
                    keys.push([key, pressed]);
                    if (state.failKey === key && pressed) throw new Error('fixture keyboard failure');
                },
            })})}),
        }}},
    };
    vm.runInNewContext(source, context);
    const extension = new context.HexExtension();
    if (enable) extension.enable();
    function call(method, params, sender = ':fixture.owner') {
        return new Promise((resolve, reject) => extension[`${method}Async`](params, {
            get_sender: () => sender,
            return_value: value => resolve(value.values),
            return_dbus_error: (_name, message) => reject(new Error(message)),
        }));
    }
    return {extension, call, state, display, Main, keys, signals, grabs, watches, emitted};
}

test('failed GNOME HUD startup releases partial initialization', () => {
    const f = fixture({enable: false});
    f.state.chromeError = 'fixture HUD failure';
    assert.throws(() => f.extension.enable(), /fixture HUD failure/);
    assert.equal(f.signals.size, 0);
    assert.equal(f.state.unexported, true);
    assert.equal(f.state.busOwned, false);
    assert.equal(f.state.label.destroyed, true);
    f.extension.disable();
});

test('GNOME shortcut delivers press and release for hold-to-dictate', async () => {
    const f = fixture();
    const [lease] = await f.call('Acquire', ['<Control><Alt>space']);
    f.display.shortcutEdge(f.extension._shortcut, true);
    f.display.shortcutEdge(f.extension._shortcut, false);
    assert.deepEqual(f.emitted.map(signal => Array.from(signal[4].values)),
        [[lease, true], [lease, false]]);
});

test('GNOME lease isolates methods and cleans up after owner disappears', async () => {
    const f = fixture();
    const [lease] = await f.call('Acquire', ['<Alt>space']);
    await assert.rejects(f.call('FocusedWindow', [lease], ':other'), /expired/);
    await assert.rejects(f.call('Acquire', ['<Alt>space'], ':other'), /Another HEX/);
    await f.call('SetState', [lease, 1]);
    assert.equal(f.state.label.visible, true);
    f.signals.get('accelerator-activated')(null, f.extension._shortcut);
    assert.equal(f.emitted[0][0], ':fixture.owner');
    f.watches.get(':fixture.owner')();
    assert.equal(f.state.label.visible, false);
    assert.equal(f.grabs.size, 0);
    await assert.rejects(f.call('Paste', [lease, 'never paste', false]), /expired/);
});

test('shortcut conflicts fail without taking a lease', async () => {
    const f = fixture(); f.state.conflict = true;
    await assert.rejects(f.call('Acquire', ['<Alt>space']), /already used/);
    assert.equal(f.watches.size, 0);
    assert.equal(f.grabs.size, 0);
});

test('window capture rejects changed focus and never captures desktop', async () => {
    const f = fixture(); const [lease] = await f.call('Acquire', ['<Alt>space']);
    assert.deepEqual(Array.from(await f.call('FocusedWindow', [lease])), ['42', 'Editor']);
    await assert.rejects(f.call('CaptureWindow', [lease, 'old-window', 1024]), /changed/);
    assert.equal(f.state.captureCount, 0);
    await f.call('CaptureWindow', [lease, '42', 1024]);
    assert.equal(f.state.captureCount, 1);
    f.state.beforeCaptureFinish = () => {f.display.focus_window = null;};
    await assert.rejects(f.call('CaptureWindow', [lease, '42', 1024]), /changed/);
    assert.equal(f.extension._capturing, false);
});

test('paste waits for modifiers and releases keys when injection fails', async () => {
    const f = fixture(); const [lease] = await f.call('Acquire', ['<Alt>space']);
    f.state.modifiers = 8;
    await assert.rejects(f.call('Paste', [lease, 'new', false]), /not ready/);
    assert.equal(f.state.clipboard, 'fixture context');
    f.state.modifiers = 0; f.state.failKey = 3;
    await assert.rejects(f.call('Paste', [lease, 'new', true]), /keyboard failure/);
    assert.deepEqual(f.keys.slice(-3), [[3, 0], [2, 0], [1, 0]]);
});

test('locked desktop refuses context and paste; disabling releases resources', async () => {
    const f = fixture(); const [lease] = await f.call('Acquire', ['<Alt>space']);
    f.Main.sessionMode.isLocked = true;
    for (const [method, args] of [['Clipboard', [lease]], ['CaptureWindow', [lease, '42', 1024]], ['Paste', [lease, 'text', false]]])
        await assert.rejects(f.call(method, args), /locked/);
    assert.equal(f.state.captureCount, 0);
    f.extension.disable();
    assert.equal(f.grabs.size, 0);
    assert.equal(f.signals.size, 0);
    assert.equal(f.state.unexported, true);
    assert.equal(f.state.label.destroyed, true);
});
