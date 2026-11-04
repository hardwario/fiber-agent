// portal-charts.js — the chart island module.
//
// Still the "one dumb JS module" from plan/viewer/stack.md's Charts row:
// the server renders a bare container plus a small config, this file reads
// it, fetches the JSON and calls echarts.setOption. No client state beyond
// one echarts instance per island, no import/export (echarts is a UMD
// global from its own vendored <script>), loaded with a plain
// `<script defer>` from pages/history.html and pages/node_detail.html only.
//
// DOM contract — two island shapes, both `class="chart-island"`:
//
//   1. Single source (partials/chart.html, pages/node_detail.html):
//        data-chart-line               -> wired sensor line, or
//        data-chart-node + data-chart-field, or
//        data-chart-eye + data-chart-field
//        data-chart-range              -> "1h"|"6h"|"24h"|"7d"|"30d"
//        data-chart-empty              -> optional, id of the "no data" block
//
//   2. Config-driven (pages/history.html — many series in one chart):
//        data-chart-config    -> id of a <script type="application/json">
//        data-chart-empty     -> id of the "no data for this period" block
//        data-chart-agg-group -> id of the Aggregation control (a <select>
//                                on /history, a pill group on /nodes/{id})
//        data-chart-brush     -> id of the two-span row under the slider
//      The JSON is { range, agg, from, to, thresholds: bool,
//                    series: [{ url, name, color, unit }],
//                    events?: [{ ts, kind, severity, label }] }.
//      `events` (MB3's probe-detail window) are drawn as vertical markLines
//      with their label, and only while the island carries
//      data-chart-events="1" — the "Eventy v grafu" toggle writes that
//      attribute and fires `portal-chart:reconfigure` on the island.
//
//   Both shapes optionally carry data-label-critical-low/-warning-low/
//   -warning-high/-critical-high (translated `monitoring.threshold.*`
//   strings for the markLine labels) — falls back to English if absent.
//
// JSON contract — GET {url}?range=…&agg=… (cookie session auth,
// same-origin, plain fetch):
//   {
//     granularity: "raw"|"minute"|"hour",
//     agg_effective: "raw"|"30s"|"1m"|"10m"|"1h"|"1d",
//     points: [{ ts: unix_seconds, avg: float|null, min: …, max: … }],
//     thresholds: { critical_low, warning_low, warning_high,
//                   critical_high, alarm_low, alarm_high } | null,
//     sensor_name: string|null, unit: string
//   }
//
// Empty is a first-class answer: a response with no points hides the chart
// and shows the server-rendered empty block, because an axis-and-grid with
// nothing in it reads as "broken", not as "no readings here".
//
// TODO(SSE): plan/viewer/design.md §4 documents a future `chart-<id>` SSE
// event carrying this same JSON for live push updates. There is no SSE
// wiring for charts yet — the setInterval poll below is the only "live"
// behavior for now.

