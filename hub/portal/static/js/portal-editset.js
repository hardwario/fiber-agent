/*
 * `editSet` — the Alpine half of a checkbox edit set (ui.edit_set,
 * plan/portal/ui_components.md §3): the Channels card today.
 *
 *   rows     ids of the switchable rows
 *   on       { id: bool } their stored state
 *   actions  { id: url } POST target, `enabled=true|false`
 *   labels   { id: name } for the confirm dialog
 *   bands    { id: bool } switching it off drafts a Node alarm table
 *   limits   { id: { post, kind, unit, value, seed, toggle, configured,
 *              d: { t: {critical_low, warning_low, warning_high,
 *                       critical_high}, enabled, hysteresis } } }
 *            the rows' limits (ui_components.md §2.8): `d` is the stored
 *            band. One inline editor (`x-ref="limrow"`, the shared
 *            threshold slider) opens under one row at a time; its check
 *            accepts into `ldraft` (counted in `changes`, not saved), its X
 *            reverts. Save posts the changed bands to their own routes,
 *            exactly the fields the limits editor posts.
 *
 * start() copies the stored state into `draft`; cancel() drops it silently;
 * save() opens the confirm dialog (`x-ref="dlg"`) when something goes off,
 * else commit(). commit() posts the changed rows one by one (the routes
 * answer `HX-Redirect` on success or a `#command-status` fragment); the
 * first refusal lands in the page's `#command-status` and stops, success
 * reloads the page. It also sorts the table under it (ui.sort_th headers,
 * `data-sort-*` on the rows, three-state like portal-list.js).
 */
