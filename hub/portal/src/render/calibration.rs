use super::*;

// ---------------------------------------------------------------------
// The Overview tab's calibration card (plan/portal/calibration.cs.md §2).
//
// Contract: templates/partials/calibration_card.html's header comment.
// Shared by the Node and the Beacon detail pages and by the calibration
// routes, which answer an htmx form with the re-rendered card.
// ---------------------------------------------------------------------

/// One certificate, current or historical, in template shape.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationEntryCtx {
    pub id: String,
    /// RFC 3339 midnight of the expiry date (the first day the certificate
    /// is no longer valid) — rendered through `short_dt("date")`, so the
    /// language decides the spelling.
    pub valid_until: String,
    /// `YYYY-MM-DD`, the "Upravit datum" input's current value.
    pub valid_until_input: String,
    pub file_name: String,
    /// Human size, `"312 kB"`.
    pub file_size: String,
    pub uploaded_by: String,
    /// RFC 3339.
    pub uploaded_at: String,
    pub note: Option<String>,
    /// The download link, `{base}/calibrations/{id}.pdf`; `None` for a
    /// deleted certificate, whose file is gone.
    pub href: Option<String>,
    /// The delete form's target, `{base}/calibrations/{id}/delete`.
    pub delete_href: String,
    /// Set once an admin deleted it (amendment 2026-10-13).
    pub deleted: Option<CalibrationDeletionCtx>,
}

/// Who deleted a certificate, when and why.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationDeletionCtx {
    /// RFC 3339.
    pub at: String,
    pub by: String,
    pub message: String,
}

/// `partials/calibration_card.html`'s `cal`.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationCardCtx {
    /// `/nodes/{node_key}` or `/beacons/{mac}` — every form and link on the
    /// card hangs off it.
    pub base: String,
    /// The card's element id, every form's htmx target: `calibration-card`,
    /// or `calibration-card-{line}` for a Hub probe, whose eight cards share
    /// one page (amendment 2026-10-14).
    pub card_id: String,
    /// The newest certificate not deleted.
    pub current: Option<CalibrationEntryCtx>,
    /// The current certificate's [`crate::calibration::CalibrationLevel`],
    /// `None` without one.
    pub state: Option<&'static str>,
    /// Whole days until the expiry date, 1 on the last valid day; `None`
    /// without a certificate or from the expiry date on.
    pub days_left: Option<i64>,
    /// The i18n key of the "N days left" phrase, chosen here because Czech
    /// needs three plural forms and a template cannot pick one cleanly.
    pub days_left_key: Option<&'static str>,
    /// Every other certificate, deleted ones included, newest first; the
    /// card renders them collapsed.
    pub history: Vec<CalibrationEntryCtx>,
    pub can_edit: bool,
    /// Admin (or superuser): the delete forms are drawn.
    pub can_delete: bool,
    /// The device holds `MAX_PER_DEVICE` certificates not deleted: the form
    /// says so instead of taking an upload the route would refuse.
    pub limit_reached: bool,
    /// The limits the forms' hints and refusals name.
    pub max_count: i64,
    pub max_mb: usize,
    pub max_delete_chars: usize,
    /// Flash after a form submission.
    pub ok: Option<String>,
    pub error: Option<String>,
    /// The summary row's tone (UI amendment 2026-10-14): `valid`, `warning`
    /// (both warnings — one orange), `expired` or `none`.
    pub tone: &'static str,
    /// The i18n key of the summary row's state badge ("Platná · N dní",
    /// "Vyprší za N dní", "Prošlá", "Bez kalibrace"); `{n}` is
    /// `days_left`.
    pub summary_key: &'static str,
    /// The modal's fragment, `{base}/calibrations/card` (viewer+).
    pub card_url: String,
    /// What the card is about when one page holds several — a probe's
    /// "1 · Sensor 1"; `None` on a Node or Beacon page.
    pub title: Option<String>,
}

/// The summary badge's key for a level and its days left.
fn summary_key(
    level: Option<crate::calibration::CalibrationLevel>,
    days: Option<i64>,
) -> &'static str {
    use crate::calibration::CalibrationLevel as L;
    match (level, days) {
        (None, _) => "monitoring.calibration.state_none",
        (Some(L::Expired), _) | (Some(_), None) => "monitoring.calibration.state_expired",
        (Some(L::Valid), Some(_)) => "monitoring.calibration.summary_valid",
        (Some(_), Some(d)) => match d {
            ..=1 => "monitoring.calibration.summary_left_one",
            2..=4 => "monitoring.calibration.summary_left_few",
            _ => "monitoring.calibration.summary_left_many",
        },
    }
}

/// The summary row's tone: both warnings share one orange.
fn tone(level: Option<crate::calibration::CalibrationLevel>) -> &'static str {
    use crate::calibration::CalibrationLevel as L;
    match level {
        None => "none",
        Some(L::Valid) => "valid",
        Some(L::Warning | L::SecondWarning) => "warning",
        Some(L::Expired) => "expired",
    }
}

