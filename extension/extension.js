// SPDX-License-Identifier: GPL-3.0-or-later
// Quick Settings tile for pbcd. All charging decisions are made by the daemon.
// This only shows its status and forwards "Charge to full" requests.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import PangoCairo from 'gi://PangoCairo';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import * as Util from 'resource:///org/gnome/shell/misc/util.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {BUS_NAME, IFACE, OBJECT_PATH, chartLayout, present} from './status.js';

// Fallback poll in case a Changed signal is missed.
const REFRESH_S = 120;

const HISTORY_HOURS = 24;
const BARS_H = 48;
const STRIP_H = 4;
const STRIP_GAP = 4;
const LABEL_GAP = 4;
const LABEL_H = 14;
// Between the bolt and the strip.
const GUTTER_PAD = 6;
const BOLT_W = 7;
const BOLT_H = 10;
const CHARGING_RGB = [0x33 / 255, 0xd1 / 255, 0x7a / 255]; // GNOME green 3

// Hourly charge bars, with a strip under them for the hours plugged in, and the hours below.
const ChargeHistory = GObject.registerClass(
class ChargeHistory extends PopupMenu.PopupBaseMenuItem {
    _init() {
        super._init({reactive: false, can_focus: false});
        this._hours = [];
        this._area = new St.DrawingArea({
            height: BARS_H + STRIP_GAP + STRIP_H + LABEL_GAP + LABEL_H,
            x_expand: true,
        });
        this._area.connect('repaint', () => this._repaint());
        this.add_child(this._area);
        this.visible = false;
    }

    setHours(hours) {
        this._hours = hours ?? [];
        this.visible = this._hours.length > 0;
        this._area.queue_repaint();
    }

    _repaint() {
        const cr = this._area.get_context();
        const [w] = this._area.get_surface_size();
        const node = this._area.get_theme_node();
        const fg = node.get_foreground_color();
        const rgb = [fg.red / 255, fg.green / 255, fg.blue / 255];

        const font = node.get_font().copy();
        if (font.get_size_is_absolute())
            font.set_absolute_size(font.get_size() * 0.8);
        else
            font.set_size(Math.round(font.get_size() * 0.8));
        const layout = PangoCairo.create_layout(cr);
        layout.set_font_description(font);
        const measure = s => {
            layout.set_text(s, -1);
            const [, logical] = layout.get_pixel_extents();
            return [logical.width, logical.height];
        };
        const text = (s, x, y) => {
            layout.set_text(s, -1);
            cr.moveTo(x, y);
            PangoCairo.show_layout(cr, layout);
        };

        // Hour labels are centred on their boundary, so the outer two overhang the chart by half.
        const half = measure('00')[0] / 2;
        const gutter = Math.max(BOLT_W + GUTTER_PAD, half);
        const cw = w - gutter - half;
        const {barWidth: bw, bars, segments, ticks} = chartLayout(this._hours, cw, BARS_H);
        const stripY = BARS_H + STRIP_GAP;

        // A lightning bolt for the plug strip.
        cr.setSourceRGBA(...rgb, 0.6);
        const bx = gutter - GUTTER_PAD - BOLT_W, by = stripY + STRIP_H / 2 - BOLT_H / 2;
        const bolt = [[0.65, 0], [0.05, 0.58], [0.45, 0.58], [0.3, 1], [0.95, 0.4], [0.55, 0.4], [0.75, 0]];
        bolt.forEach(([px, py], i) => (i ? cr.lineTo : cr.moveTo).call(cr, bx + px * BOLT_W, by + py * BOLT_H));
        cr.closePath();
        cr.fill();
        const ty = stripY + STRIP_H + LABEL_GAP;
        for (const {x, label} of ticks)
            text(label, gutter + x - measure(label)[0] / 2, ty);

        cr.translate(gutter, 0);
        cr.setSourceRGBA(...rgb, 0.8);
        const r = bw / 2;
        for (const {x, h} of bars) {
            const top = BARS_H - h;
            cr.moveTo(x, BARS_H);
            cr.lineTo(x, top + r);
            cr.arc(x + r, top + r, r, Math.PI, 2 * Math.PI);
            cr.lineTo(x + bw, BARS_H);
            cr.closePath();
        }
        cr.fill();

        const y = BARS_H + STRIP_GAP;
        for (const {x, w: sw, power} of segments) {
            if (power === 'charging')
                cr.setSourceRGBA(...CHARGING_RGB, 1);
            else
                cr.setSourceRGBA(...rgb, 0.35);
            const sr = Math.min(STRIP_H, sw) / 2;
            cr.newSubPath();
            cr.arc(x + sw - sr, y + sr, sr, -Math.PI / 2, 0);
            cr.arc(x + sw - sr, y + STRIP_H - sr, sr, 0, Math.PI / 2);
            cr.arc(x + sr, y + STRIP_H - sr, sr, Math.PI / 2, Math.PI);
            cr.arc(x + sr, y + sr, sr, Math.PI, 3 * Math.PI / 2);
            cr.closePath();
            cr.fill();
        }
        cr.$dispose();
    }
});

