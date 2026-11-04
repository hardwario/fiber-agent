//! `AlarmNotification`: everything the alarm engine's notify path carries,
//! rendered three ways — webhook JSON (`schema_version: 1`, v1 shape plus
//! additive keys), email subject (v1 subject/gate table), and the email body
//! ([`EmailBody`]: v1 template content as HTML with minimal inline CSS and no
//! logo CID — see mod docs — and the same content as the text/plain part).
//!
//! E-mails are written in the installation's `email_lang` (cs | en, V18) from
//! the `alarms.email.*` catalogue; the English catalogue keeps v1's sentences
//! word for word. Times are in the display zone (decisions.md #25). The
//! quantity and unit come from the alarm itself (`alarms::alarm_field`), so a
//! humidity alarm reads "%" and not the "C" every v1 e-mail assumed.

use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Which lifecycle event this notification describes.
#[derive(Debug, Clone, PartialEq)]
pub enum NotifyKind {
    Activated,
    /// An open alarm reached critical for the first time (decisions.md #26).
    Escalated,
    /// `ack` is `Some` when the alarm was acknowledged before it cleared —
    /// the cleared email then carries the ack attribution block.
    Cleared {
        ack: Option<AckInfo>,
    },
    Acknowledged {
        /// The acknowledger's username: the audit identity, and what the
        /// webhook's `acknowledgement.user` carries.
        user: String,
        /// How a reader sees them (`render::person_label`); the e-mail text
        /// and the webhook's additive `acknowledgement.name`.
        name: String,
        reason: Option<String>,
    },
    /// Synthetic test event (manual `POST /api/reports/*/test` endpoints).
    Test,
}

/// Ack attribution: who acknowledged, and their optional reason.
#[derive(Debug, Clone, PartialEq)]
pub struct AckInfo {
    /// Username, as on [`NotifyKind::Acknowledged`].
    pub user: String,
    /// Display label, as on [`NotifyKind::Acknowledged`].
    pub name: String,
    pub reason: Option<String>,
}

/// v1 alarm `source` values (ws.md): `fiber` (wired), `sticker` (LoRaWAN),
/// `eye`, `system` (power/wifi/ethernet disconnects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmSource {
    Fiber,
    Sticker,
    Eye,
    System,
}

impl AlarmSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AlarmSource::Fiber => "fiber",
            AlarmSource::Sticker => "sticker",
            AlarmSource::Eye => "eye",
            AlarmSource::System => "system",
        }
    }
}

/// Per-recipient-config gates from the v1 subject/gate table. Defaults:
/// activation on, clear and ack off. Test sends bypass gates (manual
/// endpoints).
#[derive(Debug, Clone)]
pub struct NotifyGates {
    pub notify_on_alarm: bool,
    pub notify_on_clear: bool,
    pub notify_on_ack: bool,
}

impl Default for NotifyGates {
    fn default() -> Self {
        Self {
            notify_on_alarm: true,
            notify_on_clear: false,
            notify_on_ack: false,
        }
    }
}

impl NotifyGates {
    /// Whether these gates allow sending an email for `kind`.
    pub fn allows(&self, kind: &NotifyKind) -> bool {
        match kind {
            // An escalation is the same alarm getting worse: whoever wants
            // to hear about alarms wants to hear this.
            NotifyKind::Activated | NotifyKind::Escalated => self.notify_on_alarm,
            NotifyKind::Cleared { .. } => self.notify_on_clear,
            NotifyKind::Acknowledged { .. } => self.notify_on_ack,
            NotifyKind::Test => true,
        }
    }
}

/// One alarm notification, fully denormalized — the engine resolves
/// device/sensor identity before handing it here so rendering is pure.
#[derive(Debug, Clone)]
pub struct AlarmNotification {
    pub kind: NotifyKind,
    /// e.g. `"warning"` / `"critical"` — uppercased in subjects.
    pub severity: String,
    /// e.g. `"temperature_high"` — uppercased in subjects.
    pub alarm_type: String,
    pub source: AlarmSource,
    /// `None` for an alarm about a device rather than a reading; the e-mail
    /// prints "–" rather than the 0.0 v2 used to invent.
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub message: String,
    pub hostname: String,
    /// Operator label; subjects and the webhook `device.label` fall back
    /// to `hostname` when unset.
    pub device_label: Option<String>,
    /// Node identity on both transports — the frozen v1 webhook field, and since
    /// GitLab #73 the P2P wire carries a DevEUI too, so a receiver keying its
    /// registry on `dev_eui` sees one device whichever radio it speaks. `None`
    /// for every non-Node source.
    pub dev_eui: Option<String>,
    /// The Node's printed identity, alongside `dev_eui` and never instead of
    /// it. Additive to the frozen `schema_version: 1` shape: v1 never emitted
    /// it, so no v1 consumer can be reading this key.
    pub serial_number: Option<String>,
    pub mac: Option<String>,
    pub sensor_name: String,
    pub sensor_id: String,
    /// Sensor line; `None` **or** the v1 `-1` sentinel render as JSON null.
    pub line: Option<i32>,
    pub location: Option<String>,
    pub triggered_at: OffsetDateTime,
    /// `alarm_events.id` — a uuid v4 string in v2 (M6-B), not v1's integer
    /// row id; the webhook `alarm.id` field carries it as a JSON string
    /// (M6-E deviation from the literal v1 schema, which had an int here).
    pub alarm_id: String,
    /// `cs` | `en` — the catalogue the e-mail is written from.
    pub lang: String,
    /// Registry field name (`common.field.*` labels it) and unit of what the
    /// alarm measures; `None` for a device alarm.
    pub quantity: Option<String>,
    pub unit: Option<String>,
    /// The Node's or Beacon's own name, when the snapshot knows it. A field
    /// alarm's `sensor_name` is the field's label.
    pub device_name: Option<String>,
}

/// One catalogue string in `lang`.
fn tr(lang: &str, key: &str) -> String {
    crate::i18n::lookup(lang, key)
}

