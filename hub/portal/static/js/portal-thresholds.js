// The interactive four-bound threshold slider behind partials/threshold_slider.html
// — a port of v1's ThresholdSlider.tsx (viewer/frontend/components/config) and
// its lib/sliderRange.ts, minus React, inline label editing and the compact
// variant, plus blank-as-unset, which v1 never had to draw.
//
// One Alpine component, registered once, used by every threshold form on the
// page — the wired form (partials/limits_editor.html) renders it per sensor
// row, so an inline <script> per instance was never an option.
//
// The number inputs are the source of truth and are rendered server-side with
// their real `name` attributes: without Alpine the form still submits, it just
// has no track to drag. Everything here is a second way to type the same four
// numbers.
//
// Geometry, straight from v1:
//
//  - The TRACK covers a fixed range per unit (TRACK below). It never widens.
//    An earlier version of this file widened the axis to fit the bounds and
//    padded past them; dragging a handle to the edge then moved the edge, and
//    a few drags later a bound read -101.4 on a slider that started at -40.
//  - A bound outside the track is PINNED to the nearest edge and marked with
//    a chevron; its label and input keep showing the real value. Inputs accept
//    a wider span than the track draws (INPUT below), exactly as v1 does.
//  - Every pointer interaction is on the track, not the handles. The track
//    finds the nearest handle by its RENDERED position, so two handles pinned
//    to one edge, or a blank bound collapsed onto its neighbour, can still be
//    told apart — by which side of the shared spot the press came from.
//  - Labels above the bar are nudged apart (v1's greedy two-pass) so they
//    never overlap, with a thin connector back to the handle when moved.
//
// Alpine registers on `alpine:init`, which this file catches because
// layout/base.html loads it before the deferred alpine.min.js executes — the
// same ordering partials/alarm_toasts.html relies on.

