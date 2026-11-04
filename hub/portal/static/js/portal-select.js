/*
 * Row selection for the shared device table — ONE component for every list
 * that picks rows rather than opening them: /limits in its edit mode, and the
 * History sensor picker. Plain DOM over data attributes, no Alpine, so it
 * survives an htmx swap of the rows and works on any `<table>`.
 *
 * Markup contract:
 *
 *   [data-select]                  the root (usually the list's wrapper)
 *     data-select-active="1|0"     selection mode on/off. Off: the boxes are
 *                                  hidden (`[data-select-col]` cells get
 *                                  `hidden`) and a row click opens the row
 *                                  as everywhere (layout/base.html). On: a
 *                                  row click toggles the row instead
 *     data-select-same="quantity"  optional: only rows whose `data-<name>`
 *                                  equals the first selected row's can join
 *                                  (limits.cs.md §9.3); the others are
 *                                  disabled while a selection exists
 *     data-select-max="N"          optional: at most N rows; the rest are
 *                                  disabled with `data-select-max-title`
 *                                  as their tooltip
 *     data-select-per="owner"      optional, with data-select-per-max="N":
 *                                  at most N selected rows may share one
 *                                  `data-<name>` value
 *     data-select-rowclick="0"     optional: a row click keeps opening the
 *                                  row (/alarms); only the box selects
 *   [data-fit-viewport]            a table's scroll area: its height is the
 *                                  viewport minus the content above it and
 *                                  the element `data-fit-below` names (the
 *                                  buttons under it), so the whole picker
 *                                  fits on screen and filtering never
 *                                  changes its size; it scrolls both ways
 *   tr[data-select-id="…"]         a selectable row
 *     data-select-locked           never selectable (the group form: a
 *                                  channel another group holds)
 *     input[data-select-box]       its checkbox, inside a `[data-select-col]` cell
 *   input[data-select-all]         the header box: selects / clears the
 *                                  VISIBLE selectable rows; checked when all
 *                                  of them are, indeterminate when partial
 *   [data-select-count]            optional: gets "n" or "n/N"
 *   [data-select-clear]            optional: a button that clears everything
 *
 * Shift-click on a box or a row selects the range from the last clicked
 * row. Rows the list filter hid (class `hidden`) are never touched by
 * select-all or a range.
 *
 * Every change dispatches `select-change` on the root, detail =
 * { ids: [...], rows: [tr, ...] } in table order. `proximosSelect(root)`
 * returns { ids(), set(ids), clear(), active(on) } for a page script.
 */