/// A catalogue string with `{name}` placeholders filled.
pub(crate) fn tr_with(lang: &str, key: &str, params: &[(&str, &str)]) -> String {
    let mut out = tr(lang, key);
    for (name, value) in params {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// A time as the e-mail shows it: the portal's own format, in the display
/// zone, naming the zone.
pub(crate) fn email_time(t: OffsetDateTime, lang: &str) -> String {
    crate::render::display_dt(t, lang, "full", &crate::display_tz::current())
}

impl AlarmNotification {
    /// The webhook event name for this kind. Escalation, clear and ack have
    /// one only when the installation opted in (`webhook_lifecycle`, V18,
    /// decisions.md #29): a receiver written against v1 only ever saw
    /// `alarm.activated`, and one that does not read `event` would take an
    /// escalation or a clear for a new alarm.
    pub fn webhook_event(&self, lifecycle: bool) -> Option<&'static str> {
        match self.kind {
            NotifyKind::Activated => Some("alarm.activated"),
            NotifyKind::Escalated => lifecycle.then_some("alarm.escalated"),
            NotifyKind::Test => Some("alarm.test"),
            NotifyKind::Cleared { .. } => lifecycle.then_some("alarm.cleared"),
            NotifyKind::Acknowledged { .. } => lifecycle.then_some("alarm.acknowledged"),
        }
    }

    /// The `schema_version: 1` webhook payload (v1 shape plus the additive
    /// `device.serial_number`, `sensor.quantity`/`sensor.unit`, `device.name`
    /// and, for clear/ack, `acknowledgement` keys — additive, so no v1
    /// consumer can be reading them), or `None` when this kind has no event.
    pub fn webhook_payload(&self, lifecycle: bool) -> Option<Value> {
        let event = self.webhook_event(lifecycle)?;
        let acknowledgement = match &self.kind {
            NotifyKind::Acknowledged { user, name, reason } => {
                json!({ "user": user, "name": name, "reason": reason })
            }
            NotifyKind::Cleared { ack: Some(ack) } => {
                json!({ "user": ack.user, "name": ack.name, "reason": ack.reason })
            }
            _ => Value::Null,
        };
        // Line: null for the -1 device-wide sentinel (and for unknown).
        let line = match self.line {
            Some(l) if l >= 0 => json!(l),
            _ => Value::Null,
        };
        Some(json!({
            "schema_version": 1,
            "event": event,
            "emitted_at": iso_utc(OffsetDateTime::now_utc()),
            "alarm": {
                "id": self.alarm_id,
                "severity": self.severity,
                "type": self.alarm_type,
                "source": self.source.as_str(),
                "value": self.value,
                "threshold": self.threshold,
                "message": self.message,
                "triggered_at": iso_utc(self.triggered_at),
            },
            "device": {
                "hostname": self.hostname,
                "label": self.device_label.clone().unwrap_or_else(|| self.hostname.clone()),
                "dev_eui": self.dev_eui,
                "serial_number": self.serial_number,
                "mac": self.mac,
                "name": self.device_name,
            },
            "sensor": {
                "name": self.sensor_name,
                "id": self.sensor_id,
                "line": line,
                // Not nullable in the v1 schema — empty string when unknown.
                "location": self.location.clone().unwrap_or_default(),
                "quantity": self.quantity,
                "unit": self.unit,
            },
            "acknowledgement": acknowledgement,
        }))
    }

    /// Email subject per the v1 table, in `lang`:
    /// `[SEVERITY] {label or hostname} - {sensor} - {TYPE}` / `[CLEARED] …`
    /// / `[ACK by {user}] …` / the fixed test subject; an escalation is the
    /// activated subject at its new level with "(escalated)" after it.
    pub fn email_subject(&self) -> String {
        let lang = self.lang.as_str();
        let device = self.device_label.as_deref().unwrap_or(&self.hostname);
        let tail = format!(
            "{} - {} - {}",
            device,
            self.sensor_label(),
            self.type_label(true)
        );
        let severity = self.severity_word().to_uppercase();
        let with = |key: &str, extra: &[(&str, &str)]| {
            let mut params = vec![("tail", tail.as_str()), ("severity", severity.as_str())];
            params.extend_from_slice(extra);
            tr_with(lang, key, &params)
        };
        match &self.kind {
            NotifyKind::Activated => with("alarms.email.subject.activated", &[]),
            NotifyKind::Escalated => with("alarms.email.subject.escalated", &[]),
            NotifyKind::Cleared { .. } => with("alarms.email.subject.cleared", &[]),
            NotifyKind::Acknowledged { name, .. } => with(
                "alarms.email.subject.acknowledged",
                &[("user", name.as_str())],
            ),
            NotifyKind::Test => tr(lang, "alarms.email.subject.test"),
        }
    }

    /// The severity in words: `critical` / `kritické`.
    fn severity_word(&self) -> String {
        let key = format!("alarms.severity.{}", self.severity.to_ascii_lowercase());
        let word = tr(&self.lang, &key);
        if word == key {
            self.severity.clone()
        } else {
            word
        }
    }

    /// The alarm type in words. English keeps v1's raw spelling —
    /// uppercase in the subject (`TEMPERATURE_HIGH`, mail filters match on
    /// it), as stored in the body; Czech uses the UI's label ("Vysoká
    /// hodnota") in both.
    fn type_label(&self, subject: bool) -> String {
        if self.lang == "en" {
            return if subject {
                self.alarm_type.to_uppercase()
            } else {
                self.alarm_type.clone()
            };
        }
        let key = format!("alarms.type.{}", self.alarm_type.to_ascii_lowercase());
        let label = tr(&self.lang, &key);
        if label == key {
            self.alarm_type.replace('_', " ")
        } else {
            label
        }
    }

    /// What the alarm is on: "Sticker 1 · Vlhkost" for a Node or Beacon
    /// field, the probe's own name for a wired line, and for a Hub alarm
    /// (`sys:*`) the Hub, as the alarm list names it (`ui.alarm_name`), not
    /// the internal key. The webhook keeps the raw `sensor.id`/`sensor.name`.
    fn sensor_label(&self) -> String {
        match (&self.device_name, &self.quantity) {
            (Some(device), Some(q)) => {
                // A field without its own catalogue name (a 1-Wire slot's) is
                // drawn from the stored label, as the alarm list draws it (#122).
                format!(
                    "{device} \u{00b7} {}",
                    crate::i18n::field_label(&self.lang, q, &self.sensor_name)
                )
            }
            (Some(device), None) => device.clone(),
            _ if self.sensor_id.starts_with("sys:") => tr(&self.lang, "alarms.table.hub_system"),
            _ => self.sensor_name.clone(),
        }
    }

    /// A reading with its unit as the portal shows it (a Node's Pa in hPa,
    /// UX-038), or "–". The webhook keeps the stored value and unit.
    fn reading(&self, v: Option<f64>) -> String {
        let def = self
            .quantity
            .as_deref()
            .and_then(|q| crate::alarms::display_def(q, self.unit.as_deref()));
        let unit = def.map_or(self.unit.as_deref(), |f| f.display_unit());
        match (v.map(|v| def.map_or(v, |f| f.to_display(v))), unit) {
            (Some(v), Some(u)) => format!("{v:.1} {u}"),
            (Some(v), None) => format!("{v:.1}"),
            (None, _) => "\u{2013}".to_string(),
        }
    }

    /// The e-mail body: severity-colored header, Device / Sensor / Location
    /// (omitted when unknown) / Type / the quantity / Threshold / Triggered
    /// rows, the link to the alarm when the installation has a `public_url`
    /// (UX-027), then the kind-specific closing blocks with the v1-mandated
    /// sentences. No inline logo (v2 simplification, see mod docs).
    pub fn email_body(&self, public_url: Option<&str>) -> EmailBody {
        let lang = self.lang.as_str();
        let t = |key: &str| tr(lang, key);
        if matches!(self.kind, NotifyKind::Test) {
            // The test mail proves the address works, or says it is missing.
            let (link, missing) = match public_url {
                Some(base) => (
                    Some((t("alarms.email.link.portal"), format!("{base}/"))),
                    None,
                ),
                None => (None, Some(Block::Para(t("alarms.email.link.missing")))),
            };
            return EmailBody {
                color: HEADER_TEST,
                title: t("alarms.email.title.test"),
                rows: Vec::new(),
                link,
                blocks: std::iter::once(Block::Para(t("alarms.email.test_body")))
                    .chain(missing)
                    .collect(),
            };
        }

        let (color, title) = match &self.kind {
            NotifyKind::Activated => (
                severity_color(&self.severity),
                tr_with(
                    lang,
                    "alarms.email.title.activated",
                    &[
                        ("SEVERITY", &self.severity.to_uppercase()),
                        ("severity", &self.severity_word()),
                    ],
                ),
            ),
            NotifyKind::Escalated => (HEADER_CRITICAL, t("alarms.email.title.escalated")),
            NotifyKind::Cleared { .. } => (HEADER_CLEARED, t("alarms.email.title.cleared")),
            NotifyKind::Acknowledged { .. } => (HEADER_ACK, t("alarms.email.title.acknowledged")),
            NotifyKind::Test => unreachable!("handled above"),
        };

        let device = self.device_label.as_deref().unwrap_or(&self.hostname);
        let quantity_label = match &self.quantity {
            Some(q) => tr(lang, &format!("common.field.{q}")),
            None => t("alarms.email.row.value"),
        };
        let mut rows = vec![
            (t("alarms.email.row.device"), device.to_string()),
            (t("alarms.email.row.sensor"), self.sensor_label()),
        ];
        if let Some(location) = self.location.as_deref().filter(|l| !l.is_empty()) {
            rows.push((t("alarms.email.row.location"), location.to_string()));
        }
        rows.push((t("alarms.email.row.type"), self.type_label(false)));
        if self.value.is_some() || self.quantity.is_some() {
            rows.push((quantity_label, self.reading(self.value)));
            rows.push((
                t("alarms.email.row.threshold"),
                self.reading(self.threshold),
            ));
        }
        rows.push((
            t("alarms.email.row.triggered"),
            email_time(self.triggered_at, lang),
        ));

        let ack_block = |user: &str, reason: Option<&str>| Block::Strong {
            head: tr_with(lang, "alarms.email.acknowledged_by", &[("user", user)]),
            tail: reason.map(|r| tr_with(lang, "alarms.email.reason", &[("reason", r)])),
        };
        let mut blocks = Vec::new();
        match &self.kind {
            NotifyKind::Activated | NotifyKind::Escalated => {
                if matches!(self.kind, NotifyKind::Escalated) {
                    blocks.push(Block::Para(t("alarms.email.escalated_body")));
                }
                if !self.message.is_empty() {
                    blocks.push(Block::Para(self.message.clone()));
                }
                // Type-specific recovery steps. Only CONCENTRATOR_MISSING has
                // one today: a card the operator must recover physically, not
                // something that self-heals.
                blocks.extend(recovery_hint(&self.alarm_type, lang));
            }
            NotifyKind::Cleared { ack } => {
                // A cleared row carries a message only when the clear says why
                // it happened: a wired alarm closed from the Hub's reported
                // state (decisions.md #27), which reads in the e-mail's language.
                if portal_domain::alarm_engine::is_wired_reconcile(&self.message) {
                    blocks.push(Block::Para(t("alarms.email.reconciled")));
                } else if !self.message.is_empty() {
                    blocks.push(Block::Para(self.message.clone()));
                }
                blocks.push(match ack {
                    Some(ack) => ack_block(&ack.name, ack.reason.as_deref()),
                    None => Block::Para(t("alarms.email.cleared_unacked")),
                });
            }
            NotifyKind::Acknowledged { name, reason, .. } => {
                blocks.push(ack_block(name, reason.as_deref()));
                blocks.push(Block::Para(t("alarms.email.ack_note")));
            }
            NotifyKind::Test => unreachable!("handled above"),
        }

        EmailBody {
            color,
            title,
            rows,
            link: public_url.map(|base| {
                (
                    t("alarms.email.link.alarm"),
                    format!(
                        "{base}/alarms/{}",
                        crate::routes::percent_encode_query(&self.alarm_id)
                    ),
                )
            }),
            blocks,
        }
    }
}

/// One e-mail's content, rendered into both MIME parts so the HTML and the
/// text/plain part cannot drift (UX-027, plan/portal/email_links.md §4).
/// Every string is plain text: [`Self::to_html`] escapes, [`Self::to_text`]
/// does not.
#[derive(Debug, Clone)]
pub struct EmailBody {
    /// The header block's background (HTML only).
    pub color: &'static str,
    pub title: String,
    /// `(label, value)` detail rows.
    pub rows: Vec<(String, String)>,
    /// `(label, url)` into the portal, only when `public_url` is set.
    pub link: Option<(String, String)>,
    /// What follows the rows and the link.
    pub blocks: Vec<Block>,
}

/// A closing block of an [`EmailBody`].
#[derive(Debug, Clone)]
pub enum Block {
    Para(String),
    /// A bold line with an optional second line: the ack attribution.
    Strong {
        head: String,
        tail: Option<String>,
    },
    /// A titled numbered list and a closing note: recovery steps.
    Steps {
        title: String,
        items: Vec<String>,
        note: String,
    },
}

impl EmailBody {
    /// The HTML part: v1's template content, minimal inline CSS, every value
    /// escaped.
    pub fn to_html(&self) -> String {
        format!("<div style=\"{WRAP}\">{}</div>", self.card_html(&FOOTER))
    }

    /// The header and the body of [`Self::to_html`] without the outer wrapper, with `tail`
    /// (the footer, or nothing for a card of a batched e-mail) closing the body.
    fn card_html(&self, tail: &str) -> String {
        let mut rows = String::new();
        for (label, value) in &self.rows {
            push_row(&mut rows, &escape(label), &escape(value));
        }
        let table = if rows.is_empty() {
            String::new()
        } else {
            format!("<table style=\"border-collapse:collapse\">{rows}</table>")
        };
        let link = self
            .link
            .as_ref()
            .map(|(label, url)| {
                format!(
                    "<p style=\"margin-top:16px\"><a href=\"{}\" style=\"{LINK}\">{}</a></p>",
                    escape(url),
                    escape(label)
                )
            })
            .unwrap_or_default();
        let mut blocks = String::new();
        for block in &self.blocks {
            match block {
                Block::Para(text) => blocks.push_str(&format!(
                    "<p style=\"margin-top:16px\">{}</p>",
                    escape(text)
                )),
                Block::Strong { head, tail } => blocks.push_str(&format!(
                    "<p style=\"margin-top:16px\"><strong>{}</strong>{}</p>",
                    escape(head),
                    tail.as_deref()
                        .map(|t| format!("<br>{}", escape(t)))
                        .unwrap_or_default(),
                )),
                Block::Steps { title, items, note } => {
                    let items: String = items
                        .iter()
                        .map(|i| format!("<li>{}</li>", escape(i)))
                        .collect();
                    let note = if note.is_empty() {
                        String::new()
                    } else {
                        format!("<p style=\"margin-top:8px\">{}</p>", escape(note))
                    };
                    blocks.push_str(&format!(
                        "<div style=\"margin-top:16px\">\
                         <strong>{}</strong>\
                         <ol style=\"margin:8px 0 0 0;padding-left:20px\">{items}</ol>\
                         {note}</div>",
                        escape(title),
                    ));
                }
            }
        }
        format!(
            "<div style=\"background:{};{HEADER}\">{}</div>\
             <div style=\"{BODY}\">{table}{link}{blocks}{tail}</div>",
            self.color,
            escape(&self.title)
        )
    }

    /// The text/plain part: the same content, unescaped and uncoloured.
    pub fn to_text(&self) -> String {
        let mut out = self.card_text();
        out.push_str(&SIGNATURE);
        out
    }

    /// [`Self::to_text`] without the signature: one card of a batched e-mail.
    fn card_text(&self) -> String {
        let mut out = format!("{}\n", self.title);
        for (label, value) in &self.rows {
            out.push_str(&format!("{label}: {value}\n"));
        }
        if let Some((label, url)) = &self.link {
            out.push_str(&format!("{label}: {url}\n"));
        }
        for block in &self.blocks {
            out.push('\n');
            match block {
                Block::Para(text) => out.push_str(&format!("{text}\n")),
                Block::Strong { head, tail } => {
                    out.push_str(&format!("{head}\n"));
                    if let Some(tail) = tail {
                        out.push_str(&format!("{tail}\n"));
                    }
                }
                Block::Steps { title, items, note } => {
                    out.push_str(&format!("{title}\n"));
                    for (n, item) in items.iter().enumerate() {
                        out.push_str(&format!("{}. {item}\n", n + 1));
                    }
                    if !note.is_empty() {
                        out.push_str(&format!("{note}\n"));
                    }
                }
            }
        }
        out
    }
}

/// The text/plain part's closing signature: the brand's `monitor` name.
static SIGNATURE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("\n-- \n{}\n", proximos_brand::MONITOR));

