/*
 * Client-side toolbar for the device lists — /nodes and /beacons
 * (devices.node.md §2.2–§2.4, inherited by devices.beacon.md §2.2).
 *
 * Shape ported from enerooo-spark's smart list: a search box with a Filters
 * disclosure and a Columns dropdown beside it, and under them a panel whose
 * header always states the result count and the active filters while its field
 * grid collapses. Column visibility follows that product's rule too — a head
 * cell and its body cells share one `data-column`, and the picker toggles them
 * together (its `static/js/smart-list.js`).
 *
 * What did NOT come across: enerooo's toolbar is a `<form method="get">` and
 * its server filters. Here every row is already in the DOM (§2.9 option A), so
 * the controls drive a pass over it — which is also why the count and the
 * chips are built here and not rendered by the server. And where enerooo uses
 * Alpine for the disclosures, this is plain JS: the panel must stay open while
 * a filter is set, and that fact is produced by the filter pass, so one owner
 * is cheaper than two talking to each other.
 *
 * Everything the pass reads lives in `data-*` on the `<tr>` (see
 * partials/node_card.html's header for the list), which is what makes it
 * survive a row repainting itself: the fresh row arrives carrying the same
 * attributes and `htmx:afterSettle` re-runs the pass.
 *
 * The FILTER pass runs on `afterSettle`, not `afterSwap`: an outerHTML swap
 * keeps the row's `id`, so htmx copies the OLD element's `class` onto the new
 * one at swap time and restores the server's `class` a settle-tick later.
 * Hooking the earlier event re-hides the row just before htmx wipes the class
 * back off it — the same trap pages/dashboard.html documents at length.
 *
 * The COLUMN pass runs on both, and on `afterSwap` it is scoped to what was
 * swapped. Cells carry no `id`, so htmx never settles them: they arrive with
 * the server's classes, which is the default column set, and waiting a settle
 * tick to fix that made the table blink on every row repaint.
 *
 * State lives in the URL query (§2.2), so a filtered view is a shareable link
 * and survives both a reload and the broadcast-lag full refresh. Column
 * choice, sort direction and whether the panel is open do NOT: they are
 * per-user preferences, so they go to localStorage (§2.4, §2.4.1).
 */