(function () {
	var KEYS = ['critical_low', 'warning_low', 'warning_high', 'critical_high'];

	// What the track draws, per unit. v1's SLIDER_MIN/MAX (-40…50) for °C and
	// the natural 0…100 for a percentage. A unit nobody listed derives a
	// track from the data it was handed, once, at init.
	var TRACK = { '°C': [-40, 50], '%': [0, 100] };
	// What the inputs and a drag may reach. Wider than the track for °C (a
	// deep-freeze probe legitimately wants -60), the track itself for %, and
	// unbounded for anything else.
	var INPUT = { '°C': [-80, 80], '%': [0, 100] };

	// Label geometry for the nudging pass. Estimated, not measured — v1 does
	// the same, and measuring would need a layout per keystroke.
	var LABEL_FONT_PX = 12;
	var LABEL_GAP_PX = 3;

	// Where the four handles start on a sensor that has NO bounds at all, as
	// a fraction of the track — v1's seededDraft (FieldThresholdAccordion),
	// which spreads them 10/20/80/90 so there is something to grab instead of
	// four empty inputs. v1's wired probes instead carry a hardcoded
	// 15/25/38/40; the spread is the rule that works for every unit without
	// a magic number per field.
	//
	// A temperature or a humidity does not use it (threshold_defaults.md D5):
	// the page hands the configured default band as `opts.seed`
	// (partials/threshold_slider.html), so the numbers a new sensor starts
	// from are the ones in the YAML, not a spread of the track.
	var SEED_FRACTIONS = [0.1, 0.2, 0.8, 0.9];

	document.addEventListener('alpine:init', function () {
		Alpine.data('thresholdSlider', function (opts) {
			opts = opts || {};
			return {
				keys: KEYS,
				b: Object.assign(
					{ critical_low: '', warning_low: '', warning_high: '', critical_high: '' },
					opts.bounds || {}
				),
				unit: opts.unit || '',
				// The configured default band for this unit, or null (see SEED_FRACTIONS).
				seedBand: Array.isArray(opts.seed) ? opts.seed : null,
				step: opts.step || 0.1,
				value: numOrNull(opts.value),
				lo: 0,
				hi: 100,
				inLo: -Infinity,
				inHi: Infinity,
				dragging: null,
				// The track's rendered width, for pixel-based label nudging.
				// Zero while the enclosing <dialog> is closed (display:none);
				// the ResizeObserver in `init` fills it in when it opens.
				barWidth: 0,

				init: function () {
					this.range();
					this.seed();

					var self = this;
					var el = this.$refs.track;
					if (el) {
						if (typeof ResizeObserver !== 'undefined') {
							new ResizeObserver(function (entries) {
								for (var i = 0; i < entries.length; i++) {
									self.barWidth = entries[i].contentRect.width;
								}
							}).observe(el);
						}
						this.barWidth = el.getBoundingClientRect().width;
					}
				},

				// Loads another sensor into the same slider — the limits editor
				// (static/js/portal-limits.js) does this when the selection on
				// /limits changes. `o` = { unit, value, bounds, seed }.
				load: function (o) {
					o = o || {};
					this.unit = o.unit || '';
					this.value = numOrNull(o.value);
					this.seedBand = Array.isArray(o.seed) ? o.seed : null;
					var b = o.bounds || {};
					for (var i = 0; i < KEYS.length; i++) {
						var v = b[KEYS[i]];
						this.b[KEYS[i]] = v === null || v === undefined ? '' : String(v);
					}
					this.range();
				},

				// The track and input spans for the unit (see TRACK / INPUT).
				range: function () {
					this.lo = 0;
					this.hi = 100;
					this.inLo = -Infinity;
					this.inHi = Infinity;
					var track = TRACK[this.unit];
					if (track) {
						this.lo = track[0];
						this.hi = track[1];
					} else {
						var vals = this.knownValues();
						if (vals.length) {
							var min = Math.min.apply(null, vals);
							var max = Math.max.apply(null, vals);
							if (max - min < 1) max = min + 1;
							var pad = (max - min) * 0.1;
							this.lo = min - pad;
							this.hi = max + pad;
						}
					}
					var input = INPUT[this.unit];
					if (input) {
						this.inLo = input[0];
						this.inHi = input[1];
					}
				},

				// Fills the four bounds with a starting band when the sensor
				// has NONE — a threshold dialog that opens on four empty
				// inputs and a flat bar gives the operator nothing to drag
				// and nothing to react to.
				//
				// Only when ALL FOUR are blank, and that restriction is the
				// whole safety of it: a sensor deliberately configured with
				// critical bounds and no warnings must keep its blanks, or
				// reopening its dialog and pressing Save would invent two
				// bounds nobody asked for. An empty threshold has no such
				// intent to preserve.
				//
				// A seed is a suggestion in the inputs, not a stored value —
				// nothing is written until the form is submitted.
				seed: function () {
					for (var i = 0; i < KEYS.length; i++) {
						if (this.num(KEYS[i]) !== null) return;
					}
					// The configured default band, as configured: a bound it leaves
					// out stays blank rather than being invented here.
					if (this.seedBand) {
						for (var k = 0; k < KEYS.length; k++) {
							var d = numOrNull(this.seedBand[k]);
							this.b[KEYS[k]] = d === null ? '' : String(d);
						}
						return;
					}
					var span = this.hi - this.lo;
					if (!(span > 0)) return;
					var step = niceStep(span);
					for (var j = 0; j < KEYS.length; j++) {
						var raw = this.lo + span * SEED_FRACTIONS[j];
						var v = Math.max(this.lo, Math.min(this.hi, round(raw, step)));
						this.b[KEYS[j]] = String(round(v, this.step));
					}
				},

				knownValues: function () {
					var out = [];
					for (var i = 0; i < KEYS.length; i++) {
						var v = numOrNull(this.b[KEYS[i]]);
						if (v !== null) out.push(v);
					}
					if (this.value !== null) out.push(this.value);
					return out;
				},

				num: function (key) {
					return numOrNull(this.b[key]);
				},

				// Where each bound is DRAWN, in value units, always inside the
				// track. A blank bound collapses onto its OUTER neighbour so
				// its own band is zero-wide and the span goes to the band
				// beside it: critical_low set + warning_low blank is red up
				// to the critical bound then green; critical_low blank +
				// warning_low set is amber from the axis start. A bound
				// outside the track is clamped to the edge (v1's clampValue)
				// and `overflow` says which way it went.
				eff: function () {
					var lo = this.lo;
					var hi = this.hi;
					var clampTrack = function (v) {
						return v === null ? null : Math.max(lo, Math.min(hi, v));
					};
					var cl = clampTrack(this.num('critical_low'));
					var wl = clampTrack(this.num('warning_low'));
					var wh = clampTrack(this.num('warning_high'));
					var ch = clampTrack(this.num('critical_high'));

					if (cl === null) cl = lo;
					if (wl === null) wl = cl;
					if (ch === null) ch = hi;
					if (wh === null) wh = ch;

					// Ascending whatever was typed: the gradient's stops must
					// not run backwards. This moves nothing the operator
					// typed, only where it is drawn.
					wl = Math.max(wl, cl);
					wh = Math.max(wh, wl);
					ch = Math.max(ch, wh);
					return { critical_low: cl, warning_low: wl, warning_high: wh, critical_high: ch };
				},

				toPct: function (v) {
					var span = this.hi - this.lo;
					if (!(span > 0)) return 0;
					return Math.max(0, Math.min(100, ((v - this.lo) / span) * 100));
				},

				pct: function (key) {
					return this.toPct(this.eff()[key]);
				},

				// 'below' | 'above' | null — v1's rangeOverflow, on the RAW
				// value. A blank bound is never out of range.
				overflow: function (v) {
					if (v === null || !isFinite(v)) return null;
					if (v < this.lo) return 'below';
					if (v > this.hi) return 'above';
					return null;
				},

				keyOverflow: function (key) {
					return this.overflow(this.num(key));
				},

				valuePct: function () {
					return this.value === null ? 0 : this.toPct(this.value);
				},

				valueOverflow: function () {
					return this.overflow(this.value);
				},

				// v1's zoneGradient: one background instead of five stacked
				// divs. Colours are the theme tokens, so the bar follows the
				// dark theme without a class per band.
				gradient: function () {
					var e = this.eff();
					var p = {
						cl: this.toPct(e.critical_low),
						wl: this.toPct(e.warning_low),
						wh: this.toPct(e.warning_high),
						ch: this.toPct(e.critical_high)
					};
					var red = 'var(--color-critical)';
					var amber = 'var(--color-warning)';
					var green = 'var(--color-success)';
					return (
						'linear-gradient(to right, ' +
						red + ' 0%, ' + red + ' ' + p.cl + '%, ' +
						amber + ' ' + p.cl + '%, ' + amber + ' ' + p.wl + '%, ' +
						green + ' ' + p.wl + '%, ' + green + ' ' + p.wh + '%, ' +
						amber + ' ' + p.wh + '%, ' + amber + ' ' + p.ch + '%, ' +
						red + ' ' + p.ch + '%, ' + red + ' 100%)'
					);
				},

				// -- labels -------------------------------------------------

				labelText: function (key) {
					var v = this.num(key);
					return v === null ? '—' : this.fmt(v);
				},

				// v1's estimateLabelWidth: chars × 0.6em, plus a chevron's
				// width when the value is pinned, plus a little padding.
				labelWidth: function (key) {
					var text = this.labelText(key);
					var chevron = this.keyOverflow(key) ? LABEL_FONT_PX + 2 : 0;
					return text.length * LABEL_FONT_PX * 0.6 + chevron + 4;
				},

				// v1's nudgeLabels: the four label centres in px, spread so
				// none overlap. Forward pass pushes right, clamp the last to
				// the bar, backward pass pulls left, clamp the first to 0.
				nudged: function () {
					var w = this.barWidth;
					var items = [];
					for (var i = 0; i < KEYS.length; i++) {
						var width = this.labelWidth(KEYS[i]);
						items.push({
							key: KEYS[i],
							naturalPx: (this.pct(KEYS[i]) / 100) * w,
							width: width,
							left: (this.pct(KEYS[i]) / 100) * w - width / 2
						});
					}
					for (var f = 1; f < items.length; f++) {
						var prevRight = items[f - 1].left + items[f - 1].width + LABEL_GAP_PX;
						if (items[f].left < prevRight) items[f].left = prevRight;
					}
					var last = items[items.length - 1];
					if (last.left + last.width > w) last.left = w - last.width;
					for (var r = items.length - 2; r >= 0; r--) {
						var nextLeft = items[r + 1].left - LABEL_GAP_PX;
						if (items[r].left + items[r].width > nextLeft) {
							items[r].left = nextLeft - items[r].width;
						}
					}
					if (items[0].left < 0) items[0].left = 0;

					var out = {};
					for (var j = 0; j < items.length; j++) {
						out[items[j].key] = {
							px: items[j].left + items[j].width / 2,
							naturalPx: items[j].naturalPx
						};
					}
					return out;
				},

				labelPx: function (key) {
					return this.nudged()[key].px;
				},

				// How far the label was pushed from its handle; the connector
				// line's horizontal run. Zero (well, under a pixel) means no
				// connector is drawn.
				labelShift: function (key) {
					var n = this.nudged()[key];
					return n.naturalPx - n.px;
				},

				// -- interaction --------------------------------------------

				// What a handle may be dragged to: one step inside its
				// neighbours (v1's ±0.1), or the input range. A blank
				// neighbour does not constrain.
				limits: function (key) {
					var i = KEYS.indexOf(key);
					var left = i > 0 ? this.num(KEYS[i - 1]) : null;
					var right = i < KEYS.length - 1 ? this.num(KEYS[i + 1]) : null;
					return [
						left === null ? this.inLo : left + this.step,
						right === null ? this.inHi : right - this.step
					];
				},

				set: function (key, v) {
					var lim = this.limits(key);
					v = Math.max(lim[0], Math.min(lim[1], v));
					this.b[key] = String(round(v, this.step));
				},

				fromClientX: function (clientX) {
					var rect = this.$refs.track.getBoundingClientRect();
					if (!rect.width) return this.lo;
					var frac = (clientX - rect.left) / rect.width;
					frac = Math.max(0, Math.min(1, frac));
					return this.lo + frac * (this.hi - this.lo);
				},

				// v1's nearestHandle, by RENDERED position (`eff`, clamped to
				// the track). Comparing raw values instead would make a
				// pinned handle ungrabbable: an on-track neighbour is always
				// numerically closer to the edge than a bound past it.
				//
				// Several handles can share a spot — a pinned pair at an
				// edge, a blank bound collapsed onto its neighbour. Then:
				//   - a press clearly to one side takes the outermost handle
				//     on that side (left → lowest, right → highest), which is
				//     the only way to seat a blank bound sitting under a set
				//     one;
				//   - a press ON the spot takes the handle whose raw value is
				//     nearest (v1's tie-break: the innermost of a pinned pair,
				//     which is also the only one free to move), and when none
				//     of them has a value, the outer one for that end of the
				//     track — dragging to the left edge of an empty slider
				//     should give you a critical_low, not a warning_low.
				nearest: function (clientX) {
					var at = this.fromClientX(clientX);
					var e = this.eff();
					var eps = 1e-9;
					var bestD = Infinity;
					var bestV = e[KEYS[0]];
					for (var i = 0; i < KEYS.length; i++) {
						var d = Math.abs(e[KEYS[i]] - at);
						if (d < bestD - eps) {
							bestD = d;
							bestV = e[KEYS[i]];
						}
					}
					var tied = [];
					for (var j = 0; j < KEYS.length; j++) {
						if (Math.abs(e[KEYS[j]] - bestV) <= eps) tied.push(KEYS[j]);
					}
					if (tied.length === 1) return tied[0];
					if (at < bestV - eps) return tied[0];
					if (at > bestV + eps) return tied[tied.length - 1];

					var best = null;
					var bestRaw = Infinity;
					for (var k = 0; k < tied.length; k++) {
						var raw = this.num(tied[k]);
						if (raw !== null && Math.abs(raw - at) < bestRaw) {
							bestRaw = Math.abs(raw - at);
							best = tied[k];
						}
					}
					if (best !== null) return best;
					return bestV <= (this.lo + this.hi) / 2 ? tied[0] : tied[tied.length - 1];
				},

				// Track press: grab the nearest handle, seat it under the
				// pointer, start dragging. The handles themselves take no
				// pointer events (v1's fix for overlapping hit targets).
				start: function (ev) {
					var key = this.nearest(ev.clientX);
					this.dragging = key;
					if (ev.currentTarget && ev.currentTarget.setPointerCapture) {
						try {
							ev.currentTarget.setPointerCapture(ev.pointerId);
						} catch (_) {
							/* a synthetic event in a test has no pointer */
						}
					}
					this.set(key, this.fromClientX(ev.clientX));
				},

				move: function (ev) {
					if (!this.dragging) return;
					ev.preventDefault();
					this.set(this.dragging, this.fromClientX(ev.clientX));
				},

				end: function () {
					this.dragging = null;
				},

				// Arrows nudge the focused handle by one step, shift by ten;
				// Home/End jump to the neighbour limits. A blank bound starts
				// from where it is drawn, so the first press gives it a number.
				key: function (ev, key) {
					var mult = ev.shiftKey ? 10 : 1;
					var cur = this.num(key);
					if (cur === null) cur = this.eff()[key];
					var lim = this.limits(key);
					var next = null;
					if (ev.key === 'ArrowLeft' || ev.key === 'ArrowDown') next = cur - this.step * mult;
					else if (ev.key === 'ArrowRight' || ev.key === 'ArrowUp') next = cur + this.step * mult;
					else if (ev.key === 'Home') next = lim[0];
					else if (ev.key === 'End') next = lim[1];
					if (next === null) return;
					ev.preventDefault();
					this.set(key, next);
				},

				// One decimal below 100, none above — "-40°C" and "80°C" on
				// the track ends, not "-40.0°C", and a pressure axis in the
				// thousands has no business showing tenths.
				fmt: function (v) {
					if (v === null || v === undefined || !isFinite(v)) return '—';
					var n = Math.abs(v) >= 100 ? Math.round(v) : Math.round(v * 10) / 10;
					return String(n) + this.unit;
				}
			};
		});
	});

	function numOrNull(raw) {
		if (raw === null || raw === undefined) return null;
		var s = String(raw).trim();
		if (s === '') return null;
		var n = Number(s);
		return isFinite(n) ? n : null;
	}

	// Rounds to the slider's step and drops float noise (0.30000000000000004).
	function round(v, step) {
		var decimals = (String(step).split('.')[1] || '').length;
		return Number((Math.round(v / step) * step).toFixed(decimals));
	}

	// A round number near a twentieth of the track, so the seeded band reads
	// as a deliberate suggestion rather than an arithmetic leftover: on the
	// -40…50 °C track the raw tenth-points are -31 and 41, which look typed
	// by accident; rounded to 5 they are -30 and 40.
	function niceStep(span) {
		var steps = [0.1, 0.2, 0.5, 1, 2, 5, 10, 20, 25, 50, 100];
		var raw = span / 20;
		for (var i = 0; i < steps.length; i++) {
			if (steps[i] >= raw) return steps[i];
		}
		return steps[steps.length - 1];
	}
})();