/// Several alarm changes of one Hub as one e-mail (decision #19, notification-delivery.md
/// §6.1): `(subject, text, html)`. Each item carries how many changes of the same alarm the
/// per-hour limit left out since its last e-mail (§6.2). One item with none left out is exactly
/// the single e-mail.
pub fn batch_email(
    items: &[(AlarmNotification, usize)],
    limit: usize,
    public_url: Option<&str>,
) -> (String, String, String) {
    let Some((first, _)) = items.first() else {
        return (String::new(), String::new(), String::new());
    };
    let lang = first.lang.as_str();
    let limit = limit.to_string();
    let body = |n: &AlarmNotification, left_out: usize| {
        let mut body = n.email_body(public_url);
        if left_out > 0 {
            body.blocks.push(Block::Para(tr_with(
                lang,
                "alarms.email.batch.left_out",
                &[("n", &left_out.to_string()), ("limit", &limit)],
            )));
        }
        body
    };
    if let [(single, left_out)] = items {
        let body = body(single, *left_out);
        return (single.email_subject(), body.to_text(), body.to_html());
    }
    let device = first.device_label.as_deref().unwrap_or(&first.hostname);
    let count = items.len().to_string();
    let params = [("device", device), ("n", count.as_str())];
    let subject = tr_with(lang, "alarms.email.subject.batch", &params);
    let intro = tr_with(lang, "alarms.email.batch.intro", &params);
    let mut html = format!("<div style=\"{WRAP}\"><p>{}</p>", escape(&intro));
    let mut text = format!("{intro}\n");
    for (n, left_out) in items {
        let body = body(n, *left_out);
        html.push_str(&format!(
            "<div style=\"margin-bottom:16px\">{}</div>",
            body.card_html("")
        ));
        text.push('\n');
        text.push_str(&body.card_text());
    }
    html.push_str(&FOOTER);
    html.push_str("</div>");
    text.push_str(&SIGNATURE);
    (subject, text, html)
}

