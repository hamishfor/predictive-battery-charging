// Run: gjs -m extension/tests/status-test.js
import {batteryLabel, present, shortTime} from '../status.js';
import System from 'system';

let failed = 0;
function check(name, cond, got) {
    if (cond) {
        print(`ok   ${name}`);
    } else {
        failed++;
        print(`FAIL ${name}: ${JSON.stringify(got)}`);
    }
}

const now = new Date(2026, 9, 8, 22, 0, 0); // Thu 8 Oct 2026, 22:00 local
const tomorrow8 = Math.floor(new Date(2026, 9, 9, 8, 0, 0).getTime() / 1000);
const today23 = Math.floor(new Date(2026, 9, 8, 23, 0, 0).getTime() / 1000);
const mon8 = Math.floor(new Date(2026, 9, 12, 8, 0, 0).getTime() / 1000);

check('shortTime today', !shortTime(today23, now).includes('tomorrow'), shortTime(today23, now));
check('shortTime tomorrow', shortTime(tomorrow8, now).startsWith('tomorrow'), shortTime(tomorrow8, now));
check('shortTime weekday', /^Mon/.test(shortTime(mon8, now)), shortTime(mon8, now));

const base = {
    plugged: true, capacity: 80, sessions: 3, mode: 'control', state: 'holding',
    prediction: {full_by: tomorrow8, confidence: 0.8, reason: 'learned: 6 similar sessions, schedule: full by Tue 08:00', source: 'schedule'},
    prediction_source: 'schedule',
    reason: 'holding until ...', override_active: false, alerts: [],
};

let v = present(null, now);
check('daemon missing', v.subtitle === 'Daemon not running' && !v.canChargeNow, v);

const words = view => [view.subtitle, ...view.detail].join(' ').split(/\s+/).length;

v = present(base, now);
check('holding checked', v.checked, v);
check('holding subtitle is short', /^Holding at \d+%$/.test(v.subtitle), v.subtitle);
check('holding headline has the time', v.headline.includes('tomorrow'), v.headline);
check('holding can charge now', v.canChargeNow, v);
check('scheduled hold', v.detail.length === 1 && v.detail[0] === 'Scheduled · 80% sure', v.detail);

v = present({...base, state: 'topping_off'}, now);
check('topping off', v.checked && v.subtitle.startsWith('Charging to 100%'), v);

v = present({...base, state: 'override', override_active: true}, now);
check('override', !v.checked && v.chargeNowActive && v.subtitle.startsWith('Charging to 100%'), v);

v = present({...base, state: 'normal', prediction: null}, now);
check('normal', !v.checked && v.subtitle.startsWith('Charging to 100%'), v);

v = present({...base, state: 'normal', prediction: null, prediction_source: null, charge_to: 100, sessions: 13}, now);
check('learning charges to 100', v.subtitle.startsWith('Charging to 100%'), v.subtitle);
check('learning explained', v.detail.some(d => d === 'Learning your routine · 13 charges so far'), v.detail);

v = present({...base, state: 'holding', prediction_source: 'learned', charge_to: 85, prediction: {...base.prediction, confidence: 0.62}}, now);
check('graded hold shows level', v.subtitle === 'Holding at 85%' && v.headline.includes('full by tomorrow'), v);
check('graded hold says usual unplug', v.detail.some(d => d === 'Your usual unplug · 62% sure'), v.detail);

v = present({...base, state: 'topping_off', charge_to: 100}, now);
check('topping off says 100', v.subtitle.startsWith('Charging to 100%'), v.subtitle);

v = present({...base, state: 'topping_off', charge_to: 92, max_pct: 92}, now);
check('topping off to max_pct', v.subtitle.startsWith('Charging to 92%'), v.subtitle);

v = present({...base, state: 'normal', prediction: null, max_pct: 92}, now);
check('normal charges to max_pct', v.subtitle.startsWith('Charging to 92%'), v.subtitle);

v = present({...base, state: 'override', override_active: true, charge_to: 100, max_pct: 92}, now);
check('override still says 100', v.subtitle.startsWith('Charging to 100%'), v.subtitle);

