// SPDX-License-Identifier: GPL-3.0-or-later
// Quick Settings tile for pbcd. All charging decisions are made by the daemon.
// This only shows its status and forwards "Charge to full" requests.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import * as Util from 'resource:///org/gnome/shell/misc/util.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {present} from './status.js';

const BUS_NAME = 'org.pbc.Daemon1';
const OBJECT_PATH = '/org/pbc/Daemon1';
const IFACE = 'org.pbc.Daemon1';
// Fallback poll in case a Changed signal is missed.
const REFRESH_S = 120;

const PredictiveChargingToggle = GObject.registerClass(
class PredictiveChargingToggle extends QuickMenuToggle {
    _init(ext) {
        super._init({
            title: 'Battery Charging',
            iconName: 'battery-level-80-charging-symbolic',
            toggleMode: false,
            menuButtonAccessibleName: 'Open Predictive Battery Charging menu',
        });
        this._ext = ext;
        this._view = null;

        this.menu.setHeader('battery-level-80-charging-symbolic', 'Predictive Battery Charging');

        this._detailSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._detailSection);
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