// --- shared template pieces (also used by checkers.rs system alerts) ---

pub(crate) const WRAP: &str =
    "font-family:Arial,Helvetica,sans-serif;max-width:600px;color:#222222";
pub(crate) const HEADER: &str = "color:#ffffff;padding:14px 20px;font-size:18px;font-weight:bold;\
     border-radius:4px 4px 0 0";
pub(crate) const BODY: &str = "border:1px solid #dddddd;border-top:none;padding:20px;\
     border-radius:0 0 4px 4px";
pub(crate) static FOOTER: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "<p style=\"color:#888888;font-size:12px;margin-bottom:0\">{}</p>",
        proximos_brand::MONITOR
    )
});

/// The button-styled link into the portal (UX-027).
pub(crate) const LINK: &str = "display:inline-block;background:#1565c0;color:#ffffff;\
     padding:10px 16px;border-radius:4px;text-decoration:none;font-weight:bold";

pub(crate) const HEADER_CRITICAL: &str = "#c62828";
pub(crate) const HEADER_WARNING: &str = "#ef6c00";
pub(crate) const HEADER_INFO: &str = "#1565c0";
pub(crate) const HEADER_CLEARED: &str = "#2e7d32";
pub(crate) const HEADER_ACK: &str = "#1565c0";
pub(crate) const HEADER_TEST: &str = "#1565c0";