v = present({...base, state: 'unplugged', plugged: false}, now);
check('unplugged', v.subtitle.startsWith('On battery') && !v.subtitle.includes('100%'), v);
check('unplugged can arm charge to full', v.canChargeNow && v.chargeNowLabel === 'Charge to full on next plug-in', v);
check('unplugged shows next deadline', v.detail.some(d => d.startsWith('Next charge: full by tomorrow')), v.detail);

v = present({...base, state: 'unplugged', plugged: false, override_active: true}, now);
check('armed on battery headline', v.headline.includes('next charge to 100%') && v.chargeNowActive, v);

v = present({...base, state: 'holding', plugged: true}, now);
check('plugged label', v.chargeNowLabel === 'Charge to full now', v);
check('plugged headline shows deadline', v.headline.includes('tomorrow'), v.headline);

v = present({...base, mode: 'collect'}, now);
check('collect mode', v.subtitle === 'Learning your routine' && !v.canChargeNow && !v.checked, v);
check('collect counts sessions', v.detail.some(d => d.includes('3 charges')), v.detail);

v = present({...base, mode: 'collect', sessions: 1}, now);
check('singular session', v.detail.some(d => d.includes('1 charge ')), v.detail);

v = present({...base, reason: 'yielded: UPower charge limit is on'}, now);
check('yielded', v.subtitle.startsWith('Paused') && !v.canChargeNow && !v.checked, v);

v = present({...base, gnome_switch: true, paused: 'Off: "Maximize Charge" is selected'}, now);
check('gnome switch off', v.subtitle.startsWith('Off') && !v.checked && !v.canChargeNow, v);
check('gnome switch off explains', v.detail.some(d => d.includes('Preserve Battery Health')), v.detail);
check('gnome switch off shows settings button', v.showPowerSettings, v);

v = present({...base, gnome_switch: true, paused: null}, now);
check('gnome switch on holding', v.checked && v.subtitle.startsWith('Holding at'), v);
check('gnome switch on adds no hint', !v.detail.some(d => d.includes('Maximize Charge')), v.detail);
check('gnome switch on shows settings button', v.showPowerSettings, v);

v = present({...base, gnome_switch: false, paused: 'UPower charge limit is on'}, now);
check('legacy pause wording', v.subtitle.startsWith('Paused') && !v.showPowerSettings, v);

v = present({...base, gnome_switch: true, mode: 'collect'}, now);
check('collect hides settings button', !v.showPowerSettings, v);

v = present({...base, alerts: ['NotCharging { since: 1, capacity: 79 }']}, now);
check('alert shown', v.alert !== null && v.detail.some(d => d.startsWith('⚠')), v);

v = present({...base, capacity: null}, now);
check('missing capacity', !v.subtitle.includes('null') && !v.subtitle.includes('undefined'), v.subtitle);

const two = [{name: 'BAT0', label: 'Internal', capacity: 64}, {name: 'BAT1', label: 'Rear', capacity: 20}];
v = present({...base, capacity: 52, batteries: two}, now);
check('two batteries listed with profile labels', v.detail.some(d => d === 'Batteries: Internal 64% · Rear 20%'), v.detail);
v = present({...base, capacity: 52, batteries: [{name: 'BAT0', capacity: 64}, {name: 'BAT1', capacity: 20}]}, now);
check('two batteries without labels use kernel names', v.detail.some(d => d === 'Batteries: BAT0 64% · BAT1 20%'), v.detail);
v = present({...base, state: 'unplugged', plugged: false, capacity: 52, batteries: [{name: 'BAT0', capacity: 64}, {name: 'BAT1', capacity: 20}]}, now);
check('headline uses combined charge', v.subtitle.includes('52%'), v.subtitle);
v = present({...base, batteries: [{name: 'BAT0', capacity: 80}]}, now);
check('one battery not listed', !v.detail.some(d => d.startsWith('Batteries:')), v.detail);
check('label fallback', batteryLabel({name: 'BAT2', label: null}) === 'BAT2', batteryLabel({name: 'BAT2'}));

for (const st of [base, {...base, state: 'normal', prediction_source: null}, {...base, state: 'unplugged', plugged: false}, {...base, mode: 'collect'}])
    check(`short: ${st.state}/${st.mode}`, words(present(st, now)) <= 14, present(st, now));

print(failed ? `\n${failed} failed` : '\nall passed');
System.exit(failed ? 1 : 0);