/// `"312 kB"` / `"2.4 MB"`. Decimal point in either language, the way the
/// battery label is written.
fn human_size(bytes: i64) -> String {
    const KIB: i64 = 1024;
    const MIB: i64 = 1024 * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{} kB", (bytes + KIB / 2) / KIB)
    } else {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    }
}

/// Czech counts days in three forms (1 den, 2–4 dny, 5+ dní); English
/// needs two, and its catalogue simply repeats the plural. Never called
/// with 0: the expiry date itself has no countdown, it is "Prošlá".
fn days_left_key(days: i64) -> &'static str {
    match days {
        ..=1 => "monitoring.calibration.left_one",
        2..=4 => "monitoring.calibration.left_few",
        _ => "monitoring.calibration.left_many",
    }
}

fn entry_ctx(base: &str, row: &portal_storage::DeviceCalibration) -> CalibrationEntryCtx {
    CalibrationEntryCtx {
        id: row.id.clone(),
        valid_until: crate::calibration::rfc3339(row.valid_until),
        valid_until_input: crate::calibration::format_date(row.valid_until),
        file_name: row.file_name.clone(),
        file_size: human_size(row.file_size),
        uploaded_by: row.uploaded_by.clone(),
        uploaded_at: crate::calibration::rfc3339(row.uploaded_at),
        note: row.note.clone(),
        href: (!row.is_deleted()).then(|| format!("{base}/calibrations/{}.pdf", row.id)),
        delete_href: format!("{base}/calibrations/{}/delete", row.id),
        deleted: row.deleted_at.map(|at| CalibrationDeletionCtx {
            at: crate::calibration::rfc3339(at),
            by: row.deleted_by.clone().unwrap_or_default(),
            message: row.delete_message.clone().unwrap_or_default(),
        }),
    }
}