/// Type-specific recovery guidance appended to an activated-alarm email, or
/// `None`. CONCENTRATOR_MISSING needs a cold power cycle — a `reboot` does not
/// drop the mPCIe power rail, so it cannot free a latched/undervolted card
/// (issue #39); the three steps spell that out for the operator. The radio
/// diagnostics pair (NORTHBRIDGE_UNREACHABLE §4, CHIRPSTACK_DOWN §4a) and
/// CONFIG_NOT_DELIVERED (PF-3: the Hub already holds what the Node did not
/// confirm) are also things an operator acts on, not ones that self-heal.
fn recovery_hint(alarm_type: &str, lang: &str) -> Option<Block> {
    let (title, stem, steps, note) = match alarm_type {
        "CONCENTRATOR_MISSING" => ("recovery.title", "concentrator", 3, true),
        "NORTHBRIDGE_UNREACHABLE" => ("recovery.title", "northbridge", 3, true),
        "CHIRPSTACK_DOWN" => ("recovery.title", "chirpstack", 2, true),
        "NORTHBRIDGE_UPDATE_FAILED" => ("recovery.title", "northbridge_update", 2, true),
        "SOUTHBRIDGE_UPDATE_FAILED" => ("recovery.title", "southbridge_update", 2, true),
        "CONFIG_NOT_DELIVERED" => ("recovery.config_title", "config", 3, false),
        _ => return None,
    };
    let t = |key: &str| tr(lang, &format!("alarms.email.{key}"));
    Some(Block::Steps {
        title: t(title),
        items: (1..=steps)
            .map(|n| t(&format!("recovery.{stem}_{n}")))
            .collect(),
        note: if note {
            t(&format!("recovery.{stem}_note"))
        } else {
            String::new()
        },
    })
}

/// Header block color for a severity string (unknown severities get the
/// info blue rather than failing).
pub(crate) fn severity_color(severity: &str) -> &'static str {
    match severity.to_ascii_lowercase().as_str() {
        "critical" => HEADER_CRITICAL,
        "warning" => HEADER_WARNING,
        _ => HEADER_INFO,
    }
}

/// One `<tr>` of the details table.
pub(crate) fn push_row(rows: &mut String, label: &str, value: &str) {
    use std::fmt::Write as _;
    // Writing to a String is infallible.
    let _ = write!(
        rows,
        "<tr><td style=\"padding:4px 16px 4px 0;color:#666666\">{label}</td>\
         <td style=\"padding:4px 0\">{value}</td></tr>"
    );
}