(function () {
	'use strict';

	var OK = ['success', 'sent', 'ok', 'drafted', 'pending', 'queued'];

	function sortable(root) { sortTable(root.querySelector('[data-edit-set-table]')); }

	// Any table: the edit set's, or a read-only `[data-sort-table]` (the Hub
	// Gateways card), which boots itself on load and after each htmx swap.
	function sortTable(table) {
		if (!table || !table.tBodies[0] || table.dataset.sortInit) return;
		table.dataset.sortInit = '1';
		var body = table.tBodies[0];
		var sort = { key: null, dir: 0 };
		function val(tr, k) {
			var v = tr.dataset['sort' + k.charAt(0).toUpperCase() + k.slice(1)];
			return v === undefined ? '' : v;
		}
		function paint() {
			table.querySelectorAll('th[data-sort-key]').forEach(function (th) {
				var active = sort.dir && th.dataset.sortKey === sort.key;
				th.setAttribute('aria-sort', active ? (sort.dir === 1 ? 'ascending' : 'descending') : 'none');
				var chev = th.querySelector('.sort-chevron');
				if (!chev) return;
				chev.classList.toggle('invisible', !active);
				chev.classList.toggle('rotate-180', !!(active && sort.dir === 1));
			});
		}
		function apply() {
			var rows = Array.prototype.filter.call(body.rows, function (tr) { return tr.hasAttribute('data-row-id'); });
			rows.sort(function (a, b) {
				var seq = Number(b.dataset.seq) - Number(a.dataset.seq);
				if (!sort.dir) return seq;
				var x = val(a, sort.key), y = val(b, sort.key);
				if (x === '' && y === '') return seq;
				if (x === '') return 1;
				if (y === '') return -1;
				var nx = Number(x), ny = Number(y);
				var c = (!isNaN(nx) && !isNaN(ny)) ? sort.dir * (nx - ny) : sort.dir * x.localeCompare(y);
				return c || seq;
			});
			rows.forEach(function (tr) { body.appendChild(tr); });
			var ed = body.querySelector('[data-lim-row]');
			var at = ed && ed.dataset.after && body.querySelector('tr[data-row-id="' + ed.dataset.after + '"]');
			if (at) at.after(ed);
		}
		table.querySelectorAll('th[data-sort-key]').forEach(function (th) {
			var btn = th.querySelector('button') || th;
			btn.addEventListener('click', function () {
				var k = th.dataset.sortKey;
				if (sort.key !== k) sort = { key: k, dir: 1 };
				else if (sort.dir === 1) sort = { key: k, dir: -1 };
				else sort = { key: null, dir: 0 };
				paint();
				apply();
			});
		});
	}

	var KEYS = ['critical_low', 'warning_low', 'warning_high', 'critical_high'];
	// The mini bar's scale: the slider's track per unit (portal-thresholds.js
	// TRACK), else the bounds' own spread.
	var SCALE = { '\u00b0C': [-40, 50], '%': [0, 100] };

	function num(v) {
		if (v === null || v === undefined || v === '') return null;
		var n = Number(v);
		return isNaN(n) ? null : n;
	}
	function clone(o) { return JSON.parse(JSON.stringify(o)); }
	function same(a, b) { return JSON.stringify(a) === JSON.stringify(b); }

	function post(url, body) {
		return fetch(url, {
			method: 'POST',
			credentials: 'same-origin',
			headers: { 'HX-Request': 'true', 'Content-Type': 'application/x-www-form-urlencoded' },
			body: body
		}).then(function (r) {
			var redirect = r.headers.get('HX-Redirect');
			return r.text().then(function (html) {
				var frag = new DOMParser().parseFromString(html, 'text/html').getElementById('command-status');
				var status = frag ? (frag.getAttribute('data-status') || '') : '';
				var ok = r.ok && (!!redirect || status === '' || OK.indexOf(status) !== -1);
				return { ok: ok, redirect: redirect, frag: frag };
			});
		});
	}

	document.addEventListener('alpine:init', function () {
		Alpine.data('editSet', function (opts) {
			opts = opts || {};
			return {
				ids: opts.rows || [],
				on: opts.on || {},
				actions: opts.actions || {},
				names: opts.labels || {},
				bands: opts.bands || {},
				lims: opts.limits || {},
				ldraft: {},
				limOpen: null,
				limOn: true,
				limToggle: false,
				open: opts.open !== false,
				editing: !!opts.always,
				always: !!opts.always,
				busy: false,
				draft: {},

				init: function () { this.reset(); sortable(this.$root); },
				reset: function () {
					var d = {};
					var self = this;
					this.ids.forEach(function (id) { d[id] = !!self.on[id]; });
					this.draft = d;
					var l = {};
					Object.keys(this.lims).forEach(function (id) { l[id] = clone(self.lims[id].d); });
					this.ldraft = l;
					this.limOpen = null;
				},
				get changes() {
					var self = this;
					return this.ids.filter(function (id) { return self.draft[id] !== !!self.on[id]; }).length
						+ this.limIds().length;
				},
				start: function () { this.reset(); this.open = true; this.editing = true; },
				cancel: function () {
					if (this.busy) return;
					this.reset();
					this.editing = this.always;
					this.$dispatch('edit-set-cancel');
				},
				isOff: function (id) { return id in this.draft ? !this.draft[id] : !this.on[id]; },
				offIds: function () {
					var self = this;
					return this.ids.filter(function (id) { return self.on[id] && !self.draft[id]; });
				},
				offList: function () {
					var self = this;
					return this.offIds().map(function (id) { return self.names[id] || id; });
				},
				offBand: function () {
					var self = this;
					return this.offIds().some(function (id) { return self.bands[id]; });
				},
				rowClass: function (id) {
					if (!this.editing) return '';
					if (this.on[id] && !this.draft[id]) return 'es-to-off';
					if (!this.on[id] && this.draft[id]) return 'es-to-on';
					if (this.limChanged(id)) return 'es-changed';
					return '';
				},

				// --- Limits (ui_components.md §2.8) ---------------------
				limChanged: function (id) {
					return !!this.lims[id] && !!this.ldraft[id] && !same(this.ldraft[id], this.lims[id].d);
				},
				limIds: function () {
					var self = this;
					return Object.keys(this.lims).filter(function (id) { return self.limChanged(id); });
				},
				// The mini bar of a row, from its draft: { grad, pos, off },
				// or null for "—" (nothing configured and nothing accepted).
				mini: function (id) {
					var L = this.lims[id], d = this.ldraft[id];
					if (!L || !d) return null;
					var t = d.t || {};
					var v = KEYS.map(function (k) { return num(t[k]); });
					var present = v.filter(function (x) { return x !== null; });
					if (!present.length || (!L.configured && !this.limChanged(id))) return null;
					var lo, hi, sc = SCALE[L.unit];
					if (sc) { lo = Math.min(sc[0], Math.min.apply(null, present)); hi = Math.max(sc[1], Math.max.apply(null, present)); }
					else {
						lo = Math.min.apply(null, present); hi = Math.max.apply(null, present);
						var base = hi > lo ? hi - lo : Math.max(Math.abs(lo) * 0.4, 2);
						lo -= base * 0.25; hi += base * 0.25;
					}
					var span = hi - lo || 1;
					var pc = function (x) { return Math.max(0, Math.min(100, (x - lo) / span * 100)); };
					var cl = v[0] !== null ? pc(v[0]) : 0;
					var wl = Math.max(v[1] !== null ? pc(v[1]) : cl, cl);
					var ch = v[3] !== null ? pc(v[3]) : 100;
					var wh = Math.min(Math.max(v[2] !== null ? pc(v[2]) : ch, wl), ch);
					var f = function (x) { return x.toFixed(2) + '%'; };
					var grad = 'linear-gradient(to right, var(--color-critical) 0 ' + f(cl)
						+ ', var(--color-warning) ' + f(cl) + ' ' + f(wl)
						+ ', var(--color-success) ' + f(wl) + ' ' + f(wh)
						+ ', var(--color-warning) ' + f(wh) + ' ' + f(ch)
						+ ', var(--color-critical) ' + f(ch) + ' 100%)';
					var val = num(L.value);
					return { grad: grad, pos: val === null ? null : pc(val), off: L.toggle && !d.enabled };
				},
				limSlider: function () {
					var el = this.$refs.limslider && this.$refs.limslider.querySelector('[x-data]');
					return el ? Alpine.$data(el) : null;
				},
				limHyst: function () {
					return this.$refs.limrow ? this.$refs.limrow.querySelector('input[name="hysteresis"]') : null;
				},
				limEdit: function (id) {
					var L = this.lims[id];
					if (!L || !this.editing) return;
					this.limOpen = null;
					var row = this.$root.querySelector('tr[data-row-id="' + id + '"]');
					var ed = this.$refs.limrow;
					if (row && ed) { row.after(ed); ed.dataset.after = id; }
					var d = this.ldraft[id];
					this.limToggle = !!L.toggle;
					this.limOn = !!d.enabled;
					var h = this.limHyst();
					if (h) h.value = d.hysteresis === null || d.hysteresis === undefined ? '' : String(d.hysteresis);
					this.limOpen = id;
					var self = this;
					this.$nextTick(function () {
						var s = self.limSlider();
						if (s) {
							s.load({ unit: L.unit, value: L.value, bounds: d.t, seed: L.seed });
							if (s.measure) s.measure();
						}
					});
				},
				// The check: the editor's values become the row's draft.
				limAccept: function () {
					var id = this.limOpen;
					if (!id) return;
					var s = this.limSlider(), h = this.limHyst();
					var t = {};
					KEYS.forEach(function (k) { t[k] = s ? num(s.b[k]) : null; });
					this.ldraft[id] = {
						t: t,
						enabled: this.lims[id].toggle ? !!this.limOn : true,
						hysteresis: h ? num(h.value) : null
					};
					this.limOpen = null;
				},
				// The X: the row keeps the draft it had before this edit.
				limRevert: function () { this.limOpen = null; },
				limBody: function (id) {
					var d = this.ldraft[id], b = new URLSearchParams();
					KEYS.forEach(function (k) { b.set(k, d.t[k] === null ? '' : String(d.t[k])); });
					b.set('hysteresis', d.hysteresis === null ? '' : String(d.hysteresis));
					if (this.lims[id].toggle && d.enabled) b.set('enabled', 'true');
					return b.toString();
				},
				save: function () {
					if (!this.changes || this.busy) return;
					if (this.offIds().length && this.$refs.dlg) this.$refs.dlg.showModal();
					else this.commit();
				},
				commit: function () {
					var self = this;
					var todo = this.ids.filter(function (id) { return self.draft[id] !== !!self.on[id]; })
						.map(function (id) {
							return { url: self.actions[id], body: 'enabled=' + (self.draft[id] ? 'true' : 'false'),
								done: function () { self.on[id] = self.draft[id]; } };
						})
						.concat(this.limIds().map(function (id) {
							return { url: self.lims[id].post, body: self.limBody(id),
								done: function () { self.lims[id].d = clone(self.ldraft[id]); } };
						}));
					if (!todo.length) return;
					this.limOpen = null;
					this.busy = true;
					var redirect = null;
					var step = function (i) {
						if (i >= todo.length) {
							window.location.href = redirect || window.location.href;
							return;
						}
						var job = todo[i];
						post(job.url, job.body).then(function (res) {
							if (res.redirect) redirect = res.redirect;
							if (res.ok) {
								job.done();
								step(i + 1);
								return;
							}
							self.busy = false;
							var cur = document.getElementById('command-status');
							if (cur && res.frag) {
								cur.replaceWith(res.frag);
								if (window.htmx) htmx.process(res.frag);
							}
						}).catch(function () { self.busy = false; });
					};
					step(0);
				}
			};
		});
	});
	function bootSortTables(el) {
		if (!el || !el.querySelectorAll) return;
		if (el.matches && el.matches('[data-sort-table]')) sortTable(el);
		el.querySelectorAll('[data-sort-table]').forEach(sortTable);
	}
	document.addEventListener('DOMContentLoaded', function () { bootSortTables(document); });
	document.addEventListener('htmx:load', function (e) { bootSortTables(e.detail && e.detail.elt); });
	// ui.edit_block(multi=true): post each changed field to its own route,
	// in order; the first refusal lands in #command-status and stops.
	function changed(e) {
		if (e.tagName === 'SELECT') {
			return Array.prototype.some.call(e.options, function (o) { return o.selected !== o.defaultSelected; });
		}
		return e.value !== e.defaultValue;
	}
	// `displaySet` — the Hub LCD sensor picker (hub_display_picker.html):
	// the edit-set bar over a table of boxes, at most `max` ticked. Save
	// posts the whole selection at once (`sensors=<id>` repeated; none =
	// the built-in layout) and shows the answer in #command-status.
	document.addEventListener('alpine:init', function () {
		Alpine.data('displaySet', function (opts) {
			return {
				ids: opts.rows || [], on: opts.on || {}, max: opts.max || 0, action: opts.action,
				editing: false, busy: false, draft: {},
				init: function () { this.reset(); sortable(this.$root); },
				reset: function () {
					var d = {}, self = this;
					this.ids.forEach(function (id) { d[id] = !!self.on[id]; });
					this.draft = d;
				},
				get count() {
					var self = this;
					return this.ids.filter(function (id) { return self.draft[id]; }).length;
				},
				get changes() {
					var self = this;
					return this.ids.filter(function (id) { return !!self.draft[id] !== !!self.on[id]; }).length;
				},
				locked: function (id) { return !this.draft[id] && this.count >= this.max; },
				rowClass: function (id) {
					if (!this.editing) return '';
					if (this.on[id] && !this.draft[id]) return 'es-to-off';
					if (!this.on[id] && this.draft[id]) return 'es-to-on';
					return '';
				},
				start: function () { this.reset(); this.editing = true; },
				cancel: function () { if (this.busy) return; this.reset(); this.editing = false; },
				save: function () {
					var self = this;
					if (!this.changes || this.busy) return;
					this.busy = true;
					var body = this.ids.filter(function (id) { return self.draft[id]; })
						.map(function (id) { return 'sensors=' + encodeURIComponent(id); }).join('&');
					post(this.action, body).then(function (res) {
						var cur = document.getElementById('command-status');
						if (cur && res.frag) { cur.replaceWith(res.frag); if (window.htmx) htmx.process(res.frag); }
						self.busy = false;
						if (res.ok) {
							var on = {};
							self.ids.forEach(function (id) { on[id] = !!self.draft[id]; });
							self.on = on;
							self.editing = false;
						}
					}).catch(function () { self.busy = false; });
				}
			};
		});
	});

	window.proximosMultiPost = function (form) {
		var todo = Array.prototype.filter.call(form.querySelectorAll('[data-te-in][data-te-action]'), changed);
		var i = 0;
		function show(frag) {
			var cur = document.getElementById('command-status');
			if (cur && frag) {
				cur.replaceWith(frag);
				if (window.htmx) htmx.process(frag);
			}
		}
		function step() {
			if (i >= todo.length) return Promise.resolve(true);
			var e = todo[i++];
			var body = encodeURIComponent(e.dataset.tePost || e.name) + '=' + encodeURIComponent(e.value.trim());
			return post(e.dataset.teAction, body).then(function (res) {
				show(res.frag);
				return res.ok ? step() : false;
			});
		}
		return step().catch(function () { return false; });
	};
})();