(() => {
	"use strict";

	const REFRESH_MS = 60_000;
	// Only short ranges auto-refresh — 7d/30d/1y charts are for review, not
	// live monitoring, and refetching a month of buckets every minute would
	// be wasted work for data that barely moves.
	const AUTO_REFRESH_RANGES = new Set(["1h", "6h"]);

	// Reads a daisyUI theme color, normalizing the modern space-separated
	// `hsl(H S% L%)` syntax (assets/css/app.css writes theme tokens this
	// way) to the legacy comma syntax `hsl(H, S%, L%)` — echarts/zrender's
	// own color parser is a hand-rolled regex that only recognizes the
	// legacy form, so a raw copy of the CSS variable would silently fail
	// to parse for anything that isn't a plain stroke/fill (e.g. areaStyle
	// opacity blending). Falls back to a fixed hex constant (approximating
	// the "proximos" light theme) if the variable is missing or in an
	// unrecognized format.
	function cssVar(name, fallback, alpha) {
		const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
		const m = raw.match(/^hsl\(\s*([\d.]+)\s+([\d.]+%)\s+([\d.]+%)\s*\)$/i);
		if (!m) return fallback;
		const [, h, s, l] = m;
		return alpha === undefined ? `hsl(${h}, ${s}, ${l})` : `hsla(${h}, ${s}, ${l}, ${alpha})`;
	}

	function readTheme() {
		return {
			primary: cssVar("--color-primary", "#f9a448"), // FIBER orange
			warning: cssVar("--color-warning", "#f59f0a"),
			error: cssVar("--color-error", "#ef4343"),
			text: cssVar("--color-base-content", "#57534e", 0.7),
			axisLine: cssVar("--color-base-content", "#a8a29e", 0.2),
			splitLine: cssVar("--color-base-content", "#a8a29e", 0.08),
			track: cssVar("--color-muted", "#f2f0ef"),
			surface: cssVar("--color-base-100", "#ffffff"),
			muted: cssVar("--color-base-content", "#a8a29e", 0.45),
		};
	}

	// The "proximos" light theme's values, frozen — the same constants
	// `cssVar` already falls back to when a variable is missing.
	//
	// A PNG export is ALWAYS light (monitoring.md §6.4, decision
	// 2026-09-01): it ends up in a report or on paper, where the reader's
	// theme is not a thing that exists. Forcing only the background white
	// is not enough — the axis labels, the tick text and the grid lines all
	// come from `readTheme()`, so on `proximos-dark` a white-background
	// capture would be pale grey text on white. This is the palette the
	// capture re-renders with before it grabs the canvas.
	const LIGHT_THEME = {
		primary: "#f9a448",
		warning: "#f59f0a",
		error: "#ef4343",
		text: "rgba(87, 83, 82, 0.7)",
		axisLine: "rgba(168, 162, 158, 0.2)",
		splitLine: "rgba(168, 162, 158, 0.08)",
		track: "#f2f0ef",
		surface: "#ffffff",
		muted: "rgba(168, 162, 158, 0.45)",
	};

	// Threshold key -> [label, color, line style]. Order controls draw
	// order (later = drawn on top of earlier at the same value).
	// `alarm_low`/`alarm_high` are deliberately not drawn: the firmware
	// keeps them only for backward compatibility (defaults 0/100 °C) and the
	// mock echoes them at the warning values, which drew two lines in one
	// place.
	//
	// `labels` comes from the island's `data-label-*` attributes (the
	// server-rendered translation for `monitoring.threshold.*`); falls back
	// to English when the template didn't set them.
	function thresholdSpecs(colors, labels) {
		return [
			["critical_low", labels.critical_low || "Critical low", colors.error, "dashed"],
			["warning_low", labels.warning_low || "Warning low", colors.warning, "dashed"],
			["warning_high", labels.warning_high || "Warning high", colors.warning, "dashed"],
			["critical_high", labels.critical_high || "Critical high", colors.error, "dashed"],
		];
	}

	// Limit lines, not area shading (UI-fixes #3: continuous lines with a
	// subtle label, no markArea under-coloring). `null` keys are skipped so
	// a sensor with only e.g. warning_high draws exactly one line, not five
	// hidden ones.
	//
	// The label sits at the START of each line, above it. `end` stacks every
	// label in the same few pixels of the right margin, where four of them
	// overlap into an unreadable smear; the left edge has the full grid
	// width to itself and the lines are horizontal, so a label per line is
	// legible as long as no two thresholds are within a few degrees.
	function buildMarkLine(thresholds, colors, unit, labels) {
		if (!thresholds) return undefined;
		const data = [];
		for (const [key, label, color, lineType] of thresholdSpecs(colors, labels)) {
			const value = thresholds[key];
			if (value === null || value === undefined) continue;
			data.push({
				yAxis: value,
				lineStyle: { color, type: lineType, width: 1 },
				label: {
					formatter: `${label} ${value}${unit}`,
					position: "insideStartTop",
					distance: 4,
					fontSize: 10,
					color,
				},
			});
		}
		if (!data.length) return undefined;
		return { silent: true, symbol: "none", animation: false, data };
	}

	// echarts ships EN and ZH locales only, so its time axis prints English
	// month and weekday names ("Aug 27", "Mon") on a Czech page — the one
	// part of the chart no `data-label-*` attribute can reach. Rather than
	// hardcoding a Czech name table, derive it from `<html lang>` through
	// Intl: a third language needs no change here. Passing the object (not a
	// registered locale name) to echarts.init is deliberate — echarts merges
	// an object over its EN defaults, so everything we omit still resolves.
	let localeCache;
	function axisLocale() {
		if (localeCache) return localeCache;
		const lang = document.documentElement.lang || "en";
		const names = (opts, count, at) =>
			Array.from({ length: count }, (_, i) =>
				new Intl.DateTimeFormat(lang, { ...opts, timeZone: "UTC" }).format(at(i)),
			);
		const month = (i) => Date.UTC(2021, i, 1);
		// 2021-01-03 was a Sunday; echarts' dayOfWeek arrays are Sunday-first.
		const weekday = (i) => Date.UTC(2021, 0, 3 + i);
		localeCache = {
			time: {
				month: names({ month: "long" }, 12, month),
				monthAbbr: names({ month: "short" }, 12, month),
				dayOfWeek: names({ weekday: "long" }, 7, weekday),
				dayOfWeekAbbr: names({ weekday: "short" }, 7, weekday),
			},
		};
		return localeCache;
	}

	// Every other timestamp on the page renders in the installation's display
	// zone (decisions.md #25; `<body data-tz>`, src/display_tz.rs) — the chart
	// must match, not the viewer's browser timezone. echarts only knows UTC or
	// the browser's zone, so ticks stay on UTC (`useUTC`) and every label is
	// formatted here in the display zone. "sv-SE" is the locale whose short
	// date is "YYYY-MM-DD HH:MM:SS": unambiguous and locale-independent.
	const TZ = (document.body && document.body.dataset.tz) || "UTC";
	const fullFmt = new Intl.DateTimeFormat("sv-SE", {
		timeZone: TZ,
		year: "numeric",
		month: "2-digit",
		day: "2-digit",
		hour: "2-digit",
		minute: "2-digit",
		second: "2-digit",
		hourCycle: "h23",
	});
	const zoneFmt = new Intl.DateTimeFormat("en-GB", { timeZone: TZ, timeZoneName: "short" });
	function fmtTime(ms) {
		return fullFmt.format(ms);
	}
	// The page's own shape for an instant (render::display_dt, default mode):
	// "26. 9. 07:20:20 CEST" / "Sep 26, 07:20:20 CEST". For labels that sit
	// on the page rather than inside the chart, so they read like the rest.
	const partsFmt = new Intl.DateTimeFormat("en-GB", {
		timeZone: TZ,
		day: "numeric",
		month: "numeric",
		hour: "2-digit",
		minute: "2-digit",
		second: "2-digit",
		hourCycle: "h23",
	});
	const MONTH_ABBR_EN = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
	function fmtPage(ms) {
		const p = {};
		for (const part of partsFmt.formatToParts(ms)) p[part.type] = part.value;
		const hms = `${p.hour}:${p.minute}:${p.second}`;
		const out = document.documentElement.lang === "cs"
			? `${p.day}. ${p.month}. ${hms}`
			: `${MONTH_ABBR_EN[Number(p.month) - 1]} ${p.day}, ${hms}`;
		return `${out} ${zoneAt(ms)}`;
	}
	// "CEST" / "CET" / "UTC" — the zone as it stood at `ms`.
	function zoneAt(ms) {
		const part = zoneFmt.formatToParts(ms).find((p) => p.type === "timeZoneName");
		return part ? part.value : TZ;
	}
	// Tick labels: time of day, or a date, in the zone.
	//
	// echarts places the ticks on UTC (`useUTC`), so on a long axis its day
	// ticks sit at UTC midnight — 02:00 in Prague in summer — and "the date
	// where the local time is 00:00" never fired: a 30-day chart read "02:00"
	// under every tick (hardware test 2026-10-09). So the label depends on
	// the span the axis shows. Over DATE_SPAN_MS a tick on a UTC day boundary
	// is that day's date (the UTC date, so a zone west of UTC does not label
	// it with the day before) and any finer tick its local time; at or under
	// it, local time, with the date at local midnight.
	const DAY_MS = 86400000;
	const DATE_SPAN_MS = 2 * DAY_MS;
	const hourFmt = new Intl.DateTimeFormat(document.documentElement.lang || "en", {
		timeZone: TZ,
		hour: "2-digit",
		minute: "2-digit",
		hourCycle: "h23",
	});
	const dayFmt = new Intl.DateTimeFormat(document.documentElement.lang || "en", {
		timeZone: TZ,
		day: "numeric",
		month: "numeric",
	});
	const utcDayFmt = new Intl.DateTimeFormat(document.documentElement.lang || "en", {
		timeZone: "UTC",
		day: "numeric",
		month: "numeric",
	});
	function fmtTick(ms, spanMs) {
		if (spanMs > DATE_SPAN_MS && ms % DAY_MS === 0) return utcDayFmt.format(ms);
		const hm = hourFmt.format(ms);
		return hm === "00:00" ? dayFmt.format(ms) : hm;
	}
	// The span the x axis shows now, in ms — zoomed in, the zoomed window —
	// or null before the chart has a model.
	function axisSpan(chart) {
		const model = chart && chart.getModel();
		const axis = model && model.getComponent("xAxis", 0);
		if (!axis || !axis.axis) return null;
		const [lo, hi] = axis.axis.scale.getExtent();
		return hi - lo;
	}
	// The span of the data itself, for when the axis has none yet.
	function dataSpan(datasets) {
		let lo = Infinity;
		let hi = -Infinity;
		for (const d of datasets) {
			for (const p of d.points) {
				if (p.ts * 1000 < lo) lo = p.ts * 1000;
				if (p.ts * 1000 > hi) hi = p.ts * 1000;
			}
		}
		return hi > lo ? hi - lo : 0;
	}

	function fmtValue(v, unit, decimals) {
		if (v === null || v === undefined) return "–";
		return `${v.toFixed(decimals)}${unit}`;
	}

	// Distinct units in order of appearance, capped at two — one y-axis
	// each (first left, second right). "%" always takes the right axis when
	// it shares the chart with another unit, so e.g. temperature+humidity
	// reads as °C left / % right regardless of selection order. A third
	// unit (rare) falls back to the left axis rather than growing a third
	// axis nobody can read.
	function axisUnits(datasets) {
		const units = [];
		for (const d of datasets) if (!units.includes(d.unit)) units.push(d.unit);
		const pct = units.indexOf("%");
		if (pct !== -1 && units.length > 1) {
			units.splice(pct, 1);
			units.splice(1, 0, "%");
		}
		return units.slice(0, 2);
	}

	// Event markLine tone, by kind (monitoring.md §6.3: "tón dle druhu —
	// warning/critical (hodnotové), neutral (offline/online), info
	// (ruční)"). A value event takes its severity's colour, so a critical
	// breach and a warning breach are not the same line.
	function eventColor(ev, colors) {
		if (ev.kind === "value_raised" || ev.kind === "value_cleared") {
			return ev.severity === "critical" ? colors.error : colors.warning;
		}
		if (ev.kind === "manual") return colors.primary;
		// Trends events (monitoring.md, amendment 2026-10-15): the /audit kinds.
		if (ev.kind === "alarm") return colors.error;
		if (ev.kind === "warning") return colors.warning;
		if (ev.kind === "log") return colors.muted;
		return colors.axisLine;
	}

	// Events as vertical lines on the time axis. One carrier series holding
	// no points: echarts hangs a markLine off a series, and giving the
	// events their own empty one keeps them out of the tooltip, out of the
	// legend and off both y-axes' extents — a markLine on a data series
	// would otherwise make that series' colour and axis look like the
	// event's.
	function esc(v) {
		return String(v == null ? "" : v).replace(/[&<>"']/g, (c) => ({
			"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
		})[c]);
	}

	// A Trends event (it carries `href`) is hoverable and clickable and has
	// no rotated label — eight series' worth of labels would bury the
	// curves; the tooltip says what the line is.
	function buildEventSeries(events, colors, labels) {
		if (!events || !events.length) return null;
		const interactive = events.some((ev) => ev.href);
		const kindLabel = (k) => (labels && labels[`ev_${k}`]) || k;
		return {
			name: "events",
			type: "line",
			data: [],
			silent: !interactive,
			tooltip: { show: false },
			animation: false,
			markLine: {
				silent: !interactive,
				symbol: "none",
				animation: false,
				tooltip: interactive
					? {
						show: true,
						trigger: "item",
						formatter: (p) => {
							const ev = p.data && p.data.ev;
							if (!ev) return "";
							const ms = ev.ts * 1000;
							const dot = `<span style="display:inline-block;width:8px;height:8px;border-radius:9999px;background:${esc(ev.color)};margin-right:6px"></span>`;
							return `${fmtTime(ms)} ${zoneAt(ms)}<br/><b style="color:${eventColor(ev, colors)}">${esc(kindLabel(ev.kind))}</b><br/>${dot}${esc(ev.item)}<br/>${esc(ev.message)}`;
						},
					}
					: undefined,
				data: events.map((ev) => ({
					xAxis: ev.ts * 1000,
					ev: ev.href ? ev : undefined,
					lineStyle: {
						color: eventColor(ev, colors),
						type: ev.href ? "solid" : "dashed",
						width: ev.href ? 1.5 : 1,
					},
					emphasis: ev.href ? { lineStyle: { width: 3 } } : undefined,
					label: ev.href
						? { show: false }
						: {
							formatter: ev.label || "",
							position: "insideEndTop",
							distance: 4,
							rotate: 90,
							fontSize: 10,
							color: eventColor(ev, colors),
						},
				})),
			},
		};
	}

	// A series whose every value is the same collapses echarts' `scale: true`
	// axis onto a single tick, and the curve draws as a flat line pinned to
	// the frame — which reads as "broken", not as "this fridge is holding
	// 4.0 °C". monitoring.md §6.1 asks for a fixed +/-1 unit margin in that
	// case; the 20% boundaryGap that handles every other series cannot,
	// because 20% of a zero range is zero.
	//
	// Returns `{min, max}` for the axis, or null to leave the automatic
	// behaviour alone. `%` is clamped at 0..100 on the low side only — a
	// humidity axis running to -1 is nonsense, one running to 101 is merely
	// unused space.
	const FLAT_EPSILON = 1e-9;
	function flatBounds(datasets, unit) {
		let lo = Infinity;
		let hi = -Infinity;
		for (const d of datasets) {
			if (d.unit !== unit) continue;
			for (const p of d.points) {
				for (const v of [p.avg, p.min, p.max]) {
					if (v === null || v === undefined) continue;
					if (v < lo) lo = v;
					if (v > hi) hi = v;
				}
			}
		}
		if (!Number.isFinite(lo) || hi - lo > FLAT_EPSILON) return null;
		return { min: unit === "%" ? Math.max(0, lo - 1) : lo - 1, max: hi + 1 };
	}

	// UX-019: a hole in the data must read as a hole, not as a straight line
	// inventing a slow drift across it. Rule: two samples further apart than
	// GAP_FACTOR × the series' typical spacing get a null point between them,
	// which breaks the line (`connectNulls: false`). The typical spacing
	// follows whatever cadence the answer has (raw uplinks, 1 min or 1 h
	// buckets), and a lost uplink or two stays joined. Render-only: the
	// statistics and the CSV export are computed server-side and never see
	// these points.
	//
	// The typical spacing is not the plain median of the steps: a 1-Wire
	// probe (or a history replay) delivers a burst of samples seconds apart
	// in every ~10 min uplink, so the plain median is a few seconds and every
	// uplink-to-uplink step became a "gap" — the series drew as lone dots.
	// Samples are first grouped into BURST_S windows (a window opens at its
	// first sample, so steady sub-minute data does not chain into one) and
	// the median is taken between window starts. MIN_GAP_S is the floor under
	// the result, so no cadence mix can break a line over a lost uplink; a
	// real outage (hours without data) still shows as a gap.
	const GAP_FACTOR = 3;
	const BURST_S = 60;
	const MIN_GAP_S = 15 * 60;
	function gapLimit(points) {
		const starts = [points[0].ts];
		for (const p of points) {
			if (p.ts - starts[starts.length - 1] >= BURST_S) starts.push(p.ts);
		}
		const steps = [];
		for (let i = 1; i < starts.length; i++) steps.push(starts[i] - starts[i - 1]);
		if (steps.length === 0) return MIN_GAP_S;
		steps.sort((a, b) => a - b);
		return Math.max(MIN_GAP_S, GAP_FACTOR * steps[Math.floor(steps.length / 2)]);
	}
	function withGaps(points) {
		if (points.length < 3) return points;
		const limit = gapLimit(points);
		if (!(limit > 0)) return points;
		const out = [points[0]];
		for (let i = 1; i < points.length; i++) {
			const prev = points[i - 1];
			if (points[i].ts - prev.ts > limit) {
				out.push({ ts: prev.ts + 1, avg: null, min: null, max: null, gap: true });
			}
			out.push(points[i]);
		}
		// A sample alone between two gaps has no line to be part of, and the
		// series draw no symbols, so it gets a dot of its own.
		return out.map((p, i) =>
			!p.gap && (i === 0 || out[i - 1].gap) && (i === out.length - 1 || out[i + 1].gap)
				? { ...p, lone: true }
				: p,
		);
	}

	// One series per selected sensor. A single series also gets the min..max
	// range band and (when the server sent them) the threshold lines; with
	// several sensors on one pair of axes neither would be readable, which
	// is exactly what v1 does too.
	function buildOption(datasets, colors, wantThresholds, labels, events, chart) {
		const single = datasets.length === 1;
		const fallbackSpan = dataSpan(datasets);
		const units = axisUnits(datasets);
		const axisFor = (d) => Math.max(0, units.indexOf(d.unit));
		const series = [];

		if (single && datasets[0].points.some((p) => p.min !== null && p.max !== null)) {
			const d = datasets[0];
			// The "two stacked transparent lines" band: an invisible base
			// line at `min`, then an invisible line stacked on top whose
			// value is (max - min) so the cumulative stacked value lands
			// exactly on `max`. Only the second gets an areaStyle, giving a
			// ~10%-opacity fill between min and max. This is a data range
			// indicator, not threshold shading (that is markLine, below).
			series.push({
				name: "range-low",
				type: "line",
				yAxisIndex: axisFor(d),
				data: d.points.map((p) => [p.ts * 1000, p.min]),
				stack: "range",
				symbol: "none",
				lineStyle: { opacity: 0 },
				silent: true,
				tooltip: { show: false },
				animation: false,
			});
			series.push({
				name: "range",
				type: "line",
				yAxisIndex: axisFor(d),
				data: d.points.map((p) => [
					p.ts * 1000,
					p.min === null || p.max === null ? null : p.max - p.min,
				]),
				stack: "range",
				symbol: "none",
				lineStyle: { opacity: 0 },
				areaStyle: { color: d.color, opacity: 0.1 },
				silent: true,
				tooltip: { show: false },
				animation: false,
			});
		}

		for (const d of datasets) {
			series.push({
				name: d.name,
				type: "line",
				yAxisIndex: axisFor(d),
				data: d.points.map((p) =>
					p.lone
						? { value: [p.ts * 1000, p.avg], symbol: "circle", symbolSize: 4 }
						: [p.ts * 1000, p.avg],
				),
				symbol: "none",
				smooth: false,
				connectNulls: false,
				lineStyle: { color: d.color, width: 2 },
				itemStyle: { color: d.color },
				markLine:
					single && wantThresholds
						? buildMarkLine(d.thresholds, colors, d.unit, labels)
						: undefined,
			});
		}

		const eventSeries = buildEventSeries(events, colors, labels);
		if (eventSeries) series.push(eventSeries);

		return {
			// Ticks are placed on UTC; their labels, the tooltip and the
			// dataZoom handles are formatted in the display zone (see
			// fmtTime), never in the browser's local timezone.
			useUTC: true,
			textStyle: { color: colors.text },
			grid: { left: 56, right: units.length > 1 ? 56 : 24, top: 16, bottom: 78 },
			xAxis: {
				type: "time",
				axisLine: { lineStyle: { color: colors.axisLine } },
				axisLabel: {
					color: colors.text,
					hideOverlap: true,
					formatter: (v) => fmtTick(v, axisSpan(chart) || fallbackSpan),
				},
				splitLine: { show: false },
			},
			// No axis `name`: echarts floats it above the axis, outside the
			// grid, where it reads as a stray "°C" pinned to nothing. The
			// unit belongs on the tick labels themselves. One axis per unit
			// (max two, see axisUnits).
			//
			// boundaryGap pads the data extent ~20% on both sides before
			// echarts rounds the bounds outward to nice tick values, so a
			// low-amplitude series (temperature) isn't squashed flat against
			// a wide co-plotted one, and rescaling on dataZoom keeps working.
			// alignTicks makes the second axis reuse the first axis's tick
			// positions (at the cost of exact 20% padding on it), so both
			// units share one set of horizontal grid lines instead of the
			// right axis's labels floating between the left axis's lines.
			yAxis: units.map((u, i) => {
				const decimals = u === "%" ? 0 : 1;
				const flat = flatBounds(datasets, u);
				return {
					type: "value",
					scale: true,
					alignTicks: true,
					boundaryGap: ["20%", "20%"],
					// Only set for a flat series; `undefined` leaves the
					// automatic extent, which is what every other series wants.
					min: flat ? flat.min : undefined,
					max: flat ? flat.max : undefined,
					position: i === 0 ? "left" : "right",
					axisLine: { show: false },
					axisTick: { show: false },
					axisLabel: {
						color: colors.text,
						formatter: (v) => `${v.toFixed(decimals)}${u}`,
					},
					splitLine: i === 0 ? { lineStyle: { color: colors.splitLine } } : { show: false },
				};
			}),
			tooltip: {
				trigger: "axis",
				axisPointer: { type: "line" },
				formatter: (params) => {
					const rows = (Array.isArray(params) ? params : [params]).filter(
						(p) => p.seriesName !== "range" && p.seriesName !== "range-low",
					);
					if (!rows.length) return "";
					const head = `${fmtTime(rows[0].value[0])} ${zoneAt(rows[0].value[0])}`;
					const body = rows
						.map((p) => {
							const d = datasets.find((x) => x.name === p.seriesName);
							const dot = `<span style="display:inline-block;width:8px;height:8px;border-radius:9999px;background:${p.color};margin-right:6px"></span>`;
							return `${dot}${p.seriesName} <b>${fmtValue(p.value[1], d ? d.unit : "", d ? d.decimals : 1)}</b>`;
						})
						.join("<br/>");
					return `${head}<br/>${body}`;
				},
			},
			legend: { show: false },
			dataZoom: [
				{ type: "inside" },
				{
					type: "slider",
					height: 14,
					bottom: 32,
					borderColor: "transparent",
					backgroundColor: colors.track,
					fillerColor: colors.primary,
					dataBackground: { lineStyle: { opacity: 0 }, areaStyle: { opacity: 0 } },
					selectedDataBackground: {
						lineStyle: { opacity: 0 },
						areaStyle: { opacity: 0 },
					},
					handleStyle: { color: colors.surface, borderColor: colors.primary, borderWidth: 2 },
					moveHandleStyle: { opacity: 0 },
					labelFormatter: (v) => fmtTime(v),
					textStyle: { color: colors.text, fontSize: 10 },
				},
			],
			series,
		};
	}

	// --- island wiring ------------------------------------------------

	function readConfig(el) {
		const configId = el.dataset.chartConfig;
		if (configId) {
			const node = document.getElementById(configId);
			if (!node) return null;
			try {
				return JSON.parse(node.textContent);
			} catch (err) {
				console.error("portal-charts: bad chart config", configId, err);
				return null;
			}
		}
		// Single-source island: one series, no explicit bucket. Three
		// flavours of source — a wired line, a Node field, a Beacon field —
		// and the Node/Beacon pair is what lets a detail page swap the
		// charted quantity by writing `data-chart-field` and firing
		// `portal-chart:reconfigure`.
		const field = el.dataset.chartField;
		let url;
		if (el.dataset.chartEye && field) {
			url = `/api/v1/chart/eye/${encodeURIComponent(el.dataset.chartEye)}/${encodeURIComponent(field)}`;
		} else if (el.dataset.chartNode && field) {
			url = `/api/v1/chart/node/${encodeURIComponent(el.dataset.chartNode)}/${encodeURIComponent(field)}`;
		} else {
			url = `/api/v1/chart/${encodeURIComponent(el.dataset.chartLine)}`;
		}
		return {
			range: el.dataset.chartRange || "24h",
			agg: "auto",
			thresholds: true,
			series: [{ url, name: "", color: cssVar("--color-primary", "#f9a448"), unit: "" }],
		};
	}

	// Threshold line labels, translated server-side and passed down as
	// `data-label-*` attributes (no `tojson` filter registered in this
	// minijinja setup, so one attribute per key rather than a JSON blob).
	// Missing attributes fall back to English in thresholdSpecs.
	function readLabels(el) {
		return {
			critical_low: el.dataset.labelCriticalLow,
			warning_low: el.dataset.labelWarningLow,
			warning_high: el.dataset.labelWarningHigh,
			critical_high: el.dataset.labelCriticalHigh,
			ev_alarm: el.dataset.labelEvAlarm,
			ev_warning: el.dataset.labelEvWarning,
			ev_log: el.dataset.labelEvLog,
		};
	}

	function markActiveAgg(el, effective) {
		const groupId = el.dataset.chartAggGroup;
		if (!groupId || !effective) return;
		const group = document.getElementById(groupId);
		if (!group) return;
		// Only when the server chose for us: an explicit pick is already
		// rendered as chosen and must not be moved under the operator.
		if (group.tagName === "SELECT") {
			// Not the operator's pick: show the bucket actually served. "Auto"
			// stays selected and names it ("Auto (10min)"); the default
			// bucket moves to the coarser one the server escalated to.
			if (group.dataset.aggExplicit) return;
			if (group.value === "auto") {
				const auto = group.querySelector('option[value="auto"]');
				const eff = group.querySelector(`option[value="${effective}"]`);
				if (auto && eff) {
					if (!auto.dataset.base) auto.dataset.base = auto.textContent;
					auto.textContent = `${auto.dataset.base} (${eff.textContent})`;
				}
			} else {
				group.value = effective;
			}
			if (window.proximosSingleSelect) window.proximosSingleSelect.sync(group);
			return;
		}
		if (group.querySelector(".pill.is-active")) return;
		const pill = group.querySelector(`[data-agg="${effective}"]`);
		if (pill) pill.classList.add("is-active");
	}

	function initIsland(el) {
		// htmx fires its first `load` on the body right after
		// DOMContentLoaded, so both entry points see the same islands on a
		// full page load; the flag makes the second one a no-op. An island
		// that arrived in a swap is a fresh element and carries no flag.
		if (el.dataset.chartReady === "1" && echarts.getInstanceByDom(el)) return;
		const config = readConfig(el);
		if (!config || !config.series || !config.series.length) return;

		const existing = echarts.getInstanceByDom(el);
		if (existing) existing.dispose();
		// A swap of the same island (Trends selection): the old chart's last
		// frame stays on top until the new data is drawn, so the card never
		// goes blank (see `carry` below).
		attachCarry(el);
		const chart = echarts.init(el, null, { locale: axisLocale() });
		el.dataset.chartReady = "1";
		// echarts.init empties the element, so the carried frame goes back on.
		let overlay = el._carryOverlay || null;
		el._carryOverlay = null;
		if (overlay) pinOverlay(el, overlay);
		const colors = readTheme();
		const labels = readLabels(el);
		const empty = el.dataset.chartEmpty ? document.getElementById(el.dataset.chartEmpty) : null;
		// The last successful fetch, kept so a re-render (the PNG capture's
		// light pass, and its restore) needs no second round trip.
		let lastAnswers = null;
		// Everything this init binds to the element itself, so
		// `disposeIsland` can take it all off again. The element survives a
		// dispose-then-init (that is what `portal-chart:reconfigure` does),
		// so a listener added here and never removed would fire once per
		// init the element had ever seen — which is how one click on PNG
		// downloaded N files (review item 3, 2026-09-10).
		if (el._chartAbort) el._chartAbort.abort();
		const abort = new AbortController();
		el._chartAbort = abort;
		// A Trends event line opens its alarm or its log entry.
		chart.on("click", (p) => {
			const ev = p && p.componentType === "markLine" && p.data && p.data.ev;
			if (ev && ev.href) window.location.href = ev.href;
		});

		function query() {
			const params = new URLSearchParams();
			if (config.range === "custom" && config.from && config.to) {
				params.set("from", config.from);
				params.set("to", config.to);
			} else {
				params.set("range", config.range);
			}
			if (config.agg) params.set("agg", config.agg);
			return params.toString();
		}

		function show(hasData) {
			if (empty) empty.style.display = hasData ? "none" : "flex";
			el.style.display = hasData ? "" : "none";
		}

		async function refresh() {
			if (!overlay) chart.showLoading();
			try {
				const qs = query();
				const answers = await Promise.all(
					config.series.map(async (s) => {
						// A wired line names no Hub in its path, so the series
						// carries the one it belongs to (devices.hub.md §12 E).
						// Absent on a single-Hub portal, where the endpoint
						// resolves the only Hub by itself.
						const sq = s.hub ? `${qs}&hub=${encodeURIComponent(s.hub)}` : qs;
						const res = await fetch(`${s.url}?${sq}`, { credentials: "same-origin" });
						if (!res.ok) throw new Error(`chart fetch failed: HTTP ${res.status}`);
						const json = await res.json();
						const unit = s.unit || json.unit || "";
						return {
							name: s.name || json.sensor_name || "",
							color: s.color,
							unit,
							decimals: unit === "%" ? 0 : 1,
							points: withGaps(json.points || []),
							thresholds: json.thresholds,
							agg_effective: json.agg_effective,
						};
					}),
				);
				const hasData = answers.some((a) => a.points.length > 0);
				lastAnswers = hasData ? answers : null;
				show(hasData);
				if (hasData) {
					// The toggle lives on the element, not in the config: it
					// is a per-viewer preference the server never sees, so
					// it is read here, on the way into `buildOption`,
					// rather than baked into the config the server sent.
					const showEvents = el.dataset.chartEvents === "1";
					chart.setOption(
						buildOption(
							answers,
							colors,
							!!config.thresholds,
							labels,
							showEvents ? config.events : null,
							chart,
						),
						true,
					);
					chart.resize();
				}
				markActiveAgg(el, answers[0] && answers[0].agg_effective);
			} catch (err) {
				console.error("portal-charts: failed to load chart", err);
			} finally {
				chart.hideLoading();
				if (overlay) {
					overlay.remove();
					// The clip was for the frame only; tooltips may overflow.
					el.style.overflow = "";
					overlay = null;
				}
			}
		}

		// "Save this chart as a PNG" — dispatched by the probe-detail
		// window's PNG button, handled here because this is where the
		// fetched series and the option builder live.
		//
		// Re-render light, capture, re-render normal. `getDataURL` flushes
		// zrender synchronously, so the on-screen chart never shows the
		// light frame — and `animation: false` on the restore keeps it from
		// tweening back through it.
		el.addEventListener("portal-chart:png", (ev) => {
			if (!lastAnswers) return;
			const events = el.dataset.chartEvents === "1" ? config.events : null;
			const option = (palette) =>
				buildOption(lastAnswers, palette, !!config.thresholds, labels, events, chart);
			chart.setOption(option(LIGHT_THEME), true);
			const url = chart.getDataURL({ type: "png", pixelRatio: 2, backgroundColor: "#ffffff" });
			chart.setOption(option(colors), true);
			const a = document.createElement("a");
			a.href = url;
			a.download = (ev.detail && ev.detail.filename) || "chart.png";
			a.click();
		}, { signal: abort.signal });

		refresh();
		if (AUTO_REFRESH_RANGES.has(config.range)) {
			// Cleared by disposeIsland when htmx swaps the card away, and
			// dies with the page on a full navigation.
			el._chartTimer = setInterval(refresh, REFRESH_MS);
		}

		// v1 labels the range brush's ends permanently; echarts only shows
		// them mid-drag, so mirror the current dataZoom window into the
		// server-rendered span pair under the slider.
		const brush = el.dataset.chartBrush ? document.getElementById(el.dataset.chartBrush) : null;
		if (brush) {
			const paint = () => {
				// `finished` also fires for a chart that was never given an
				// option (the empty-window case), where there is no model.
				const model = chart.getModel();
				const axis = model && model.getComponent("xAxis", 0);
				if (!axis) {
					// A swapped-in chart has no axis until its data arrives;
					// keep the carried times until then.
					if (overlay) return;
					brush.innerHTML = "";
					return;
				}
				const [lo, hi] = axis.axis.scale.getExtent();
				brush.innerHTML = `<span>${fmtPage(lo)}</span><span>${fmtPage(hi)}</span>`;
			};
			chart.on("dataZoom", paint);
			chart.on("finished", paint);
		}

		const observer = new ResizeObserver(() => chart.resize());
		observer.observe(el);
		el._chartObserver = observer;
	}

	// Last frame of an island that htmx is swapping out, by element id, for
	// the replacement island to show until its own data is drawn.
	const carry = new Map();
	const carryBrush = new Map();

	function disposeIsland(el) {
		if (el._chartTimer) clearInterval(el._chartTimer);
		if (el._chartObserver) el._chartObserver.disconnect();
		// Takes every listener this element's last init added with it (see
		// the AbortController in `initIsland`).
		if (el._chartAbort) {
			el._chartAbort.abort();
			el._chartAbort = null;
		}
		delete el.dataset.chartReady;
		const chart = echarts.getInstanceByDom(el);
		if (chart) chart.dispose();
	}

	// Put the carried frame over a freshly swapped island at once: htmx's
	// `load` (which runs `initIsland`) comes a settle later, and without
	// this the card shows nothing in between.
	// The frame must cover the island's own box and nothing else. The island
	// is the containing block (echarts.init and dispose rewrite its inline
	// style, so this is set again every time) and clips the image.
	function pinOverlay(el, img) {
		el.style.position = "relative";
		el.style.overflow = "hidden";
		el.appendChild(img);
	}

	// The island's own brush row, looked up next to it rather than by id:
	// during an outerHTML swap htmx inserts the new subtree before cleaning
	// up the old one, so both ids exist and getElementById finds the new,
	// still empty row.
	function brushOf(el) {
		const id = el.dataset.chartBrush;
		const scope = el.parentElement;
		return (id && scope && scope.querySelector(`[id="${id}"]`)) || null;
	}

	function attachCarry(el) {
		if (el._carryOverlay || !el.id || !carry.has(el.id)) return;
		const img = document.createElement("img");
		img.src = carry.get(el.id);
		img.alt = "";
		img.setAttribute("aria-hidden", "true");
		img.style.cssText =
			"position:absolute;inset:0;width:100%;height:100%;object-fit:fill;pointer-events:none;z-index:10";
		pinOverlay(el, img);
		el._carryOverlay = img;
		carry.delete(el.id);
		// The brush times under the chart stay too, until the new chart
		// repaints them.
		const brush = brushOf(el);
		if (brush && carryBrush.has(el.id)) brush.innerHTML = carryBrush.get(el.id);
		carryBrush.delete(el.id);
	}

	function snapshotIsland(el) {
		const chart = el.id && echarts.getInstanceByDom(el);
		if (!chart || el.style.display === "none") return;
		try {
			const brush = brushOf(el);
			if (brush) carryBrush.set(el.id, brush.innerHTML);
			carry.set(el.id, chart.getDataURL({ type: "png", pixelRatio: window.devicePixelRatio || 1 }));
		} catch (err) {
			carry.delete(el.id);
		}
	}

	function initAll(root) {
		const scope = root && root.querySelectorAll ? root : document;
		if (scope.classList && scope.classList.contains("chart-island")) initIsland(scope);
		scope.querySelectorAll(".chart-island").forEach(initIsland);
	}

	document.addEventListener("DOMContentLoaded", () => initAll(document));

	// "Redraw this island with whatever its attributes now say" — the
	// probe-detail window's "Eventy v grafu" toggle flips
	// `data-chart-events` and fires this. A dispose-then-init rather than a
	// bare `setOption`, because the option is rebuilt from the fetched
	// answers and those live inside `initIsland`'s closure.
	document.addEventListener("portal-chart:reconfigure", (e) => {
		const el = e.target;
		if (!el || !el.classList || !el.classList.contains("chart-island")) return;
		disposeIsland(el);
		initIsland(el);
	});

	// PNG download of a drawn chart (Trends "Download" menu, ui_components.md
	// amendment 2026-10-15). Client-side from echarts: the chart on the page
	// background plus a legend strip (the chart's own legend is hidden, the
	// picker rail carries the colours on screen), saved as
	// <prefix>-YYYYMMDD-HHMM.png.
	function pad2(n) {
		return String(n).padStart(2, "0");
	}

	function exportPng(el, prefix) {
		const chart = el && echarts.getInstanceByDom(el);
		if (!chart) return;
		const css = getComputedStyle(document.body);
		const bg = css.backgroundColor && css.backgroundColor !== "rgba(0, 0, 0, 0)" ? css.backgroundColor : "#ffffff";
		const fg = css.color || "#111827";
		const ratio = 2;
		const src = new Image();
		src.onload = () => {
			const opt = chart.getOption() || {};
			const palette = opt.color || [];
			const items = (opt.series || [])
				.filter((s) => s && s.name && !String(s.id || "").startsWith("_"))
				.map((s, i) => ({
					name: String(s.name),
					color: (s.lineStyle && s.lineStyle.color) || (s.itemStyle && s.itemStyle.color) || palette[i % palette.length] || fg,
				}))
				.filter((it, i, a) => a.findIndex((o) => o.name === it.name) === i);
			const font = `${12 * ratio}px system-ui, sans-serif`;
			const canvas = document.createElement("canvas");
			const ctx = canvas.getContext("2d");
			ctx.font = font;
			const pad = 12 * ratio;
			const rowH = 20 * ratio;
			// Lay the legend out in rows that fit the chart width.
			const rows = [[]];
			let x = pad;
			for (const it of items) {
				const w = 18 * ratio + ctx.measureText(it.name).width + 16 * ratio;
				if (x + w > src.width - pad && rows[rows.length - 1].length) {
					rows.push([]);
					x = pad;
				}
				rows[rows.length - 1].push({ ...it, x });
				x += w;
			}
			const legendH = items.length ? rows.length * rowH + pad : 0;
			canvas.width = src.width;
			canvas.height = src.height + legendH;
			ctx.fillStyle = bg;
			ctx.fillRect(0, 0, canvas.width, canvas.height);
			ctx.drawImage(src, 0, 0);
			ctx.font = font;
			ctx.textBaseline = "middle";
			rows.forEach((row, r) => {
				const y = src.height + r * rowH + rowH / 2;
				for (const it of row) {
					ctx.fillStyle = typeof it.color === "string" ? it.color : fg;
					ctx.fillRect(it.x, y - 2 * ratio, 12 * ratio, 4 * ratio);
					ctx.fillStyle = fg;
					ctx.fillText(it.name, it.x + 18 * ratio, y);
				}
			});
			const d = new Date();
			const name = `${prefix || "chart"}-${d.getFullYear()}${pad2(d.getMonth() + 1)}${pad2(d.getDate())}-${pad2(d.getHours())}${pad2(d.getMinutes())}.png`;
			const a = document.createElement("a");
			a.href = canvas.toDataURL("image/png");
			a.download = name;
			document.body.appendChild(a);
			a.click();
			a.remove();
		};
		src.src = chart.getDataURL({ type: "png", pixelRatio: ratio, backgroundColor: bg });
	}

	document.addEventListener("click", (e) => {
		const btn = e.target.closest && e.target.closest("[data-chart-png]");
		if (!btn) return;
		const pop = btn.closest("[popover]");
		if (pop && pop.hidePopover) pop.hidePopover();
		exportPng(document.getElementById(btn.dataset.chartPng), btn.dataset.chartPngPrefix);
	});

	// htmx swaps the History page's `#history-main` in place (see
	// pages/history.html), so every re-render brings a brand new island
	// that DOMContentLoaded will never see again.
	document.addEventListener("htmx:load", (e) => initAll(e.detail.elt));
	document.addEventListener("htmx:beforeCleanupElement", (e) => {
		const el = e.detail.elt;
		if (!el || !el.querySelectorAll) return;
		const gone = (i) => {
			snapshotIsland(i);
			disposeIsland(i);
		};
		if (el.classList.contains("chart-island")) gone(el);
		el.querySelectorAll(".chart-island").forEach(gone);
	});
	document.addEventListener("htmx:afterSwap", (e) => {
		const root = e.detail.target && document.getElementById(e.detail.target.id);
		if (root && root.querySelectorAll) root.querySelectorAll(".chart-island").forEach(attachCarry);
	});
	// A frame no replacement island picked up is dropped once the swap is in.
	document.addEventListener("htmx:afterSettle", () => {
		carry.clear();
		carryBrush.clear();
	});

	// Swap `fresh` into `old` everywhere but along the path to `keep`: a
	// node on that path keeps its identity (its attributes and the nodes
	// beside it are taken from `fresh`), `keep` itself only gets new
	// children. A removed popover closes, so this is what lets the Events
	// dropdown stay open across a redraw. Old chart islands hand their last
	// frame over as htmx's swap does, the new ones are initialised.
	function swapAround(old, fresh, keep) {
		for (const a of [...old.attributes]) if (!fresh.hasAttribute(a.name)) old.removeAttribute(a.name);
		for (const a of [...fresh.attributes]) old.setAttribute(a.name, a.value);
		if (old === keep) {
			old.replaceChildren(...fresh.childNodes);
			return;
		}
		const next = fresh.querySelector(`#${keep.id}`);
		let freshPath = next;
		while (freshPath && freshPath.parentElement !== fresh) freshPath = freshPath.parentElement;
		let oldPath = keep;
		while (oldPath.parentElement !== old) oldPath = oldPath.parentElement;
		for (const child of [...old.childNodes]) {
			if (child === oldPath) continue;
			if (child.querySelectorAll) {
				const islands = [...child.querySelectorAll(".chart-island")];
				if (child.classList.contains("chart-island")) islands.push(child);
				islands.forEach((i) => { snapshotIsland(i); disposeIsland(i); });
			}
			child.remove();
		}
		const kids = [...fresh.childNodes];
		const at = kids.indexOf(freshPath);
		oldPath.before(...kids.slice(0, at));
		oldPath.after(...kids.slice(at + 1));
		swapAround(oldPath, freshPath, keep);
	}

	// The Events dropdown (partials/history_controls.html): a tick redraws
	// the chart at once, and the dropdown stays open, so several kinds can
	// be picked in a row. The URL is built from the boxes, not the link, so
	// ticks quicker than the server still add up. The links stay the no-JS
	// path.
	let eventsSeq = 0;
	document.addEventListener("click", async (e) => {
		const item = e.target.closest && e.target.closest('#history-events [role="menuitemcheckbox"]');
		if (!item) return;
		e.preventDefault();
		e.stopPropagation();
		const on = item.getAttribute("aria-checked") !== "true";
		item.setAttribute("aria-checked", on ? "true" : "false");
		const box = item.querySelector('input[type="checkbox"]');
		if (box) box.checked = on;
		const pop = item.closest("[popover]");
		const picked = [...pop.querySelectorAll('[role="menuitemcheckbox"][aria-checked="true"]')]
			.map((a) => a.dataset.kind);
		const all = pop.querySelectorAll('[role="menuitemcheckbox"]').length;
		const url = new URL(window.location.href);
		url.searchParams.delete("ev");
		url.searchParams.delete("evt");
		if (picked.length) {
			url.searchParams.set("ev", "1");
			// All three = no `evt` (EventOpts::qs); a hidden log box means
			// the boxes shown are not all three.
			if (!(picked.length === 3 && all === 3)) url.searchParams.set("evt", picked.join(","));
		}
		const href = url.pathname + "?" + url.searchParams.toString().replace(/%2C/g, ",").replace(/%3A/g, ":");
		history.replaceState({}, "", href);
		const seq = ++eventsSeq;
		let html;
		try {
			const res = await fetch(href, { headers: { "HX-Request": "true" } });
			if (!res.ok) throw new Error(res.status);
			html = await res.text();
		} catch (err) {
			window.location.href = href;
			return;
		}
		// A later tick's answer wins; this one is stale.
		if (seq !== eventsSeq) return;
		const fresh = new DOMParser().parseFromString(html, "text/html").getElementById("history-main");
		const main = document.getElementById("history-main");
		const keep = document.getElementById("history-events");
		if (!fresh || !main || !keep || !fresh.querySelector("#history-events")) {
			window.location.href = href;
			return;
		}
		swapAround(main, document.importNode(fresh, true), keep);
		main.querySelectorAll(".chart-island").forEach(attachCarry);
		htmx.process(main);
		initAll(main);
		carry.clear();
		carryBrush.clear();
	}, true);
})();
