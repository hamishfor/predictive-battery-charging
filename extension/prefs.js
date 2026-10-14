// SPDX-License-Identifier: GPL-3.0-or-later
// Settings live in /etc/pbcd/config.toml, which is owned by root, so this
// window only displays them and explains how to edit the file.

import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

import {BUS_NAME, IFACE, OBJECT_PATH, batteryLabel} from './status.js';

function call(method, params, type) {
    const r = Gio.DBus.system.call_sync(
        BUS_NAME, OBJECT_PATH, IFACE, method, params,
        new GLib.VariantType(type), Gio.DBusCallFlags.NONE, 5000, null);
    return JSON.parse(r.deepUnpack()[0]);
}

function fmtDuration(s) {
    const h = Math.floor(s / 3600);
    const m = Math.round((s % 3600) / 60);
    return h > 0 ? `${h} h ${m} min` : `${m} min`;
}

function fmtWhen(ts) {
    return new Date(ts * 1000).toLocaleString([], {weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit'});
}

export default class PredictiveChargingPrefs extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        window.set_default_size(560, 640);
        const page = new Adw.PreferencesPage({title: 'Predictive Battery Charging', icon_name: 'battery-level-80-charging-symbolic'});
        window.add(page);

        let status = null;
        let sessions = [];
        let error = null;
        try {
            status = call('StatusJson', null, '(s)');
            sessions = call('SessionsJson', new GLib.Variant('(x)', [Math.floor(Date.now() / 1000) - 14 * 86400]), '(s)');
        } catch (e) {
            error = Gio.DBusError.strip_remote_error(e) ?? e.message;
        }

        const state = new Adw.PreferencesGroup({title: 'Status'});
        page.add(state);
        if (error) {
            state.add(new Adw.ActionRow({title: 'Daemon not reachable', subtitle: error}));
            state.add(new Adw.ActionRow({title: 'Install', subtitle: 'From the repository: sudo dist/install.sh'}));
            return;
        }
        const row = (title, subtitle) => {
            const r = new Adw.ActionRow({title, subtitle: subtitle ?? '—', subtitle_selectable: true});
            return r;
        };
        state.add(row('Mode', status.mode === 'control' ? 'Control: limits charging, then charges to full before you unplug' : 'Collect: learning only, never limits charging'));
        state.add(row('State', status.reason ?? status.state));
        if (Array.isArray(status.batteries) && status.batteries.length > 1) {
            state.add(row('Battery (combined)', `${status.capacity ?? '?'}%`));
            for (const b of status.batteries)
                state.add(row(batteryLabel(b), `${b.capacity ?? '?'}% · ${b.status} · thresholds ${b.thresholds ?? '?'}${b.energy_full_wh ? ` · ${b.energy_full_wh} Wh` : ''}`));
        } else {
            state.add(row('Battery', `${status.capacity ?? '?'}% · thresholds ${status.thresholds ?? '?'}`));
        }
        if (status.prediction)
            state.add(row('Next full-by', `${fmtWhen(status.prediction.full_by)} (${status.prediction.reason})`));
        if (Number.isInteger(status.secs_to_full))
            state.add(row('Estimated time to full', fmtDuration(status.secs_to_full)));
        for (const a of status.alerts ?? [])
            state.add(row('Alert', a));

        const learned = new Adw.PreferencesGroup({
            title: 'Learned plug-in sessions',
            description: `${status.sessions} recorded in total. The last 14 days are shown. All of it stays on this device.`,
        });
        page.add(learned);
        if (sessions.length === 0)
            learned.add(row('No finished sessions yet', 'A session is recorded when you unplug.'));
        for (const s of sessions.slice(-20).reverse()) {
            const plugged = Math.round((s.plugged.lo + s.plugged.hi) / 2);
            const end = s.end.unplugged ? Math.round((s.end.unplugged.lo + s.end.unplugged.hi) / 2) : s.end.censored;
            const exact = s.end.unplugged && s.end.unplugged.lo === s.end.unplugged.hi && s.plugged.lo === s.plugged.hi;
            learned.add(row(
                `${fmtWhen(plugged)} → ${fmtWhen(end)}`,
                `${fmtDuration(end - plugged)}${exact ? '' : ' (approximate: changed while asleep or off)'}` +
                `${s.capacity_at_plug != null ? ` · ${s.capacity_at_plug}% → ${s.capacity_at_end ?? '?'}%` : ''}`));
        }

        const settings = new Adw.PreferencesGroup({
            title: 'Settings',
            description: 'Settings are in /etc/pbcd/config.toml. After editing, run: sudo systemctl restart pbcd',
        });
        page.add(settings);
        const open = new Gtk.Button({label: 'Show config file', valign: Gtk.Align.CENTER});
        open.connect('clicked', () => {
            Gio.AppInfo.launch_default_for_uri('file:///etc/pbcd/config.toml', null);
        });
        const r = row('Charge limit, schedules, sleep behaviour', 'mode, hold_min, hold_max, [[schedule]], quiet_wake, …');
        r.add_suffix(open);
        settings.add(r);
    }
}