(function () {
	'use strict';

	// The device lists' fields, each with a match rule of its own (a location
	// can be "none", a gateway select is built from the rows, `oor` has a
	// two-class option, `fault` asks only whether the row carries one).
	// Any OTHER `data-list-filter` on a page is a plain equality match against
	// the row's `data-<name>`, which is all a list like /settings/users needs
	// — see `extras` below.
	var BUILTIN = ['kind', 'location', 'connection', 'oor', 'proto', 'fault'];
	var PARAM = { kind: 'kind', location: 'loc', connection: 'conn', oor: 'oor', proto: 'proto', fault: 'fault' };

	function store(key, value) {
		try { localStorage.setItem(key, value); } catch (err) { /* private mode */ }
	}
	function forget(key) {
		try { localStorage.removeItem(key); } catch (err) { /* private mode */ }
	}
	function load(key, fallback) {
		try {
			var raw = localStorage.getItem(key);
			return raw === null ? fallback : raw;
		} catch (err) { return fallback; }
	}

	function init(root) {
		var key = root.dataset.listKey || 'list';
		var COLS_KEY = 'proximos.list.' + key + '.columns';
		var SORT_KEY = 'proximos.list.' + key + '.sort';
		var OPEN_KEY = 'proximos.list.' + key + '.filtersOpen';

		// Two list shapes. `/nodes` and `/beacons` are one table, so the rows
		// are its `<tr>` and the script can sort them. Monitoring is N group
		// sections in any of four layouts (two of them cards), so the rows are
		// `.monitoring-row` wherever they sit, there is no table to sort, and
		// a section whose every row was filtered out has to go with them.
		var table = root.querySelector('[data-list-table]');
		var body = table ? table.tBodies[0] : null;
		var sections = root.querySelectorAll('section[data-section]');
		// A third shape: /alarms filters on the SERVER and paginates, so it has
		// the same chrome and none of the rows. There is nothing here to filter
		// or sort — the disclosure and the Columns popover are the whole job,
		// and the count, the badge and the panel's initial state arrive
		// rendered. Wiring only those beats a second copy of the toggle.
		var serverMode = !body && sections.length === 0;

		// Which URL param the value filter answers to. Monitoring keeps
		// `status`, because that is what its metric tiles already link to.
		var oorRow = root.querySelector('[data-list-oor-param]');
		if (oorRow && oorRow.dataset.listOorParam) PARAM.oor = oorRow.dataset.listOorParam;

		var search = root.querySelector('[data-list-search]');
		var selects = {};
		BUILTIN.forEach(function (name) {
			selects[name] = root.querySelector('[data-list-filter="' + name + '"]');
		});
		// Generic fields: the name is both the row attribute (`data-role`) and
		// the URL param, and the rule is membership in the row's space-separated list. A list whose filters are
		// plain columns gets them without teaching this script anything.
		var extras = [];
		root.querySelectorAll('[data-list-filter]').forEach(function (el) {
			var name = el.dataset.listFilter;
			if (BUILTIN.indexOf(name) !== -1) return;
			extras.push(name);
			selects[name] = el;
		});
		var FILTERS = BUILTIN.concat(extras);
		var panel = root.querySelector('[data-list-panel]');
		var grid = root.querySelector('[data-list-grid]');
		var toggle = root.querySelector('[data-list-filters-toggle]');
		var countEl = root.querySelector('[data-list-count]');
		var chipsEl = root.querySelector('[data-list-chips]');
		var clearBtn = root.querySelector('[data-list-clear]');
		var gwSel = root.querySelector('[data-list-gwselect]');
		var noMatch = root.querySelector('[data-list-nomatch]');

		function rows() {
			if (body) return Array.prototype.slice.call(body.querySelectorAll('tr[data-kind]'));
			// Scoped to rows inside a section, and never to anything in a
			// group header: the header's badges are UNFILTERED group totals
			// (monitoring.backend.md §14.7) — a collapsed group still has to
			// inform, which is exactly the case where no row is on screen.
			return Array.prototype.slice.call(root.querySelectorAll('section[data-section] .monitoring-row'));
		}

		/* ---- default order ------------------------------------------------
		 * "Sort off" means the server's order, newest registration first
		 * (§2.4.1), and a Node that joins while the page is open is PREPENDED
		 * by its own SSE event. So the original order cannot be recovered from
		 * the data: it is stamped here, once per row, and a row that arrives
		 * without a stamp gets one below the current minimum — which puts it
		 * exactly where the prepend put it. */
		function stamp() {
			if (!body) return;
			var min = 0;
			rows().forEach(function (tr) {
				if (tr.dataset.seq) min = Math.min(min, Number(tr.dataset.seq));
			});
			rows().forEach(function (tr) {
				if (tr.dataset.seq) return;
				// A row an SSE/htmx swap replaced keeps its predecessor's
				// place in the default order; without this it took a fresh,
				// lowest seq and jumped to the end on every live update.
				if (tr.id && swappedSeq[tr.id]) {
					tr.dataset.seq = swappedSeq[tr.id];
					delete swappedSeq[tr.id];
					return;
				}
				tr.dataset.seq = String(--min);
			});
		}
		var swappedSeq = Object.create(null);
		document.body.addEventListener('htmx:beforeSwap', function (ev) {
			var t = ev.detail && ev.detail.target;
			if (!t || !body || !body.contains(t) && !t.contains(body)) return;
			var olds = t.matches && t.matches('tr[data-seq]') ? [t] : [];
			Array.prototype.push.apply(olds, t.querySelectorAll ? t.querySelectorAll('tr[data-seq]') : []);
			olds.forEach(function (tr) { if (tr.id) swappedSeq[tr.id] = tr.dataset.seq; });
		});

		/* ---- options built from the rows themselves ----------------------- */
		function fillLocations() {
			var sel = selects.location;
			if (!sel) return;
			var seen = Object.create(null);
			var values = [];
			rows().forEach(function (tr) {
				var loc = tr.dataset.location || '';
				if (!loc || seen[loc]) return;
				seen[loc] = true;
				values.push(loc);
			});
			values.sort(function (a, b) { return a.localeCompare(b); });
			var keep = sel.value;
			// Everything after the two fixed options ("all" and "none") is
			// rebuilt; a location that vanished with its last row must not
			// stay on offer.
			while (sel.options.length > 2) sel.remove(2);
			values.forEach(function (v) {
				var opt = document.createElement('option');
				opt.value = v;
				opt.textContent = v;
				sel.appendChild(opt);
			});
			if (keep && sel.querySelector('option[value="' + CSS.escape(keep) + '"]')) sel.value = keep;
		}

		function fillGateways() {
			if (!gwSel) return;
			var seen = Object.create(null);
			rows().forEach(function (tr) {
				(tr.dataset.gw || '').split(' ').forEach(function (id) {
					if (id) seen[id] = true;
				});
			});
			var ids = Object.keys(seen).sort();
			var keep = gwSel.value;
			// Same rebuild as the locations: everything after "all" goes, so a
			// gateway that fell silent with its last row is not still on offer.
			while (gwSel.options.length > 1) gwSel.remove(1);
			ids.forEach(function (id) {
				var opt = document.createElement('option');
				opt.value = id;
				opt.textContent = id;
				gwSel.appendChild(opt);
			});
			gwSel.value = ids.indexOf(keep) !== -1 ? keep : 'all';
		}

		function gwSelected() {
			return gwSel && gwSel.value !== 'all' ? [gwSel.value] : [];
		}

		/* ---- Protocol is only OFFERED in a genuinely mixed list (§2.2.5) ---
		 * A Node is hardware-bound to one transport, so in a single-transport
		 * fleet the badge would sit on every row saying nothing and the filter
		 * would be dead. The filter field, the inline badge and the picker's
		 * own Protocol entry all appear only once two transports are present.
		 *
		 * What this does NOT do any more is decide whether the Protocol
		 * COLUMN is visible: that is the picker's job, like every other
		 * column. It used to be neither — the column was drawn by the mixed
		 * check alone and the picker had no entry for it, so the one column
		 * an operator could not turn off was the one that appeared on its
		 * own. `protoMixed` is what the two mechanisms share; `paintCols`
		 * reads it. */
		var protoMixed = false;
		function syncProtocol() {
			var kinds = Object.create(null);
			rows().forEach(function (tr) { if (tr.dataset.proto) kinds[tr.dataset.proto] = true; });
			protoMixed = Object.keys(kinds).length > 1;
			var wrap = root.querySelector('[data-list-protowrap]');
			if (wrap) wrap.classList.toggle('hidden', !protoMixed);
			if (!protoMixed && selects.proto) selects.proto.value = 'all';
		}

		/* ---- optional columns ---------------------------------------------
		 * One `data-column` on the head cell and on every body cell of that
		 * column; the picker toggles the pair. Same handle enerooo-spark uses,
		 * and the reason is the same: a class name the script has to parse is
		 * one rename away from silently doing nothing. */
		// The checkboxes carry their own default, so a list where every column
		// is toggleable opens showing the useful ones rather than blank. An
		// ABSENT stored value means "never chose"; a stored `[]` means "chose
		// nothing", and those must not collapse into each other or a viewer
		// who hid every column would get them all back on the next load.
		function defaultCols() {
			return offeredBoxes()
				.filter(function (box) { return box.dataset.colDefault === '1'; })
				.map(function (box) { return box.dataset.col; });
		}
		// A column can be irrelevant to THIS fleet rather than merely off —
		// Protocol in a single-transport list. Such an entry is not rendered,
		// is not counted in the "3 / 10", and cannot be switched on by a
		// stored preference from a day the fleet was mixed.
		function offeredBoxes() {
			return Array.prototype.slice
				.call(root.querySelectorAll('[data-col]'))
				.filter(function (box) { return box.dataset.col !== 'proto' || protoMixed; });
		}
		function readCols() {
			var raw = load(COLS_KEY, null);
			if (raw === null) return defaultCols();
			try { return JSON.parse(raw) || []; }
			catch (err) { return defaultCols(); }
		}
		/* `scope` limits the cell pass to freshly swapped markup — see the
		 * `htmx:afterSwap` hook below. Omitted, it is the whole list, which
		 * is what the first paint and the settle pass want. The picker's own
		 * checkboxes are always root-scoped: they live in the toolbar and no
		 * swap brings new ones. */
		function paintCols(scope) {
			var offered = offeredBoxes().map(function (box) { return box.dataset.col; });
			var on = readCols().filter(function (c) { return offered.indexOf(c) !== -1; });
			root.querySelectorAll('[data-col]').forEach(function (box) {
				var live = offered.indexOf(box.dataset.col) !== -1;
				box.checked = live && on.indexOf(box.dataset.col) !== -1;
				var label = box.parentNode;
				if (label && label.tagName === 'LABEL') label.classList.toggle('hidden', !live);
			});
			var within = scope && scope.querySelectorAll ? scope : root;
			within.querySelectorAll('[data-column]').forEach(function (cell) {
				cell.classList.toggle('hidden', on.indexOf(cell.dataset.column) === -1);
			});
			// The inline badge beside the name is the compact form of the same
			// fact. It shows on a mixed fleet and steps aside the moment the
			// operator asks for the column, so Protocol is never in the row
			// twice.
			var badgeOff = !protoMixed || on.indexOf('proto') !== -1;
			within.querySelectorAll('.proto-col').forEach(function (el) {
				el.classList.toggle('hidden', badgeOff);
			});
			// Offered only when it would do something. Compared as sorted
			// sets, not as stored strings: turning a column off and back on
			// leaves the same choice in a different order, and a Reset that
			// stayed lit after that would be claiming a difference there is
			// none of.
			var count = root.querySelector('[data-list-cols-count]');
			if (count) {
				count.textContent = on.length + ' / ' + offered.length;
			}
		}

		/* ---- sorting ------------------------------------------------------- */
		var sort = { key: null, dir: 0 };
		try {
			var stored = JSON.parse(load(SORT_KEY, 'null'));
			if (stored && stored.key) sort = stored;
		} catch (err) { /* private mode */ }

		// A row may own a DETAIL row rendered immediately after it — today
		// only the Beacon threshold accordion (partials/beacon_thresholds_row.html),
		// which carries `data-row-for="<owner id>"`. It has no `data-kind`,
		// deliberately: the filter must not treat it as a Beacon, and its
		// `hidden` class is its collapsed state rather than a filter verdict.
		// That also means `rows()` never returns it, so both passes below
		// have to carry it along by hand.
		function mateOf(tr) {
			if (!body || !tr.id) return null;
			return body.querySelector('tr[data-row-for="' + tr.id + '"]');
		}

		function cmpValue(tr, k) {
			var raw = tr.dataset['sort' + k.charAt(0).toUpperCase() + k.slice(1)];
			return raw === undefined ? '' : raw;
		}
		/* Monitoring's tables (amendment 2026-10-15): in the detailed one
		 * each sensor is one `<tbody class="mon-sensor">` holding a `<tr>`
		 * per channel, so sorting moves whole blocks and a block's channels
		 * keep their order; in the simplified one a sensor is one `<tr>`. "Off" is the server's order, remembered per row id the
		 * first time a block is seen (an SSE swap keeps its id). */
		var monSeq = Object.create(null);
		var monNext = 0;
		function sortMonitorTables() {
			root.querySelectorAll('table[data-mon-table]').forEach(function (table) {
				var list = Array.prototype.slice.call(table.querySelectorAll(':scope > tbody.mon-sensor, :scope > tbody > tr.monitoring-row'));
				if (list.length < 2) return;
				list.forEach(function (tb) { if (!(tb.id in monSeq)) monSeq[tb.id] = monNext++; });
				var seq = function (tb) { return monSeq[tb.id]; };
				var sorted = list.slice().sort(function (a, b) {
					if (!sort.dir || !sort.key) return seq(a) - seq(b);
					var x = cmpValue(a, sort.key);
					var y = cmpValue(b, sort.key);
					if (x === '' && y === '') return seq(a) - seq(b);
					if (x === '') return 1;
					if (y === '') return -1;
					var nx = Number(x);
					var ny = Number(y);
					var c = (!isNaN(nx) && !isNaN(ny)) ? sort.dir * (nx - ny) : sort.dir * String(x).localeCompare(String(y));
					return c || seq(a) - seq(b);
				});
				if (sorted.every(function (el, i) { return el === list[i]; })) return;
				sorted.forEach(function (el) { el.parentNode.appendChild(el); });
			});
		}
		function sortRows() {
			if (!body) { sortMonitorTables(); return; }
			var list = rows();
			if (!sort.dir || !sort.key) {
				list.sort(function (a, b) { return Number(b.dataset.seq) - Number(a.dataset.seq); });
			} else {
				var dir = sort.dir;
				list.sort(function (a, b) {
					var x = cmpValue(a, sort.key);
					var y = cmpValue(b, sort.key);
					// A missing value sinks in BOTH directions: "no reading
					// yet" is not the smallest reading, and floating it to the
					// top on an ascending sort would bury the rows the
					// operator asked to see.
					if (x === '' && y === '') return Number(b.dataset.seq) - Number(a.dataset.seq);
					if (x === '') return 1;
					if (y === '') return -1;
					var nx = Number(x);
					var ny = Number(y);
					var c = (!isNaN(nx) && !isNaN(ny)) ? dir * (nx - ny) : dir * String(x).localeCompare(String(y));
					// Equal keys keep the default order, so a live update
					// never shuffles rows that tie.
					return c || Number(b.dataset.seq) - Number(a.dataset.seq);
				});
			}
			// Each row takes its detail row with it. Without this the
			// re-append moved every Beacon row to the end of the tbody and
			// left the accordions where they were — so all of them stacked
			// ABOVE the table (reported 2026-09-14: "collapsible opens above
			// the Beacon, not below").
			var want = [];
			list.forEach(function (tr) {
				want.push(tr);
				var mate = mateOf(tr);
				if (mate) want.push(mate);
			});
			// An unchanged order costs nothing. This pass runs on every
			// `afterSettle`, so on a list whose rows repaint every ten
			// seconds it used to rebuild the whole `tbody` that often for no
			// result: a reflow of every row, and any text the operator had
			// selected in the table lost, because a selection does not
			// survive its nodes being moved. The rows a swap brings arrive in
			// place (`outerHTML` keeps the position), so the common case is
			// exactly this early return.
			// Membership by the same attributes the two builders select on:
			// `rows()` takes `tr[data-kind]` and `mateOf` takes
			// `tr[data-row-for]`, so the empty-state and no-match rows are
			// correctly left out of the comparison.
			var seq = [];
			Array.prototype.forEach.call(body.children, function (el) {
				if (el.hasAttribute('data-kind') || el.hasAttribute('data-row-for')) seq.push(el);
			});
			if (seq.length === want.length && seq.every(function (el, i) { return el === want[i]; })) return;
			want.forEach(function (el) { body.appendChild(el); });
		}
		function paintSort() {
			root.querySelectorAll('th[data-sort-key]').forEach(function (th) {
				var active = sort.dir && th.dataset.sortKey === sort.key;
				var chev = th.querySelector('.sort-chevron');
				th.setAttribute('aria-sort', active ? (sort.dir === 1 ? 'ascending' : 'descending') : 'none');
				if (!chev) return;
				chev.classList.toggle('invisible', !active);
				chev.classList.toggle('rotate-180', !!(active && sort.dir === 1));
			});
		}

		/* ---- the panel ------------------------------------------------------
		 * Open state is remembered. An active filter shows the panel header
		 * with its chips, so what is narrowing the list stays visible, but
		 * never expands the grid of controls (Hynek 2026-10-15: a stat tile
		 * must not pop the Filters open). */
		function activeFilters() {
			var out = [];
			FILTERS.forEach(function (name) {
				var sel = selects[name];
				if (!sel || sel.value === 'all') return;
				if (name === 'proto' && sel.closest('[data-list-protowrap]') &&
					sel.closest('[data-list-protowrap]').classList.contains('hidden')) return;
				var label = root.querySelector('label[for="' + sel.id + '"]');
				out.push({
					name: name,
					label: label ? label.textContent.trim() : name,
					value: sel.options[sel.selectedIndex].textContent.trim(),
					clear: function () { sel.value = 'all'; },
				});
			});
			var gwLabelEl = root.querySelector('[data-list-gwlabel]');
			var gwLabel = gwLabelEl ? gwLabelEl.textContent.trim() : 'Gateway';
			gwSelected().forEach(function (id) {
				out.push({
					name: 'gw:' + id,
					label: gwLabel,
					value: id,
					clear: function () { gwSel.value = 'all'; },
				});
			});
			return out;
		}

		function paintPanel(visible, total, active) {
			var q = search && search.value.trim();
			var anything = active.length > 0 || !!q;

			if (toggle) {
				var chev = toggle.querySelector('.list-chevron');
				if (chev) chev.classList.toggle('rotate-180', open);
				toggle.setAttribute('aria-expanded', String(open));
				toggle.classList.toggle('btn-v1-primary', open);
				toggle.classList.toggle('btn-v1-outline', !open);
			}
			// The grid collapses; the header does not, because it is the only
			// place the list says how many rows survived.
			if (panel) panel.classList.toggle('hidden', !(open || anything));
			if (grid) grid.classList.toggle('hidden', !open);

			if (countEl) {
				countEl.textContent = (countEl.dataset.tpl || '{visible}/{total}')
					.replace('{visible}', visible)
					.replace('{total}', total)
					.replace('{entity}', countEl.dataset.entity || '');
			}
			if (clearBtn) clearBtn.classList.toggle('hidden', !anything);

			if (!chipsEl) return;
			chipsEl.textContent = '';
			active.forEach(function (f) {
				var chip = document.createElement('span');
				chip.className = 'inline-flex items-center gap-1.5 pl-2.5 pr-1.5 py-1 rounded-lg text-xs font-medium bg-primary/12 text-primary';
				chip.appendChild(document.createTextNode(f.label + ': ' + f.value));
				var x = document.createElement('button');
				x.type = 'button';
				x.className = 'p-0.5 rounded hover:bg-primary/20 leading-none';
				x.textContent = '×';
				x.title = (countEl && countEl.dataset.removeLabel) || '';
				x.addEventListener('click', function () { f.clear(); apply(); push(); });
				chip.appendChild(x);
				chipsEl.appendChild(chip);
			});
		}

		/* ---- the filter pass ----------------------------------------------- */
		// `alarming` spans two value classes. It exists because Monitoring's
		// metric tiles deep-link to it (`?status=alarming`) — one tile counting
		// "with alarms" cannot point at a filter that only names one class.
		function matchesValue(want, got) {
			if (want === 'all') return true;
			if (want === 'alarming') return got === 'warning' || got === 'critical';
			return got === want;
		}

		function apply() {
			var q = search ? search.value.trim().toLowerCase() : '';
			var kind = selects.kind ? selects.kind.value : 'all';
			var loc = selects.location ? selects.location.value : 'all';
			var conn = selects.connection ? selects.connection.value : 'all';
			var oor = selects.oor ? selects.oor.value : 'all';
			var proto = selects.proto ? selects.proto.value : 'all';
			var fault = selects.fault ? selects.fault.value : 'all';
			var gws = gwSelected();
			var all = rows();
			var visible = 0;

			all.forEach(function (tr) {
				var rowLoc = tr.dataset.location || '';
				// On Monitoring the group's own name is part of the haystack
				// (monitoring.md §2.2 point 1). It lives on the section rather
				// than on every row, so it is appended here instead of being
				// threaded through three row builders on the server.
				var hay = tr.dataset.search || '';
				var sec = tr.closest('section[data-section]');
				if (sec && sec.dataset.sectionName) hay += ' ' + sec.dataset.sectionName;
				var okSearch = !q || hay.indexOf(q) !== -1;
				var okKind = kind === 'all' || tr.dataset.kind === kind;
				var okLoc = loc === 'all' || (loc === '__none__' ? rowLoc === '' : rowLoc === loc);
				var okConn = conn === 'all' || (conn === 'online') === (tr.dataset.connected === '1');
				var okOor = matchesValue(oor, tr.dataset.oor);
				var okProto = proto === 'all' || tr.dataset.proto === proto;
				// Presence, never equality: /nodes and /beacons write the
				// fault KIND into `data-fault` and Monitoring writes "1", and
				// the only question #64's Poruchy tile asks is whether the
				// row carries one at all.
				var okFault = fault === 'all' || !!tr.dataset.fault;
				// Gateway is a checklist, so it is an OR inside an AND: the
				// row matches if ANY selected gateway heard it (§2.4).
				var heard = (tr.dataset.gw || '').split(' ');
				var okGw = gws.length === 0 || gws.some(function (id) { return heard.indexOf(id) !== -1; });
				var okExtra = extras.every(function (name) {
					var sel = selects[name];
					return !sel || sel.value === 'all' || (' ' + (tr.dataset[name] || '') + ' ').indexOf(' ' + sel.value + ' ') !== -1;
				});
				var ok = okSearch && okKind && okLoc && okConn && okOor && okProto && okFault && okGw && okExtra;
				tr.classList.toggle('hidden', !ok);
				// A filtered-out row hides its detail row too, but a matching
				// one never SHOWS it: on a detail row `hidden` is the
				// collapsed state, and clearing it here would spring every
				// accordion open on the first keystroke in the search box.
				if (!ok) {
					var mate = mateOf(tr);
					if (mate) mate.classList.add('hidden');
				}
				if (ok) visible++;
			});

			// A section whose every row was filtered out keeps its header —
			// name, location and the unfiltered totals — and swaps its body
			// for a short "no channel matches" line (monitoring.md, amendment
			// 2026-10-15). A group with no rows at all has its own message.
			//
			// A COLLAPSED group is deliberately not special-cased: collapse
			// sets the `hidden` ATTRIBUTE on the section's body, this sets the
			// `hidden` CLASS on rows and on the two body halves, so the
			// collapsed state survives any filter change.
			var filtering = !!q || activeFilters().length > 0;
			Array.prototype.forEach.call(sections, function (sec) {
				var secRows = sec.querySelectorAll('.monitoring-row');
				var any = Array.prototype.some.call(secRows, function (r) {
					return !r.classList.contains('hidden');
				});
				var empty = secRows.length > 0 && !any;
				// The channel-count badge reads "{shown} of {n}" while a
				// filter is on (#113), and goes back to the server's total
				// text when it is cleared. The header is an htmx swap target,
				// so the original text is stashed on the element each pass.
				var badge = sec.querySelector('[data-group-count]');
				if (badge) {
					if (badge.dataset.plain === undefined) badge.dataset.plain = badge.textContent;
					if (filtering) {
						var shown = 0;
						Array.prototype.forEach.call(secRows, function (r) {
							if (!r.classList.contains('hidden')) shown += parseInt(r.dataset.channels || '0', 10);
						});
						badge.textContent = (badge.dataset.filtered || '').replace('{shown}', shown);
					} else {
						badge.textContent = badge.dataset.plain;
					}
				}
				var content = sec.querySelector('[data-filter-content]');
				var msg = sec.querySelector('[data-filter-empty]');
				if (content && msg) {
					content.classList.toggle('hidden', empty);
					msg.classList.toggle('hidden', !empty);
				} else {
					sec.classList.toggle('hidden', empty);
				}
			});

			// "Nothing matches" is a different statement from "nothing is
			// registered", and the page has a separate row for the latter.
			if (noMatch) noMatch.classList.toggle('hidden', visible > 0 || all.length === 0);
			paintPanel(visible, all.length, activeFilters());
		}

		/* ---- URL query ------------------------------------------------------ */
		function push() {
			var params = new URLSearchParams(window.location.search);
			if (search && search.value.trim()) params.set('q', search.value.trim()); else params.delete('q');
			FILTERS.forEach(function (name) {
				var sel = selects[name];
				if (sel && sel.value !== 'all') params.set(PARAM[name] || name, sel.value);
				else params.delete(PARAM[name] || name);
			});
			var gws = gwSelected();
			if (gws.length) params.set('gw', gws.join(',')); else params.delete('gw');
			var qs = params.toString();
			// replaceState, not pushState: a filter is not a navigation, and
			// one history entry per keystroke would make Back useless.
			history.replaceState(null, '', window.location.pathname + (qs ? '?' + qs : ''));
		}

		function seed() {
			var params = new URLSearchParams(window.location.search);
			if (search && params.get('q')) search.value = params.get('q');
			FILTERS.forEach(function (name) {
				var sel = selects[name];
				var want = params.get(PARAM[name] || name);
				// Only a value the control actually offers: a junk param must
				// not leave the select showing blank.
				if (sel && want && sel.querySelector('option[value="' + CSS.escape(want) + '"]')) sel.value = want;
			});
			var gw = params.get('gw');
			if (gw && gwSel && gwSel.querySelector('option[value="' + CSS.escape(gw) + '"]')) gwSel.value = gw;
		}

		/* ---- wiring --------------------------------------------------------- */
		// Only the remembered toggle opens the filter grid. The server shows the
		// panel when a filter is set (a stat tile, a link with params), so the
		// chips stay visible, but that never expands the grid — the same rule
		// paintPanel applies client-side.
		var open = load(OPEN_KEY, 'false') === 'true';
		var panelShown = !!(serverMode && panel && !panel.classList.contains('hidden'));

		// Server mode has no filter pass to repaint from, so the toggle does
		// the showing and hiding itself.
		function setOpen(next) {
			open = next;
			store(OPEN_KEY, String(open));
			if (serverMode) {
				if (panel) panel.classList.toggle('hidden', !(open || panelShown));
				if (grid) grid.classList.toggle('hidden', !open);
				if (toggle) {
					var chev = toggle.querySelector('.list-chevron');
					if (chev) chev.classList.toggle('rotate-180', open);
					toggle.setAttribute('aria-expanded', String(open));
					toggle.classList.toggle('btn-v1-primary', open);
					toggle.classList.toggle('btn-v1-outline', !open);
				}
			} else {
				apply();
			}
		}

		if (toggle) {
			toggle.addEventListener('click', function () { setOpen(!open); });
		}
		if (clearBtn) {
			clearBtn.addEventListener('click', function () {
				if (search) search.value = '';
				FILTERS.forEach(function (name) { if (selects[name]) selects[name].value = 'all'; });
				if (gwSel) gwSel.value = 'all';
				apply();
				push();
			});
		}

		if (search) search.addEventListener('input', function () { apply(); push(); });
		FILTERS.forEach(function (name) {
			var sel = selects[name];
			if (sel) sel.addEventListener('change', function () { apply(); push(); });
		});
		if (gwSel) gwSel.addEventListener('change', function () { apply(); push(); });

		root.querySelectorAll('[data-col]').forEach(function (box) {
			box.addEventListener('change', function () {
				var on = readCols().filter(function (c) { return c !== box.dataset.col; });
				if (box.checked) on.push(box.dataset.col);
				store(COLS_KEY, JSON.stringify(on));
				paintCols();
			});
		});

		// Reset FORGETS the preference rather than storing the defaults. Those
		// are two different states — "never chose" follows the declared
		// defaults if they ever change, a stored copy of today's defaults
		// would pin the old set forever — and this is the one that means what
		// the button says.
		var colsReset = root.querySelector('[data-list-cols-reset]');
		if (colsReset) {
			colsReset.addEventListener('click', function () {
				forget(COLS_KEY);
				paintCols();
			});
		}

		root.querySelectorAll('th[data-sort-key]').forEach(function (th) {
			th.addEventListener('click', function () {
				var k = th.dataset.sortKey;
				// asc -> desc -> off, and "off" returns to the registration
				// order rather than to ascending (§2.4.1).
				if (sort.key !== k) sort = { key: k, dir: 1 };
				else if (sort.dir === 1) sort = { key: k, dir: -1 };
				else sort = { key: null, dir: 0 };
				store(SORT_KEY, JSON.stringify(sort));
				paintSort();
				sortRows();
			});
		});

		// The Columns dropdown: plain click-outside, no library.
		// `data-list-pop` marks the panel, its previous sibling is the button.
		root.querySelectorAll('[data-list-pop]').forEach(function (pop) {
			var btn = pop.previousElementSibling;
			if (!btn) return;
			btn.addEventListener('click', function (ev) {
				ev.stopPropagation();
				var closed = pop.classList.toggle('hidden');
				btn.setAttribute('aria-expanded', String(!closed));
				var chev = btn.querySelector('.list-chevron');
				if (chev) chev.classList.toggle('rotate-180', !closed);
			});
			pop.addEventListener('click', function (ev) { ev.stopPropagation(); });
		});
		document.addEventListener('click', function () {
			root.querySelectorAll('[data-list-pop]').forEach(function (pop) {
				pop.classList.add('hidden');
				var btn = pop.previousElementSibling;
				if (!btn) return;
				btn.setAttribute('aria-expanded', 'false');
				var chev = btn.querySelector('.list-chevron');
				if (chev) chev.classList.remove('rotate-180');
			});
		});

		/* The column choice is per-user and lives in localStorage, so every
		 * row the server re-renders arrives in the DEFAULT column set with
		 * the optional cells carrying `hidden`. The settle pass below fixes
		 * it a tick later, and that tick was visible: a Beacon row swaps on
		 * every `eye/sensors` publish, the row briefly lost three cells while
		 * the header kept them, and the auto-layout table reflowed every
		 * column width — the whole table blinked every few seconds.
		 *
		 * `afterSwap` runs in the same task as the DOM insertion, so painting
		 * here lands before the browser gets a chance to paint the row. Only
		 * CELLS are painted (scoped to what was swapped): they have no `id`,
		 * so htmx does not settle their attributes and the server's classes
		 * are already on them. The row's own class is a different matter —
		 * see the file header — which is why the filter pass stays on
		 * `afterSettle`. */
		function paintSwapped(ev) {
			var t = ev.target;
			paintCols(t && t.nodeType === 1 && document.contains(t) ? t : undefined);
		}

		if (serverMode) {
			setOpen(open);
			paintCols();
			// The table here is an htmx swap target — the SSE re-render and the
			// bulk-acknowledge response both replace it wholesale — and a
			// fresh one arrives with every column showing. Without this the
			// hidden ones come back the first time anyone acknowledges
			// anything.
			document.body.addEventListener('htmx:afterSwap', paintSwapped);
			document.body.addEventListener('htmx:afterSettle', function () { paintCols(); });
			liveServerSearch();
			return;
		}

		/* ---- server mode: search as you type --------------------------------
		 * /alarms and /audit paginate, so their search is a query and not a
		 * pass over the rows on screen. It used to run only on Enter, which
		 * read as "the search does nothing" next to every other list that
		 * filters on each keystroke. Now a pause in typing re-runs the form's
		 * GET in the background and swaps in the page's `[data-list-results]`
		 * and the panel header (count, chips) — the input itself is never
		 * replaced, so focus and caret stay. Pagination restarts at page one,
		 * as a submitted search would. */
		function liveServerSearch() {
			var input = root.querySelector('input[type="search"][name]');
			var form = input && input.form;
			if (!form) return;
			var timer = null;
			var ctl = null;
			var seq = 0;
			function url() {
				var params = new URLSearchParams();
				new FormData(form).forEach(function (v, k) {
					if (k !== 'offset' && String(v) !== '') params.append(k, v);
				});
				var qs = params.toString();
				return (form.getAttribute('action') || window.location.pathname) + (qs ? '?' + qs : '');
			}
			function run() {
				var target = url();
				var mine = ++seq;
				if (ctl) ctl.abort();
				ctl = window.AbortController ? new AbortController() : null;
				fetch(target, { credentials: 'same-origin', signal: ctl ? ctl.signal : undefined })
					.then(function (r) { return r.ok ? r.text() : Promise.reject(r.status); })
					.then(function (html) {
						if (mine !== seq) return;
						var doc = new DOMParser().parseFromString(html, 'text/html');
						var fresh = doc.querySelectorAll('[data-list-results]');
						root.querySelectorAll('[data-list-results]').forEach(function (el, i) {
							if (!fresh[i]) return;
							el.innerHTML = fresh[i].innerHTML;
							if (window.htmx) window.htmx.process(el);
						});
						var head = root.querySelector('[data-list-panel-head]');
						var newHead = doc.querySelector('[data-list-panel-head]');
						if (head && newHead) head.innerHTML = newHead.innerHTML;
						var newPanel = doc.querySelector('[data-list-panel]');
						if (panel && newPanel && !newPanel.classList.contains('hidden')) panel.classList.remove('hidden');
						history.replaceState(null, '', target);
						paintCols();
						document.body.dispatchEvent(new CustomEvent('portal-list:swapped', { bubbles: true }));
					})
					.catch(function () { /* aborted or failed: Enter still submits */ });
			}
			input.addEventListener('input', function () {
				clearTimeout(timer);
				timer = setTimeout(run, 250);
			});
		}

		function refresh() {
			stamp();
			fillLocations();
			fillGateways();
			syncProtocol();
			paintCols();
			paintSort();
			sortRows();
			apply();
		}

		stamp();
		fillLocations();
		fillGateways();
		syncProtocol();
		paintCols();
		seed();
		paintSort();
		sortRows();
		apply();

		document.body.addEventListener('htmx:afterSwap', paintSwapped);
		document.body.addEventListener('htmx:afterSettle', refresh);
	}

	/* ---- collapsibles --------------------------------------------------
	 * The device pages open their limits editor in a row under the field
	 * instead of a modal (partials/limits_editor.html). That is "a button
	 * toggles an
	 * element's `hidden` class", so both live here rather than as an inline
	 * script per page.
	 *
	 * Globals and inline `onclick`, not listeners: the Beacon row that
	 * carries one of these buttons is an SSE swap target, so it is replaced
	 * every few seconds. An attribute comes back already wired; a listener
	 * would have to be re-attached on every swap.
	 *
	 * The button is found through `aria-controls`, so a panel and its button
	 * share exactly one identifier.
	 */
	function paintToggle(id, open) {
		var btn = document.querySelector('[aria-controls="' + id + '"]');
		if (!btn) return;
		btn.setAttribute('aria-expanded', String(open));
		var chev = btn.querySelector('.list-chevron');
		if (chev) chev.classList.toggle('rotate-180', open);
		// Same open-state paint the Filters disclosure uses.
		if (btn.classList.contains('btn-v1')) {
			btn.classList.toggle('btn-v1-primary', open);
			btn.classList.toggle('btn-v1-outline', !open);
		}
	}

	window.proximosCollapse = function (id, open) {
		var el = document.getElementById(id);
		if (!el) return false;
		el.classList.toggle('hidden', !open);
		paintToggle(id, open);
		return open;
	};

	/** Returns the state it moved to, so a caller can lazily load on open. */
	window.proximosToggleCollapse = function (id) {
		var el = document.getElementById(id);
		if (!el) return false;
		return window.proximosCollapse(id, el.classList.contains('hidden'));
	};

	function boot() {
		document.querySelectorAll('[data-list-key]').forEach(init);
	}
	if (document.readyState === 'loading') {
		document.addEventListener('DOMContentLoaded', boot);
	} else {
		boot();
	}
})();
