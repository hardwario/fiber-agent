/*
 * `limitsEditor` — the Alpine half of partials/limits_editor.html, the one
 * limits editor of the portal (plan/portal/limits.cs.md §9.4). The template
 * header holds the context contract; this file holds the behaviour.
 *
 *   targets   [{ id, label, kind, post, unit, value, t, enabled, hysteresis }]
 *             what Save writes to. The FIRST one is what the editor shows.
 *   send      a Node target posts `send=1` too (its draft is sent at once)
 *
 * select(detail)  load new targets: `detail.targets` (the list page fires
 *                 `limits-select` on window with them). An empty list
 *                 disables the editor.
 * cancel()        restore the first target's stored values.
 * save(form)      one POST per target to its own `post` URL with the form's
 *                 fields; the `#command-status` fragment answers, and its
 *                 `data-status` decides ok/error. Results land in `results`.
 *                 With ONE target the fragment also replaces the page's
 *                 `#command-status`, and `limits-done` fires on success.
 *
 * Edit mode (ui.edit_set, ui_components.md §3, 2026-10-15): idle shows
 * "Set limits", start() snapshots the shown values, `changes` counts the
 * inputs that differ from it (Save is disabled at 0), cancel() restores
 * them silently. `opts.always` = the editor opens editing (a device row
 * opened by its Upravit, the /limits selection) and stays so.
 *
 * The slider is the shared `thresholdSlider`, reached through Alpine's
 * `$data` on the element under `x-ref="slider"`; the hysteresis and enabled
 * inputs are plain form fields set here.
 */
(function () {
	'use strict';

	var OK = ['success', 'sent', 'ok', 'drafted', 'pending', 'queued', 'saved', 'delivered', 'confirmed'];

	function str(v) { return v === null || v === undefined ? '' : String(v); }

	// A field the selected targets disagree on has no common value: the editor
	// shows the first target's, and Save must stay possible even when the user
	// leaves it as shown (e.g. switching every limit off while the first one is
	// already off). Such a field is marked in the baseline so it always counts
	// as a change.
	var MIXED = '\u0000mixed';
	function targetField(t, k) {
		if (k === 'enabled') return String(!!t.enabled);
		if (k === 'hysteresis') return str(t.hysteresis);
		return str((t.t || {})[k]);
	}
	function mixed(base, targets) {
		if (!base || !targets || targets.length < 2) return base;
		Object.keys(base).forEach(function (k) {
			var first = targetField(targets[0], k);
			if (targets.some(function (t) { return targetField(t, k) !== first; })) base[k] = MIXED;
		});
		return base;
	}

	document.addEventListener('alpine:init', function () {
		Alpine.data('limitsEditor', function (opts) {
			opts = opts || {};
			return {
				targets: opts.targets || [],
				send: !!opts.send,
				labels: opts.labels || {},
				results: [],
				busy: false,
				always: !!opts.always,
				editing: !!opts.always,
				base: null,
				tick: 0,

				// What the editor shows now, as strings: the four bounds
				// (the slider's reactive `b`), the hysteresis and the switch.
				snap: function () {
					var o = {};
					var s = this.slider();
					['critical_low', 'warning_low', 'warning_high', 'critical_high'].forEach(function (k) {
						o[k] = s && s.b ? str(s.b[k]) : '';
					});
					var h = this.$root.querySelector('input[name="hysteresis"]');
					o.hysteresis = h ? str(h.value) : '';
					var e = this.$refs.enabled;
					o.enabled = e ? String(e.checked) : '';
					return o;
				},
				get changes() {
					this.tick;
					if (!this.editing || !this.base || !this.targets.length) return 0;
					var now = this.snap(), base = this.base;
					return Object.keys(now).filter(function (k) { return now[k] !== base[k]; }).length;
				},
				rebase: function () {
					var self = this;
					this.base = null;
					this.$nextTick(function () { self.base = mixed(self.snap(), self.targets); });
				},
				init: function () { if (this.editing) this.rebase(); },
				start: function () {
					this.results = [];
					if (this.targets.length) this.show(this.targets[0]);
					this.editing = true;
					this.rebase();
				},

				slider: function () {
					var el = this.$refs.slider && this.$refs.slider.querySelector('[x-data]');
					return el ? Alpine.$data(el) : null;
				},

				// Puts a target's stored values into the slider, the hysteresis
				// input and the switch.
				show: function (t) {
					if (!t) return;
					var s = this.slider();
					if (s) s.load({ unit: t.unit, value: t.value, bounds: t.t || {}, seed: t.seed });
					var form = this.$el.closest('form') || this.$el;
					var h = form.querySelector('input[name="hysteresis"]');
					if (h) h.value = str(t.hysteresis);
					var e = this.$refs.enabled;
					if (e) e.checked = !!t.enabled;
				},

				select: function (detail) {
					this.targets = (detail && detail.targets) || [];
					this.results = [];
					this.show(this.targets[0]);
					this.rebase();
				},

				cancel: function () {
					if (this.busy) return;
					this.results = [];
					if (this.targets.length) this.show(this.targets[0]);
					this.editing = this.always;
					this.rebase();
					this.$dispatch('limits-cancel');
				},

				save: function (form) {
					form = form || this.$root;
					if (!this.targets.length || this.busy || !this.changes) return;
					var self = this;
					var base = new FormData(form);
					this.busy = true;
					this.results = [];
					var one = this.targets.length === 1;
					var jobs = this.targets.map(function (t) {
						var body = new URLSearchParams();
						base.forEach(function (v, k) { body.append(k, v); });
						if (self.send && t.kind === 'node') body.set('send', '1');
						return fetch(t.post, {
							method: 'POST',
							credentials: 'same-origin',
							headers: { 'HX-Request': 'true', 'Content-Type': 'application/x-www-form-urlencoded' },
							body: body.toString()
						}).then(function (r) {
							return r.text().then(function (html) {
								var doc = new DOMParser().parseFromString(html, 'text/html');
								var frag = doc.getElementById('command-status');
								var status = frag ? (frag.getAttribute('data-status') || '') : '';
								var ok = r.ok && (status === '' || OK.indexOf(status) !== -1);
								if (one && frag) {
									var cur = document.getElementById('command-status');
									if (cur) {
										cur.replaceWith(frag);
										if (window.htmx) htmx.process(frag);
									}
								}
								var msg = frag ? frag.textContent.replace(/\s+/g, ' ').trim() : '';
								return { id: t.id, label: t.label, ok: ok, message: msg };
							});
						}).catch(function (err) {
							return { id: t.id, label: t.label, ok: false, message: String(err) };
						});
					});
					Promise.all(jobs).then(function (res) {
						var allOk = res.every(function (r) { return r.ok; });
						self.busy = false;
						if (allOk) {
							self.editing = self.always;
							self.rebase();
						}
						// One target answers in the page's own status line.
						self.results = one ? [] : res;
						self.$dispatch('limits-saved', { results: res });
						if (one && allOk) self.$dispatch('limits-done');
					});
				}
			};
		});
	});
})();
