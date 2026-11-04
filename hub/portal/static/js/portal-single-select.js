// Single-choice dropdown (partials/ui.html `single_select`,
// plan/portal/ui_components.md §4). The hidden SELECT is the state; this
// file mirrors it into the button label and the check marks, and turns a
// row click into `select.value = ...` plus a bubbling `change`.
(function () {
	"use strict";

	function sync(select) {
		const root = select && select.closest("[data-single-select]");
		if (!root) return;
		const opt = select.options[select.selectedIndex];
		const cur = root.querySelector("[data-ss-current]");
		if (cur) cur.textContent = opt ? opt.textContent : "";
		root.querySelectorAll("[data-ss-value]").forEach(function (b) {
			const on = b.dataset.ssValue === select.value;
			b.setAttribute("aria-selected", on ? "true" : "false");
			const mark = b.querySelector("[data-ss-check]");
			if (mark) mark.style.visibility = on ? "" : "hidden";
			const o = Array.prototype.find.call(select.options, function (x) { return x.value === b.dataset.ssValue; });
			const t = b.querySelector("[data-ss-text]");
			if (o && t) t.textContent = o.textContent;
		});
	}

	document.addEventListener("click", function (ev) {
		const item = ev.target instanceof Element && ev.target.closest("[data-ss-value]");
		if (!item) return;
		const root = item.closest("[data-single-select]");
		const select = root && root.querySelector("select");
		if (!select) return;
		const pop = item.closest("[popover]");
		if (pop && pop.hidePopover) { try { pop.hidePopover(); } catch (e) { /* closed */ } }
		if (select.value === item.dataset.ssValue) return;
		select.value = item.dataset.ssValue;
		sync(select);
		select.dispatchEvent(new Event("change", { bubbles: true }));
	});

	function syncAll() {
		document.querySelectorAll("[data-single-select] select").forEach(sync);
	}
	document.addEventListener("DOMContentLoaded", syncAll);
	document.addEventListener("htmx:afterSettle", syncAll);

	window.proximosSingleSelect = { sync: sync };
})();