/// The card from one device's rows (newest first, deleted ones included, as
/// `Storage::list_device_calibrations` returns them).
pub fn calibration_card_ctx(
    base: &str,
    rows: &[portal_storage::DeviceCalibration],
    cfg: &crate::config::CalibrationConfig,
    now: i64,
    can_edit: bool,
    can_delete: bool,
) -> CalibrationCardCtx {
    let current = crate::calibration::current_of(rows);
    let level = current.map(|c| crate::calibration::level(c.valid_until, now, cfg));
    let days_left = current
        .map(|c| crate::calibration::days_left(c.valid_until, now))
        .filter(|d| *d > 0);
    let standing = rows.iter().filter(|r| !r.is_deleted()).count() as i64;
    CalibrationCardCtx {
        base: base.to_string(),
        card_id: "calibration-card".to_string(),
        current: current.map(|c| entry_ctx(base, c)),
        state: level.map(|l| l.as_str()),
        days_left,
        days_left_key: days_left.map(days_left_key),
        history: rows
            .iter()
            .filter(|r| Some(r.id.as_str()) != current.map(|c| c.id.as_str()))
            .map(|r| entry_ctx(base, r))
            .collect(),
        can_edit,
        can_delete,
        limit_reached: standing >= crate::calibration::MAX_PER_DEVICE,
        max_count: crate::calibration::MAX_PER_DEVICE,
        max_mb: crate::calibration::MAX_PDF_BYTES / (1024 * 1024),
        max_delete_chars: crate::calibration::MAX_DELETE_MESSAGE_CHARS,
        ok: None,
        error: None,
        tone: tone(level),
        summary_key: summary_key(level, days_left),
        card_url: format!("{base}/calibrations/card"),
        title: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OCT_31: i64 = 1_793_404_800;
    const DAY: i64 = 86_400;

    fn row(id: &str, valid_until: i64) -> portal_storage::DeviceCalibration {
        portal_storage::DeviceCalibration {
            id: id.into(),
            hostname: "h1".into(),
            scope: "node".into(),
            entity: "aa11".into(),
            valid_until,
            file_name: "cert.pdf".into(),
            file_size: 320_000,
            sha256: "00".repeat(32),
            note: None,
            uploaded_by: "novak".into(),
            uploaded_at: OCT_31 - 300 * DAY,
            deleted_at: None,
            deleted_by: None,
            delete_message: None,
        }
    }

    #[test]
    fn the_card_names_the_current_certificate_and_keeps_the_rest_as_history() {
        let cfg = crate::config::CalibrationConfig::default();
        let rows = [row("new", OCT_31), row("old", OCT_31 - 365 * DAY)];
        let ctx = calibration_card_ctx("/nodes/aa11", &rows, &cfg, OCT_31 - 10 * DAY, true, false);

        let current = ctx.current.as_ref().expect("current");
        assert_eq!(current.id, "new");
        assert_eq!(
            current.href.as_deref(),
            Some("/nodes/aa11/calibrations/new.pdf")
        );
        assert_eq!(current.delete_href, "/nodes/aa11/calibrations/new/delete");
        assert!(current.deleted.is_none());
        assert_eq!(current.valid_until_input, "2026-10-31");
        assert_eq!(current.file_size, "313 kB");
        assert_eq!(ctx.state, Some("warning"));
        assert_eq!(ctx.days_left, Some(10));
        assert_eq!(ctx.days_left_key, Some("monitoring.calibration.left_many"));
        assert_eq!(ctx.history.len(), 1);
        assert_eq!(ctx.history[0].id, "old");
        assert!(!ctx.limit_reached);
        assert_eq!(ctx.max_mb, 10);
        assert_eq!(ctx.tone, "warning");
        assert_eq!(ctx.summary_key, "monitoring.calibration.summary_left_many");
        assert_eq!(ctx.card_url, "/nodes/aa11/calibrations/card");
    }

    /// UI amendment 2026-10-14: the summary row's tone and badge — one
    /// orange for both warnings, grey "Bez kalibrace" without a certificate.
    #[test]
    fn the_summary_row_has_one_orange_and_a_grey_none() {
        let cfg = crate::config::CalibrationConfig::default();
        let at = |days: i64| {
            calibration_card_ctx(
                "/x",
                &[row("c", OCT_31)],
                &cfg,
                OCT_31 - days * DAY,
                false,
                false,
            )
        };
        let valid = at(60);
        assert_eq!(
            (valid.tone, valid.summary_key),
            ("valid", "monitoring.calibration.summary_valid")
        );
        let second = at(3);
        assert_eq!(
            (second.tone, second.summary_key),
            ("warning", "monitoring.calibration.summary_left_few")
        );
        let one = at(1);
        assert_eq!(one.summary_key, "monitoring.calibration.summary_left_one");
        let expired = at(-2);
        assert_eq!(
            (expired.tone, expired.summary_key),
            ("expired", "monitoring.calibration.state_expired")
        );
        let none = calibration_card_ctx("/x", &[], &cfg, OCT_31, false, false);
        assert_eq!(
            (none.tone, none.summary_key),
            ("none", "monitoring.calibration.state_none")
        );
    }

    #[test]
    fn a_lapsed_certificate_has_no_countdown_and_no_certificate_has_no_state() {
        let cfg = crate::config::CalibrationConfig::default();
        let ctx = calibration_card_ctx(
            "/nodes/aa11",
            &[row("c", OCT_31)],
            &cfg,
            OCT_31,
            false,
            false,
        );
        assert_eq!(ctx.state, Some("expired"), "the expiry date itself");
        assert_eq!((ctx.days_left, ctx.days_left_key), (None, None));

        let last_day = calibration_card_ctx(
            "/nodes/aa11",
            &[row("c", OCT_31)],
            &cfg,
            OCT_31 - DAY,
            false,
            false,
        );
        assert_eq!(last_day.state, Some("second_warning"));
        assert_eq!(last_day.days_left, Some(1));

        let empty = calibration_card_ctx("/nodes/aa11", &[], &cfg, OCT_31, true, false);
        assert!(empty.current.is_none());
        assert_eq!(empty.state, None);
        assert!(empty.history.is_empty());
    }

    /// Amendment 2026-10-13: a deleted certificate is never current — the
    /// previous one standing is — and it stays in the history without a
    /// download link, saying who deleted it and why.
    #[test]
    fn a_deleted_certificate_is_history_without_a_link() {
        let cfg = crate::config::CalibrationConfig::default();
        let mut deleted = row("new", OCT_31 + 365 * DAY);
        deleted.deleted_at = Some(OCT_31 - DAY);
        deleted.deleted_by = Some("admin1".into());
        deleted.delete_message = Some("wrong device".into());
        let rows = [deleted, row("old", OCT_31)];
        let ctx = calibration_card_ctx("/nodes/aa11", &rows, &cfg, OCT_31 - 10 * DAY, true, true);

        assert_eq!(ctx.current.as_ref().expect("current").id, "old");
        assert_eq!(ctx.state, Some("warning"), "the previous one's level");
        assert_eq!(ctx.history.len(), 1);
        let h = &ctx.history[0];
        assert_eq!(h.id, "new");
        assert_eq!(h.href, None, "no download for a deleted certificate");
        let d = h.deleted.as_ref().expect("marked deleted");
        assert_eq!(
            (d.by.as_str(), d.message.as_str()),
            ("admin1", "wrong device")
        );
        assert!(ctx.can_delete);

        let all_gone = calibration_card_ctx("/nodes/aa11", &rows[..1], &cfg, OCT_31, true, true);
        assert!(all_gone.current.is_none());
        assert_eq!(all_gone.state, None, "no certificate standing, no state");
        assert_eq!(all_gone.history.len(), 1, "the record stays listed");
    }

    #[test]
    fn czech_gets_its_three_plural_forms() {
        assert_eq!(days_left_key(1), "monitoring.calibration.left_one");
        for n in 2..=4 {
            assert_eq!(days_left_key(n), "monitoring.calibration.left_few");
        }
        for n in [5, 11, 22, 30] {
            assert_eq!(days_left_key(n), "monitoring.calibration.left_many");
        }
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(3 * 1024 * 1024 + 400 * 1024), "3.4 MB");
    }
}
