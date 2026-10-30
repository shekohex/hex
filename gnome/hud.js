import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Pango from 'gi://Pango';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

// Stage coordinates are physical pixels; St's font sizes follow the theme scale.
const WIDTH = 420;
const PADDING = 18;
const HEADER = 34;
const FADE_MS = 160;

export class CaptionHud {
    constructor() {
        this.actor = new St.Widget({
            reactive: false, can_focus: false, visible: false, clip_to_allocation: true,
            style: 'background-color: #101112; border: 1px solid #303136; border-radius: 18px; box-shadow: 0 8px 24px 0 rgba(0,0,0,0.28);',
        });
        this._status = new St.Label({
            reactive: false, can_focus: false,
            style: 'color: #b4b7bd; font-size: 11px; font-weight: 600;',
        });
        this._viewport = new St.Widget({reactive: false, can_focus: false, clip_to_allocation: true});
        this._caption = new St.Label({
            reactive: false, can_focus: false,
            style: 'color: #f4f4f5; font-size: 15px; font-weight: 500;',
        });
        this._caption.clutter_text.set_line_wrap(true);
        this._caption.clutter_text.set_line_wrap_mode(Pango.WrapMode.WORD_CHAR);
        this._caption.clutter_text.set_ellipsize(Pango.EllipsizeMode.NONE);
        this._caption.clutter_text.set_use_markup(false);
        this._viewport.add_child(this._caption);
        this.actor.add_child(this._status);
        this.actor.add_child(this._viewport);
        this._text = '';
    }

    _animate(actor, values, duration = FADE_MS) {
        actor.remove_all_transitions();
        if (St.Settings.get().enable_animations)
            actor.ease({...values, duration, mode: Clutter.AnimationMode.EASE_OUT_CUBIC});
        else
            Object.assign(actor, values);
    }

    show(state, text, position) {
        const monitor = Main.layoutManager.primaryMonitor;
        if (!state || position === 2 || !monitor) {
            this.hide();
            return;
        }
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        const width = Math.min(WIDTH * scale, Math.max(1, monitor.width - 32 * scale));
        const contentWidth = Math.max(1, width - 2 * PADDING * scale);
        const appearing = !this.actor.visible;
        const changed = text !== this._text || width !== this.actor.width;
        this.actor.set_width(width);
        this._status.text = state === 1 ? '●  Recording' : '◌  Transcribing';
        this._status.set_position(PADDING * scale, 10 * scale);
        this._status.set_width(contentWidth);
        this._viewport.set_position(PADDING * scale, HEADER * scale);
        this._viewport.set_width(contentWidth);
        let captionHeight = this._viewport.height;
        if (changed || appearing) {
            this._stopFade();
            const previous = this._text;
            this._text = text;
            this._caption.text = text;
            this._caption.set_width(contentWidth);
            const layout = this._caption.clutter_text.get_layout();
            layout.set_width(Math.round(contentWidth * Pango.SCALE));
            const lines = text ? layout.get_lines_readonly() : [];
            const first = lines[Math.max(0, lines.length - 2)];
            const top = first ? layout.index_to_pos(first.get_start_index()).y / Pango.SCALE : 0;
            const [, totalHeight] = layout.get_pixel_size();
            captionHeight = text ? Math.ceil(totalHeight - top) : 0;
            this._caption.set_height(totalHeight);
            // The viewport clips older rows while the newest two roll into place.
            this._animate(this._caption, {translation_y: -top}, appearing ? 0 : 180);
            const olderEnd = lines.length > 1 ? lines[lines.length - 1].get_start_index() : 0;
            const common = Array.from(text);
            const old = Array.from(previous);
            let prefix = 0;
            while (prefix < common.length && prefix < old.length && common[prefix] === old[prefix]) prefix++;
            // The service sends a bounded tail. A rolling window must not
            // restart the fade on words that were already visible.
            for (let overlap = Math.min(old.length, common.length); overlap >= Math.max(8, prefix + 1); overlap--) {
                if (old.slice(-overlap).join('') === common.slice(0, overlap).join('')) {
                    prefix = overlap;
                    break;
                }
            }
            const freshStart = new TextEncoder().encode(common.slice(0, prefix).join('')).length;
            this._fadeWords(freshStart, olderEnd);
        }
        this._viewport.set_height(captionHeight);
        const height = (HEADER + (captionHeight ? 12 : 0)) * scale + captionHeight;
        const x = monitor.x + Math.round((monitor.width - width) / 2);
        const y = position === 1 ? monitor.y + monitor.height - height - 16 * scale
            : monitor.y + (Main.panel.height + 6 * scale);
        if (appearing) {
            this.actor.set_position(x, y);
            this.actor.set_height(height);
            this.actor.opacity = 0;
            this.actor.translation_y = -4 * scale;
            this.actor.show();
            this._animate(this.actor, {opacity: 255, translation_y: 0}, 180);
        } else if (height !== this.actor.height || x !== this.actor.x || y !== this.actor.y) {
            this._animate(this.actor, {height, x, y}, 180);
        }
    }

    _fadeWords(freshStart, olderEnd) {
        const text = this._text;
        // Word boundaries also cover scripts without whitespace. Byte indices are
        // Pango's UTF-8 offsets, not JavaScript's UTF-16 string indices.
        const words = [...new Intl.Segmenter(undefined, {granularity: 'word'}).segment(text)]
            .filter(word => word.isWordLike)
            .map(word => ({start: new TextEncoder().encode(text.slice(0, word.index)).length,
                end: new TextEncoder().encode(text.slice(0, word.index + word.segment.length)).length}))
            .filter(word => word.end > freshStart);
        const started = GLib.get_monotonic_time();
        const animate = St.Settings.get().enable_animations && words.length > 0;
        const paint = () => {
            const elapsed = animate ? (GLib.get_monotonic_time() - started) / 1000 : FADE_MS + 160;
            const attributes = new Pango.AttrList();
            // Clutter's alpha attributes need an explicit Pango foreground;
            // the label's CSS color alone can render these runs as black.
            attributes.insert(Pango.attr_foreground_new(0xf4f4, 0xf4f4, 0xf5f5));
            if (olderEnd) {
                const older = Pango.attr_foreground_alpha_new(40000);
                older.start_index = 0; older.end_index = olderEnd;
                attributes.insert(older);
            }
            for (let index = 0; index < words.length; index++) {
                const delay = Math.min(index * 35, 140);
                const alpha = Math.max(0, Math.min(1, (elapsed - delay) / FADE_MS));
                const maximum = words[index].start < olderEnd ? 40000 : 65535;
                const attribute = Pango.attr_foreground_alpha_new(Math.round(alpha * maximum));
                attribute.start_index = Math.max(words[index].start, freshStart);
                attribute.end_index = words[index].end;
                attributes.insert(attribute);
            }
            this._caption.clutter_text.set_attributes(attributes);
            if (elapsed >= FADE_MS + 140) {
                this._fade = 0;
                return GLib.SOURCE_REMOVE;
            }
            return GLib.SOURCE_CONTINUE;
        };
        paint();
        if (animate)
            this._fade = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 16, paint);
    }

    _stopFade() {
        if (this._fade) GLib.source_remove(this._fade);
        this._fade = 0;
    }

    hide() {
        this._stopFade();
        this.actor.remove_all_transitions();
        this._caption.remove_all_transitions();
        this._caption.text = '';
        this._caption.clutter_text.set_attributes(null);
        this._caption.translation_y = 0;
        this._viewport.set_height(0);
        this._text = '';
        this.actor.hide();
    }

    destroy() {
        this.hide();
        this.actor.destroy();
    }
}
