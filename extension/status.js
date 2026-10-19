// SPDX-License-Identifier: GPL-3.0-or-later
// Turns daemon status into display strings. It has no GNOME Shell imports so
// that plain gjs can run tests/status-test.js against it.

/**
 * Format a unix timestamp as a short local time. It omits the weekday when the time falls today.
 *
 * @param {number} ts unix seconds
 * @param {Date} now
 * @returns {string}
 */
export const BUS_NAME = 'org.pbc.Daemon1';
export const OBJECT_PATH = '/org/pbc/Daemon1';
export const IFACE = 'org.pbc.Daemon1';

const charges = n => `${n} charge${n === 1 ? '' : 's'}`;

export function shortTime(ts, now = new Date()) {
    const d = new Date(ts * 1000);
    const hm = d.toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'});
    const sameDay = d.toDateString() === now.toDateString();
    if (sameDay)
        return hm;
    const tomorrow = new Date(now);
    tomorrow.setDate(now.getDate() + 1);
    if (d.toDateString() === tomorrow.toDateString())
        return `tomorrow ${hm}`;
    return `${d.toLocaleDateString([], {weekday: 'short'})} ${hm}`;
}

/**
 * Turn daemon status into what the tile and menu show.
 *
 * @param {object|null} st parsed StatusJson, or null when the daemon is unreachable
 * @param {Date} now
 * @returns {{visible: boolean, checked: boolean, title: string, subtitle: string, headline: string,
 *            icon: string, header: string, detail: string[], canChargeNow: boolean,
 *            chargeNowActive: boolean, alert: string|null, showPowerSettings: boolean,
 *            chargeNowLabel: string}}
 */
export function present(st, now = new Date()) {
    const base = {
        showPowerSettings: false,
        chargeNowLabel: 'Charge to full now',
        visible: true,
        checked: false,
        // The tile is narrow, so it gets the short name. The menu header has room for the full one.
        title: 'Battery Charging',
        subtitle: '',
        headline: '',
        icon: 'battery-level-80-charging-symbolic',
        header: 'Predictive Battery Charging',
        detail: [],
        canChargeNow: false,
        chargeNowActive: false,
        alert: null,
    };

    if (!st) {
        return {
            ...base,
            subtitle: 'Daemon not running',
            headline: 'Daemon not running',
            icon: 'battery-missing-symbolic',
            detail: ['Check: systemctl status pbcd'],
        };
    }

    const cap = Number.isInteger(st.capacity) ? `${st.capacity}%` : '';
    const withCap = s => (cap ? `${s} · ${cap}` : s);
    const control = st.mode === 'control';
    // A pause is reported in `paused`, or as a reason starting with "yielded:".
    const pausedWhy = st.paused ?? (typeof st.reason === 'string' && st.reason.startsWith('yielded:') ? st.reason : null);
    const paused = !!pausedWhy;
    const gnomeSwitch = st.gnome_switch === true;
    const fullBy = st.prediction?.full_by;
    // The most automatic charging goes to (config `max_pct`), 100 when unset.
    const max = Number.isInteger(st.max_pct) ? st.max_pct : 100;
    // The current charge limit: `max`, a hold below it, or 100 for "charge to full".
    const limit = Number.isInteger(st.charge_to) ? st.charge_to : max;
    const conf = Number.isFinite(st.prediction?.confidence) ? Math.round(st.prediction.confidence * 100) : null;
    // No learned prediction yet. A schedule alone never holds.
    const learning = !st.prediction_source;
    const detail = [];
    // `subtitle` fits the narrow tile. `headline` is the menu header's longer version.
    let subtitle;
    let headline;
    let checked = false;
    let icon = 'battery-level-80-charging-symbolic';

    if (!control) {
        subtitle = 'Learning your routine';
        icon = 'battery-good-symbolic';
        detail.push(`Collect mode · ${charges(st.sessions)} recorded`);
    } else if (paused && gnomeSwitch) {
        subtitle = 'Off';
        headline = withCap('Off · charging to 100%');
        icon = 'battery-good-symbolic';
        detail.push('Turn on: Preserve Battery Health in Power settings');
    } else if (paused) {
        subtitle = 'Paused';
        headline = 'Paused: Preserve Battery Health is on';
        icon = 'battery-caution-symbolic';
        detail.push('Turn it off in Power settings');
    } else {
        switch (st.state) {
        case 'holding':
            checked = true;
            subtitle = `Holding at ${limit}%`;
            headline = fullBy ? `${subtitle} · full by ${shortTime(fullBy, now)}` : subtitle;
            if (conf !== null)
                detail.push(`${st.prediction_source === 'learned' ? 'Your usual unplug' : 'Scheduled'} · ${conf}% sure`);
            break;
        case 'topping_off':
            checked = true;
            subtitle = `Charging to ${max}%`;
            headline = fullBy ? `${subtitle} · full by ${shortTime(fullBy, now)}` : subtitle;
            icon = 'battery-level-90-charging-symbolic';
            break;
        case 'override':
            subtitle = withCap('Charging to 100%');
            icon = 'battery-full-charging-symbolic';
            detail.push('Until you unplug');
            break;
        case 'normal':
            subtitle = withCap(`Charging to ${max}%`);
            if (learning)
                detail.push(`Learning your routine · ${charges(st.sessions)} so far`);
            break;
        case 'unplugged':
        default:
            icon = 'battery-good-symbolic';
            subtitle = withCap('On battery');
            if (st.override_active === true)
                headline = `${subtitle} · next charge to 100%`;
            else if (fullBy)
                detail.push(`Next charge: full by ${shortTime(fullBy, now)}`);
            break;
        }
    }
    headline ??= subtitle;

    // With two batteries the subtitle shows the combined charge, so list each one here.
    if (Array.isArray(st.batteries) && st.batteries.length > 1) {
        const parts = st.batteries.map(b => `${batteryLabel(b)} ${Number.isInteger(b.capacity) ? `${b.capacity}%` : '?'}`);
        detail.push(`Batteries: ${parts.join(' · ')}`);
    }

    const alert = Array.isArray(st.alerts) && st.alerts.length > 0 ? st.alerts.at(-1) : null;
    if (alert)
        detail.push(`⚠ ${alert}`);

    return {
        ...base,
        checked,
        subtitle,
        headline,
        icon,
        detail,
        canChargeNow: control && !paused,
        chargeNowLabel: st.plugged === true ? 'Charge to full now' : 'Charge to full on next plug-in',
        showPowerSettings: gnomeSwitch && control,
        chargeNowActive: st.override_active === true,
        alert,
    };
}

/**
 * A battery's display name: the hardware profile's label if the daemon sent
 * one, otherwise the kernel name (BAT0, BAT1).
 *
 * @param {{name: string, label?: string|null}} b
 * @returns {string}
 */
export function batteryLabel(b) {
    return b.label || b.name;
}