const PredictiveChargingToggle = GObject.registerClass(
class PredictiveChargingToggle extends QuickMenuToggle {
    _init(ext) {
        super._init({
            title: 'Battery Charging',
            iconName: 'battery-level-80-charging-symbolic',
            toggleMode: false,
        });
        // GNOME 48's QuickMenuToggle has no menuButtonAccessibleName, and passing it to _init throws.
        if ('menuButtonAccessibleName' in this)
            this.menuButtonAccessibleName = 'Open Predictive Battery Charging menu';
        this._ext = ext;
        this._view = null;

        this.menu.setHeader('battery-level-80-charging-symbolic', 'Predictive Battery Charging');

        this._detailSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._detailSection);
        this.history = new ChargeHistory();
        this.menu.addMenuItem(this.history);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._chargeNow = new PopupMenu.PopupSwitchMenuItem('Charge to full now', false);
        this._chargeNowId = this._chargeNow.connect('toggled', (_item, on) => this._ext.chargeNow(on));
        this.menu.addMenuItem(this._chargeNow);

        // The on/off switch is Settings → Power → Battery Charging, not this tile.
        this._powerSettings = this.menu.addAction('Power Settings', () => {
            Main.overview.hide();
            Main.panel.closeQuickSettings();
            Util.spawn(['gnome-control-center', 'power']);
        });

        // toggleMode is off, so clicking the tile does not change its checked state.
        // Checked only shows that charging is being limited.
        this.connect('clicked', () => this.menu.open());
    }

    update(view) {
        this._view = view;
        this.set({
            visible: view.visible,
            checked: view.checked,
            title: view.title,
            subtitle: view.subtitle,
            iconName: view.icon,
        });
        this.menu.setHeader(view.icon, view.header, view.headline);

        this._detailSection.removeAll();
        for (const line of view.detail) {
            const item = new PopupMenu.PopupMenuItem(line, {reactive: false, can_focus: false});
            item.label.clutter_text.line_wrap = true;
            this._detailSection.addMenuItem(item);
        }

        this._chargeNow.block_signal_handler(this._chargeNowId);
        this._chargeNow.setToggleState(view.chargeNowActive);
        this._chargeNow.unblock_signal_handler(this._chargeNowId);
        this._chargeNow.label.text = view.chargeNowLabel;
        this._chargeNow.setSensitive(view.canChargeNow || view.chargeNowActive);
        this._powerSettings.visible = view.showPowerSettings;

    }
});

const PredictiveChargingIndicator = GObject.registerClass(
class PredictiveChargingIndicator extends SystemIndicator {
    _init(ext) {
        super._init();
        this.toggle = new PredictiveChargingToggle(ext);
        this.quickSettingsItems.push(this.toggle);
    }

    destroy() {
        this.quickSettingsItems.forEach(item => item.destroy());
        super.destroy();
    }
});

export default class PredictiveChargingExtension extends Extension {
    enable() {
        this._indicator = new PredictiveChargingIndicator(this);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
        this._indicator.toggle.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._refreshHistory();
        });

        this._bus = Gio.DBus.system;
        this._cancellable = new Gio.Cancellable();

        // Refresh on Changed and whenever the daemon appears on the bus.
        // Show it as missing when it leaves.
        this._signalId = this._bus.signal_subscribe(
            BUS_NAME, IFACE, 'Changed', OBJECT_PATH, null,
            Gio.DBusSignalFlags.NONE, () => this._refresh());
        this._watchId = Gio.bus_watch_name_on_connection(
            this._bus, BUS_NAME, Gio.BusNameWatcherFlags.NONE,
            () => this._refresh(),
            () => this._show(null));
        this._timerId = GLib.timeout_add_seconds(GLib.PRIORITY_LOW, REFRESH_S, () => {
            this._refresh();
            return GLib.SOURCE_CONTINUE;
        });
        this._refresh();
    }

    disable() {
        this._cancellable?.cancel();
        this._cancellable = null;
        if (this._signalId)
            this._bus.signal_unsubscribe(this._signalId);
        if (this._watchId)
            Gio.bus_unwatch_name(this._watchId);
        if (this._timerId)
            GLib.source_remove(this._timerId);
        this._signalId = this._watchId = this._timerId = 0;
        this._indicator?.destroy();
        this._indicator = null;
        this._bus = null;
    }

    _show(status) {
        this._indicator?.toggle.update(present(status));
        if (!status)
            this._indicator?.toggle.history.setHours(null);
    }

    // Only while the menu is open. Daemons without HistoryJson just hide the chart.
    _refreshHistory() {
        if (!this._bus || !this._indicator?.toggle.menu.isOpen)
            return;
        this._bus.call(
            BUS_NAME, OBJECT_PATH, IFACE, 'HistoryJson', new GLib.Variant('(u)', [HISTORY_HOURS]),
            new GLib.VariantType('(s)'), Gio.DBusCallFlags.NONE, 5000, this._cancellable,
            (bus, res) => {
                try {
                    const [json] = bus.call_finish(res).deepUnpack();
                    this._indicator?.toggle.history.setHours(JSON.parse(json));
                } catch (e) {
                    if (!e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                        this._indicator?.toggle.history.setHours(null);
                }
            });
    }

    _refresh() {
        if (!this._bus)
            return;
        this._bus.call(
            BUS_NAME, OBJECT_PATH, IFACE, 'StatusJson', null,
            new GLib.VariantType('(s)'), Gio.DBusCallFlags.NONE, 5000, this._cancellable,
            (bus, res) => {
                try {
                    const [json] = bus.call_finish(res).deepUnpack();
                    this._show(JSON.parse(json));
                    this._refreshHistory();
                } catch (e) {
                    if (!e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                        this._show(null);
                }
            });
    }

    chargeNow(on) {
        if (!this._bus)
            return;
        this._bus.call(
            BUS_NAME, OBJECT_PATH, IFACE, 'ChargeNow', new GLib.Variant('(b)', [on]),
            null, Gio.DBusCallFlags.ALLOW_INTERACTIVE_AUTHORIZATION, 30000, this._cancellable,
            (bus, res) => {
                try {
                    bus.call_finish(res);
                } catch (e) {
                    if (e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                        return;
                    Main.notify('Predictive Battery Charging', `Could not change charging: ${Gio.DBusError.strip_remote_error(e) ?? e.message}`);
                }
                this._refresh();
            });
    }
}