(function () {
	'use strict';

	function init(root) {
		if (root._select) return root._select;
		var last = null;
		var sameAttr = root.dataset.selectSame || '';
		var max = parseInt(root.dataset.selectMax || '0', 10) || 0;
		var perAttr = root.dataset.selectPer || '';
		var perMax = parseInt(root.dataset.selectPerMax || '0', 10) || 0;

		function rows() {
			return Array.prototype.slice.call(root.querySelectorAll('tr[data-select-id]'));
		}
		function box(tr) { return tr.querySelector('input[data-select-box]'); }
		// A locked row counts as not there for select-all and ranges.
		function visible(tr) { return !tr.classList.contains('hidden') && !tr.hidden && !tr.hasAttribute('data-select-locked'); }
		function selected() { return rows().filter(function (tr) { var b = box(tr); return b && b.checked; }); }
		function isActive() { return root.dataset.selectActive === '1'; }

		// Which rows may still join: the same quantity as the first pick, and
		// room under the cap. A selected row always stays enabled, so it can
		// be cleared.
		function allowed(tr, sel) {
			var b = box(tr);
			if (b && b.checked) return true;
			if (tr.hasAttribute('data-select-locked')) return false;
			if (max && sel.length >= max) return false;
			if (perAttr && perMax) {
				var key = tr.dataset[perAttr] || '';
				if (key && sel.filter(function (s) { return (s.dataset[perAttr] || '') === key; }).length >= perMax) return false;
			}
			if (sameAttr && sel.length) return (tr.dataset[sameAttr] || '') === (sel[0].dataset[sameAttr] || '');
			return true;
		}

		// The visible rows the header box acts on: shown by the list filter,
		// not locked, and either selectable now or already selected (a row the
		// rules disable cannot be picked, so it must not keep the header box
		// from reading "all").
		function eligible() {
			return rows().filter(function (tr) {
				var b = box(tr);
				return b && visible(tr) && (b.checked || !b.disabled);
			});
		}

		// The header box mirrors the eligible rows: checked when all are,
		// indeterminate when some are, clear otherwise.
		function paintAll() {
			var all = root.querySelector('input[data-select-all]');
			if (!all) return;
			var el = eligible();
			var on = el.filter(function (tr) { return box(tr).checked; }).length;
			all.checked = on > 0 && on === el.length;
			all.indeterminate = on > 0 && on < el.length;
		}

		function paint() {
			var sel = selected();
			rows().forEach(function (tr) {
				var b = box(tr);
				if (!b) return;
				var ok = allowed(tr, sel);
				b.disabled = !ok;
				tr.classList.toggle('opacity-50', !ok && isActive());
				tr.toggleAttribute('data-selected', b.checked);
				if (tr.hasAttribute('data-select-locked')) { /* keeps its own title */ }
				else if (!ok && max && sel.length >= max && root.dataset.selectMaxTitle) tr.title = root.dataset.selectMaxTitle;
				else if (!ok && perAttr && root.dataset.selectPerTitle) tr.title = root.dataset.selectPerTitle;
				else tr.removeAttribute('title');
			});
			paintAll();
			var count = root.querySelector('[data-select-count]');
			if (count) count.textContent = max ? sel.length + '/' + max : String(sel.length);
			// `data-select-keep-col` on the root keeps the column while selection
			// is off (ui_components.md §3): the boxes stay, disabled and grey
			// (.es-check), and become live controls in selection mode.
			var keepCol = root.hasAttribute('data-select-keep-col');
			root.querySelectorAll('[data-select-col]').forEach(function (c) {
				if (!keepCol) { c.classList.toggle('hidden', !isActive()); return; }
				c.classList.remove('invisible');
				c.querySelectorAll('input[data-select-box], input[data-select-all]').forEach(function (b) { if (!isActive()) b.disabled = true; else if (b.hasAttribute('data-select-all')) b.disabled = false; });
			});
			root.dispatchEvent(new CustomEvent('select-change', {
				bubbles: true,
				detail: { ids: sel.map(function (tr) { return tr.dataset.selectId; }), rows: sel }
			}));
		}

		function setRow(tr, on) {
			var b = box(tr);
			if (!b) return;
			if (on && !allowed(tr, selected())) return;
			b.checked = on;
		}

		function toggle(tr, shift) {
			var b = box(tr);
			if (!b || b.disabled) return;
			var on = !b.checked;
			if (shift && last && last !== tr && rows().indexOf(last) !== -1) {
				var list = rows();
				var a = list.indexOf(last), z = list.indexOf(tr);
				var lo = Math.min(a, z), hi = Math.max(a, z);
				for (var i = lo; i <= hi; i++) if (visible(list[i])) setRow(list[i], on);
			} else {
				setRow(tr, on);
			}
			last = tr;
			paint();
		}

		// A click on the box: the browser already flipped it, so undo that and
		// let `toggle` decide (range, same-quantity rule) in one place.
		root.addEventListener('click', function (ev) {
			var el = ev.target instanceof Element ? ev.target : null;
			if (!el) return;
			var b = el.closest('input[data-select-box]');
			var tr = el.closest('tr[data-select-id]');
			if (b && tr) {
				b.checked = !b.checked;
				toggle(tr, ev.shiftKey);
				return;
			}
			if (el.closest('input[data-select-all]')) {
				// All eligible visible rows selected -> clear them; otherwise
				// pick every visible row the rules allow. Rows the filter hid
				// keep their state.
				var vis = rows().filter(visible);
				var el2 = eligible();
				var full = el2.length > 0 && el2.every(function (r) { return box(r).checked; });
				vis.forEach(function (r) { setRow(r, !full); });
				paint();
				return;
			}
			if (el.closest('[data-select-clear]')) { api.clear(); return; }
			// A row click in selection mode toggles the row; preventDefault
			// tells layout/base.html's row-opening handler to stand down.
			if (tr && isActive() && root.dataset.selectRowclick !== '0' && !el.closest('a, button, input, select, textarea, label')) {
				ev.preventDefault();
				if (ev.shiftKey) {
					var s = window.getSelection();
					if (s) s.removeAllRanges();
				}
				toggle(tr, ev.shiftKey);
			}
		});
		// A list filter hides or shows rows without touching a box; repaint
		// the header box once portal-list.js has applied it.
		['input', 'change'].forEach(function (t) {
			document.addEventListener(t, function (ev) {
				if (ev.target instanceof Element && ev.target.closest('[data-select-box], [data-select-all]')) return;
				setTimeout(paintAll, 0);
			});
		});

		// Shift+click selects text across rows before the click lands.
		root.addEventListener('mousedown', function (ev) {
			if (ev.shiftKey && isActive() && ev.target instanceof Element && ev.target.closest('tr[data-select-id]')) ev.preventDefault();
		});

		var api = {
			ids: function () { return selected().map(function (tr) { return tr.dataset.selectId; }); },
			set: function (ids) {
				rows().forEach(function (tr) { var b = box(tr); if (b) b.checked = false; });
				(ids || []).forEach(function (id) {
					var tr = rows().find(function (r) { return r.dataset.selectId === id; });
					if (tr) setRow(tr, true);
				});
				paint();
			},
			clear: function () { api.set([]); last = null; },
			active: function (on) {
				root.dataset.selectActive = on ? '1' : '0';
				if (!on) api.clear(); else paint();
			},
			paint: paint
		};
		root._select = api;
		paint();
		return api;
	}

	// Viewport fit: one height for the scroll area, recomputed on resize and
	// whenever the page layout moves (an editor opening above it, a swap).
	function fit() {
		document.querySelectorAll('[data-fit-viewport]').forEach(function (el) {
			var top = el.getBoundingClientRect().top + window.scrollY;
			var below = 16;
			var sel = el.dataset.fitBelow;
			var b = sel ? document.querySelector(sel) : null;
			if (b) below += b.getBoundingClientRect().height + 16;
			var min = parseInt(el.dataset.fitMin || '240', 10) || 240;
			var h = Math.max(min, Math.floor(window.innerHeight - top - below));
			if (Math.abs((parseFloat(el.style.height) || 0) - h) > 1) el.style.height = h + 'px';
		});
	}
	var fitQueued = false;
	function queueFit() {
		if (fitQueued) return;
		fitQueued = true;
		requestAnimationFrame(function () { fitQueued = false; fit(); });
	}
	window.proximosFit = queueFit;

	window.proximosSelect = init;
	function boot() {
		document.querySelectorAll('[data-select]').forEach(init);
		queueFit();
	}
	if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
	else boot();
	window.addEventListener('resize', queueFit);
	document.addEventListener('htmx:afterSettle', boot);
	document.addEventListener('portal-list:swapped', boot);
	if (window.ResizeObserver) {
		var ro = new ResizeObserver(queueFit);
		var watch = function () { if (document.body) ro.observe(document.body); };
		if (document.body) watch(); else document.addEventListener('DOMContentLoaded', watch);
	}
})();