/// Minimal HTML escaping for interpolated values.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// RFC3339 UTC (`…Z`) — webhook `emitted_at` / `triggered_at`. Formatting
/// a valid `OffsetDateTime` only fails for out-of-range years; degrade to
/// an empty string rather than panicking.
pub(crate) fn iso_utc(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // time 0.3.44 without the "macros" feature: build timestamps by hand.
    fn utc(y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> OffsetDateTime {
        let month = time::Month::try_from(mo).expect("month");
        let date = time::Date::from_calendar_date(y, month, d).expect("date");
        let tod = time::Time::from_hms(h, mi, s).expect("time");
        time::PrimitiveDateTime::new(date, tod).assume_utc()
    }

    fn base(kind: NotifyKind) -> AlarmNotification {
        AlarmNotification {
            kind,
            severity: "critical".into(),
            alarm_type: "temperature_high".into(),
            source: AlarmSource::Fiber,
            value: Some(42.63),
            threshold: Some(40.0),
            message: "Temperature 42.6 C exceeds threshold 40.0 C".into(),
            hostname: "fiber-01".into(),
            device_label: Some("Cold Store A".into()),
            dev_eui: None,
            serial_number: None,
            mac: None,
            sensor_name: "Sensor 3".into(),
            sensor_id: "fiber-01:3".into(),
            line: Some(3),
            location: Some("north wall".into()),
            triggered_at: utc(2026, 8, 18, 9, 30, 0),
            alarm_id: "17".into(),
            lang: "en".into(),
            quantity: Some("temperature".into()),
            unit: Some("\u{00b0}C".into()),
            device_name: None,
        }
    }

    #[test]
    fn webhook_payload_golden_shape() {
        let n = base(NotifyKind::Activated);
        let got = n
            .webhook_payload(false)
            .expect("activated has a webhook event");

        // emitted_at is "now" — check it parses as RFC3339 UTC, then splice
        // it into the golden value for the exact field-for-field compare.
        let emitted_at = got["emitted_at"].as_str().expect("emitted_at string");
        assert!(emitted_at.ends_with('Z'), "ISO UTC: {emitted_at}");
        OffsetDateTime::parse(emitted_at, &Rfc3339).expect("parseable RFC3339");

        let expected = serde_json::json!({
            "schema_version": 1,
            "event": "alarm.activated",
            "emitted_at": emitted_at,
            "alarm": {
                "id": "17",
                "severity": "critical",
                "type": "temperature_high",
                "source": "fiber",
                "value": 42.63,
                "threshold": 40.0,
                "message": "Temperature 42.6 C exceeds threshold 40.0 C",
                "triggered_at": "2026-08-18T09:30:00Z",
            },
            "device": {
                "hostname": "fiber-01",
                "label": "Cold Store A",
                "dev_eui": null,
                "serial_number": null,
                "mac": null,
                "name": null,
            },
            "sensor": {
                "name": "Sensor 3",
                "id": "fiber-01:3",
                "line": 3,
                "location": "north wall",
                "quantity": "temperature",
                "unit": "\u{00b0}C",
            },
            "acknowledgement": null,
        });
        assert_eq!(
            got, expected,
            "schema_version 1 shape: ws.md plus the additive keys"
        );
    }

    #[test]
    fn webhook_events_only_activated_and_test() {
        assert_eq!(
            base(NotifyKind::Activated).webhook_event(false),
            Some("alarm.activated")
        );
        assert_eq!(
            base(NotifyKind::Test).webhook_event(false),
            Some("alarm.test")
        );
        assert!(base(NotifyKind::Cleared { ack: None })
            .webhook_payload(false)
            .is_none());
        assert!(base(NotifyKind::Acknowledged {
            user: "alice".into(),
            name: "alice".into(),
            reason: None
        })
        .webhook_payload(false)
        .is_none());

        let test = base(NotifyKind::Test)
            .webhook_payload(false)
            .expect("test event");
        assert_eq!(test["event"], "alarm.test");
        assert_eq!(test["schema_version"], 1);
    }

    #[test]
    fn webhook_nullables_and_line_sentinel() {
        let mut n = base(NotifyKind::Activated);
        n.device_label = None;
        n.dev_eui = Some("0011223344556677".into());
        n.mac = None;
        n.line = Some(-1); // v1 device-wide sentinel → null
        n.location = None;

        let p = n.webhook_payload(false).expect("payload");
        assert_eq!(
            p["device"]["label"], "fiber-01",
            "label falls back to hostname"
        );
        assert_eq!(p["device"]["dev_eui"], "0011223344556677");
        assert_eq!(p["device"]["serial_number"], Value::Null);
        assert_eq!(p["device"]["mac"], Value::Null);
        assert_eq!(p["sensor"]["line"], Value::Null);
        assert_eq!(
            p["sensor"]["location"], "",
            "location is not nullable in schema 1"
        );

        let mut n = base(NotifyKind::Activated);
        n.line = None;
        let p = n.webhook_payload(false).expect("payload");
        assert_eq!(p["sensor"]["line"], Value::Null);
    }

    #[test]
    fn subject_table() {
        assert_eq!(
            base(NotifyKind::Activated).email_subject(),
            "[CRITICAL] Cold Store A - Sensor 3 - TEMPERATURE_HIGH"
        );
        assert_eq!(
            base(NotifyKind::Cleared { ack: None }).email_subject(),
            "[CLEARED] Cold Store A - Sensor 3 - TEMPERATURE_HIGH"
        );
        assert_eq!(
            base(NotifyKind::Acknowledged {
                user: "alice".into(),
                name: "alice".into(),
                reason: None
            })
            .email_subject(),
            "[ACK by alice] Cold Store A - Sensor 3 - TEMPERATURE_HIGH"
        );
        assert_eq!(
            base(NotifyKind::Test).email_subject(),
            format!("{} - Test Email", proximos_brand::MONITOR)
        );

        // No label → hostname in the subject.
        let mut n = base(NotifyKind::Activated);
        n.device_label = None;
        assert_eq!(
            n.email_subject(),
            "[CRITICAL] fiber-01 - Sensor 3 - TEMPERATURE_HIGH"
        );
    }

    #[test]
    fn gate_table_defaults() {
        let gates = NotifyGates::default();
        assert!(
            gates.allows(&NotifyKind::Activated),
            "notify_on_alarm default on"
        );
        assert!(
            !gates.allows(&NotifyKind::Cleared { ack: None }),
            "clear default off"
        );
        assert!(
            !gates.allows(&NotifyKind::Acknowledged {
                user: "a".into(),
                name: "a".into(),
                reason: None
            }),
            "ack default off"
        );
        assert!(gates.allows(&NotifyKind::Test), "test bypasses gates");
    }

    #[test]
    fn activated_html_rows_and_formats() {
        let html = base(NotifyKind::Activated).email_body(None).to_html();
        for needle in [
            ">Device<",
            ">Cold Store A<",
            ">Sensor<",
            ">Sensor 3<",
            ">Location<",
            ">north wall<",
            ">Type<",
            ">temperature_high<",
            ">Temperature<",
            ">42.6 \u{00b0}C<", // %.1f formatting of 42.63, the alarm's own unit
            ">Threshold<",
            ">40.0 \u{00b0}C<",
            ">Triggered<",
            ">Aug 18, 2026, 09:30:00 UTC<", // test processes display in UTC
        ] {
            assert!(html.contains(needle), "missing {needle} in:\n{html}");
        }
        assert!(html.contains(HEADER_CRITICAL), "severity-colored header");
    }

    #[test]
    fn activated_concentrator_missing_email_carries_recovery_steps() {
        let mut n = base(NotifyKind::Activated);
        n.alarm_type = "CONCENTRATOR_MISSING".into();
        let html = n.email_body(None).to_html();
        for needle in [
            "Recovery",
            "Power the Hub off",
            "Disconnect power and wait",
            "Reconnect power",
            "reseat the concentrator",
        ] {
            assert!(html.contains(needle), "missing {needle} in:\n{html}");
        }
        // A different alarm type must not carry the concentrator recovery block.
        assert!(
            !base(NotifyKind::Activated)
                .email_body(None)
                .to_html()
                .contains("Power the Hub off"),
            "recovery block leaked onto a non-concentrator alarm"
        );
    }

    /// The radio alarms and an undelivered Node configuration carry their own steps, in the
    /// e-mail's language (radio diagnostics §4, §4a; PF-3).
    #[test]
    fn radio_and_config_alarms_carry_their_steps() {
        for (alarm_type, needle) in [
            ("NORTHBRIDGE_UNREACHABLE", "Northbridge row"),
            ("CHIRPSTACK_DOWN", "LoRaWAN rows"),
            ("NORTHBRIDGE_UPDATE_FAILED", "Northbridge firmware card"),
            ("SOUTHBRIDGE_UPDATE_FAILED", "Southbridge firmware"),
            ("CONFIG_NOT_DELIVERED", "Send again"),
        ] {
            let mut n = base(NotifyKind::Activated);
            n.alarm_type = alarm_type.into();
            let body = n.email_body(None);
            assert!(body.to_html().contains(needle), "{alarm_type}");
            assert!(body.to_text().contains(needle), "{alarm_type}");
            n.lang = "cs".into();
            assert!(
                !n.email_body(None).to_html().contains(needle),
                "{alarm_type} in Czech"
            );
        }
    }

    /// Decision #19 §6.1: the changes of one Hub within the window are one e-mail naming
    /// their number; one change alone is the single e-mail of before.
    #[test]
    fn a_batch_is_one_e_mail_and_a_single_change_is_unchanged() {
        let one = base(NotifyKind::Activated);
        let (subject, text, html) = batch_email(&[(one.clone(), 0)], 5, None);
        assert_eq!(subject, one.email_subject());
        assert_eq!(html, one.email_body(None).to_html());
        assert_eq!(text, one.email_body(None).to_text());

        let mut two = base(NotifyKind::Cleared { ack: None });
        two.sensor_name = "Sensor 4".into();
        let (subject, text, html) = batch_email(&[(one, 0), (two, 3)], 5, None);
        assert!(subject.ends_with("- 2 changes"), "{subject}");
        assert!(html.contains("2 alarm changes on Cold Store A"), "{html}");
        assert!(html.contains("3 earlier change(s) of this alarm were not e-mailed"));
        assert_eq!(html.matches(FOOTER.as_str()).count(), 1, "one footer");
        assert_eq!(
            text.matches(proximos_brand::MONITOR).count(),
            1,
            "one signature"
        );
    }

    #[test]
    fn location_row_omitted_when_unknown() {
        let mut n = base(NotifyKind::Activated);
        n.location = None;
        let html = n.email_body(None).to_html();
        assert!(!html.contains(">Location<"));

        n.location = Some(String::new());
        assert!(
            !n.email_body(None).to_html().contains(">Location<"),
            "empty string counts as unknown"
        );
    }

    #[test]
    fn cleared_html_ack_attribution_or_notice() {
        let with_ack = base(NotifyKind::Cleared {
            ack: Some(AckInfo {
                user: "alice".into(),
                name: "alice".into(),
                reason: Some("sensor swap".into()),
            }),
        })
        .email_body(None)
        .to_html();
        assert!(with_ack.contains("Acknowledged by alice"));
        assert!(with_ack.contains("Reason: sensor swap"));
        assert!(!with_ack.contains("cleared without being acknowledged"));
        assert!(with_ack.contains(HEADER_CLEARED));

        let without = base(NotifyKind::Cleared { ack: None })
            .email_body(None)
            .to_html();
        assert!(without.contains("cleared without being acknowledged"));
        assert!(
            without.contains("exceeds threshold 40.0 C"),
            "a clear that says why it happened shows it"
        );
    }

    #[test]
    fn ack_html_mandated_sentence() {
        let html = base(NotifyKind::Acknowledged {
            user: "bob".into(),
            name: "bob".into(),
            reason: None,
        })
        .email_body(None)
        .to_html();
        assert!(html.contains("Acknowledged by bob"));
        assert!(html.contains(
            "the alarm is still active \u{2014} this is an acknowledgment, not a resolution"
        ));
    }

    #[test]
    fn html_escapes_operator_and_device_strings() {
        let mut n = base(NotifyKind::Activated);
        n.device_label = Some("<script>alert(1)</script>".into());
        n.message = "a & b < c".into();
        let html = n.email_body(None).to_html();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b &lt; c"));
    }

    #[test]
    fn timestamp_helpers() {
        let t = utc(2026, 1, 5, 3, 7, 9);
        assert_eq!(email_time(t, "cs"), "5. 1. 2026 03:07:09 UTC");
        assert_eq!(iso_utc(t), "2026-01-05T03:07:09Z");
    }

    /// decisions.md #29: escalation, clear and ack have a webhook event only
    /// with `webhook_lifecycle`, and clear/ack then carry who acknowledged.
    #[test]
    fn webhook_events_follow_the_lifecycle_switch() {
        assert_eq!(base(NotifyKind::Escalated).webhook_event(false), None);
        assert_eq!(
            base(NotifyKind::Escalated).webhook_event(true),
            Some("alarm.escalated")
        );
        let cleared = base(NotifyKind::Cleared {
            ack: Some(AckInfo {
                user: "alice".into(),
                name: "alice".into(),
                reason: Some("door".into()),
            }),
        });
        assert_eq!(cleared.webhook_event(true), Some("alarm.cleared"));
        let p = cleared.webhook_payload(true).expect("opted in");
        assert_eq!(p["acknowledgement"]["user"], "alice");
        assert_eq!(p["acknowledgement"]["reason"], "door");
        let ack = base(NotifyKind::Acknowledged {
            user: "bob".into(),
            name: "Bob Novák (bob)".into(),
            reason: None,
        });
        assert_eq!(ack.webhook_event(true), Some("alarm.acknowledged"));
        // `user` stays the machine field; `name` is the additive display label,
        // and the e-mail speaks in it.
        let p = ack.webhook_payload(true).expect("opted in");
        assert_eq!(p["acknowledgement"]["user"], "bob");
        assert_eq!(p["acknowledgement"]["name"], "Bob Novák (bob)");
        assert!(ack.email_subject().starts_with("[ACK by Bob Novák (bob)] "));
        assert!(ack
            .email_body(None)
            .to_html()
            .contains("Acknowledged by Bob Novák (bob)"));
        assert_eq!(ack.webhook_event(false), None);
    }

    /// decisions.md #27: a wired alarm closed from the Hub's reported state
    /// says so, in the e-mail's language, not in the stored English sentence.
    #[test]
    fn a_reconciled_clear_explains_itself_in_czech() {
        let mut n = base(NotifyKind::Cleared { ack: None });
        n.lang = "cs".into();
        // As #28b writes it: the window start follows the sentence.
        n.message = format!(
            "{} Last NORMAL window started 2026-09-25T12:00:00Z.",
            portal_domain::alarm_engine::WIRED_RECONCILE_MESSAGE
        );
        let html = n.email_body(None).to_html();
        assert!(
            html.contains("Přechod do normálu portál nezachytil"),
            "{html}"
        );
        assert!(!html.contains("was not received"));
    }

    #[test]
    fn escalated_email_says_so() {
        let n = base(NotifyKind::Escalated);
        assert_eq!(
            n.email_subject(),
            "[CRITICAL] Cold Store A - Sensor 3 - TEMPERATURE_HIGH (escalated)"
        );
        let html = n.email_body(None).to_html();
        assert!(html.contains("Alarm Escalated to CRITICAL"));
        assert!(html.contains("has now crossed its critical limit"));
        assert!(NotifyGates::default().allows(&NotifyKind::Escalated));
    }

    /// #122: a 1-Wire slot's field has no catalogue name, so the e-mail draws the stored
    /// label in the unified "1W slot N - … - quantity" shape.
    #[test]
    fn a_one_wire_email_uses_the_slot_label() {
        let mut n = base(NotifyKind::Activated);
        n.lang = "cs".into();
        n.quantity = Some("w1_1_temperature".into());
        n.device_name = Some("Sticker 1".into());
        n.sensor_name = "1W slot 1 \u{2014} Temperature".into();
        assert_eq!(n.sensor_label(), "Sticker 1 \u{00b7} 1W slot 1 - teplota");
    }

    /// The worked example, in the customer's language: a Node's humidity
    /// alarm names its Node and its field, and reads "%".
    #[test]
    fn czech_humidity_email_names_the_node_and_unit() {
        let mut n = base(NotifyKind::Activated);
        n.lang = "cs".into();
        n.alarm_type = "HIGH".into();
        n.severity = "warning".into();
        n.quantity = Some("humidity".into());
        n.unit = Some("%".into());
        n.device_name = Some("Sticker 1".into());
        n.value = Some(63.5);
        n.threshold = Some(60.0);
        assert_eq!(
            n.email_subject(),
            "[VAROVÁNÍ] Cold Store A - Sticker 1 \u{00b7} Vlhkost - Vysoká hodnota"
        );
        let html = n.email_body(None).to_html();
        for needle in [
            ">Zařízení<",
            ">Vlhkost<",
            ">63.5 %<",
            ">Mez<",
            ">60.0 %<",
            ">Alarm: varování<",
        ] {
            assert!(html.contains(needle), "missing {needle} in:\n{html}");
        }
        assert!(!html.contains("\u{00b0}C"));
    }

    /// A Hub alarm's subject names the Hub as the alarm list does, not the
    /// internal `sys:online` key; the webhook keeps the raw id.
    #[test]
    fn a_hub_alarm_subject_names_the_hub_not_the_sys_key() {
        let mut n = base(NotifyKind::Activated);
        n.lang = "cs".into();
        n.source = AlarmSource::System;
        n.alarm_type = "HUB_OFFLINE".into();
        n.device_label = Some("Mock Device 1".into());
        n.sensor_id = "sys:online".into();
        n.sensor_name = "sys:online".into();
        n.quantity = None;
        n.unit = None;
        n.value = None;
        n.threshold = None;
        assert_eq!(
            n.email_subject(),
            "[KRITICKÉ] Mock Device 1 - Hub - Hub offline"
        );
        assert!(!n.email_body(None).to_html().contains("sys:online"));
        let hook = n.webhook_payload(false).expect("activated has an event");
        assert_eq!(hook["sensor"]["id"], "sys:online");
        assert_eq!(hook["sensor"]["name"], "sys:online");
    }

    /// A Node's pressure reads in hPa in the e-mail, as in the rest of the
    /// portal (UX-038); the webhook keeps the stored value and unit, which
    /// since the v2 decoder are hPa too.
    #[test]
    fn a_pressure_alarm_email_reads_hpa_and_the_webhook_keeps_the_stored_unit() {
        let mut n = base(NotifyKind::Activated);
        n.alarm_type = "HIGH".into();
        n.source = AlarmSource::Sticker;
        n.quantity = Some("pressure".into());
        n.unit = Some("hPa".into());
        n.value = Some(1013.288);
        n.threshold = Some(1010.0);
        let html = n.email_body(None).to_html();
        for needle in [">1013.3 hPa<", ">1010.0 hPa<"] {
            assert!(html.contains(needle), "missing {needle} in:\n{html}");
        }
        assert!(!html.contains(" Pa<"), "{html}");
        let hook = n.webhook_payload(false).expect("activated has an event");
        assert_eq!(hook["alarm"]["value"], 1013.288);
        assert_eq!(hook["alarm"]["threshold"], 1010.0);
        assert_eq!(hook["sensor"]["unit"], "hPa");
    }

    /// UX-027: with `public_url` both parts link to the alarm detail, every
    /// lifecycle kind alike; without it neither part carries a link.
    #[test]
    fn an_alarm_email_links_to_the_alarm_only_with_a_public_url() {
        let base_url = "https://proximos-ce3d59f8.local";
        let url = "https://proximos-ce3d59f8.local/alarms/8921fdc5-aa";
        for kind in [
            NotifyKind::Activated,
            NotifyKind::Escalated,
            NotifyKind::Cleared { ack: None },
            NotifyKind::Acknowledged {
                user: "bob".into(),
                name: "bob".into(),
                reason: None,
            },
        ] {
            let mut n = base(kind);
            n.alarm_id = "8921fdc5-aa".into();
            let body = n.email_body(Some(base_url));
            let html = body.to_html();
            assert!(html.contains(&format!("href=\"{url}\"")), "{html}");
            assert!(html.contains(">Open alarm in the portal<"), "{html}");
            let text = body.to_text();
            assert!(
                text.contains(&format!("Open alarm in the portal: {url}")),
                "{text}"
            );

            let none = n.email_body(None);
            assert!(!none.to_html().contains("http"), "{}", none.to_html());
            assert!(!none.to_text().contains("http"), "{}", none.to_text());
        }
    }

    /// UX-027: the text part is the e-mail in its own language, rows and
    /// closing sentences included, with no markup and no English stub.
    #[test]
    fn the_text_part_is_the_czech_email_without_markup() {
        let mut n = base(NotifyKind::Cleared {
            ack: Some(AckInfo {
                user: "alice".into(),
                name: "alice".into(),
                reason: Some("výměna <sondy>".into()),
            }),
        });
        n.lang = "cs".into();
        n.alarm_id = "a1".into();
        let text = n.email_body(Some("https://p.local")).to_text();
        for needle in [
            "Alarm skončil\n",
            "Zařízení: Cold Store A\n",
            "Mez: 40.0 \u{00b0}C\n",
            "Otevřít alarm v portálu: https://p.local/alarms/a1\n",
            "Potvrdil uživatel alice\n",
            "Důvod: výměna <sondy>\n",
            SIGNATURE.as_str(),
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(!text.contains("<p") && !text.contains("<tr") && !text.contains("&lt;"));
        assert!(!text.contains("HTML-capable"));
    }

    /// The test e-mail links to the portal root so the admin can check the
    /// address, or says in one sentence that it is missing.
    #[test]
    fn the_test_email_links_to_the_portal_or_says_the_address_is_missing() {
        let mut n = base(NotifyKind::Test);
        n.lang = "cs".into();
        let with = n.email_body(Some("https://p.local")).to_text();
        assert!(
            with.contains("Otevřít portál: https://p.local/\n"),
            "{with}"
        );
        assert!(!with.contains("není nastavená"));
        let without = n.email_body(None);
        assert!(without
            .to_text()
            .contains("Adresa portálu není nastavená, e-maily neobsahují odkaz."));
        assert!(!without.to_html().contains("href"));
    }
}
