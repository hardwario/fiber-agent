//! Notification persistence (M6-B): global email/webhook config, per-device
//! report config, system-alert config, and the unified delivery history.
//!
//! Crypto-free, like `keys_repo`: the `*_enc` secret blobs arrive already
//! sealed by the bin's key service and are handed back verbatim. The two
//! singleton tables (`notify_global_config`, `system_alert_config`) are
//! seeded by V5, so their getters always return a row. `recipients` columns
//! hold JSON arrays; this repo (de)serializes them to `Vec<String>` so
//! callers never touch the encoding.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{
    DeviceReportConfig, NewReportHistory, NotifyGlobalConfig, ReportHistoryEntry,
    ReportHistoryFilter, SystemAlertConfig,
};
use crate::Storage;

fn row_to_global(r: &rusqlite::Row<'_>) -> rusqlite::Result<NotifyGlobalConfig> {
    Ok(NotifyGlobalConfig {
        provider: r.get(0)?,
        from_email: r.get(1)?,
        from_name: r.get(2)?,
        sendgrid_api_key_enc: r.get(3)?,
        smtp_host: r.get(4)?,
        smtp_port: r.get(5)?,
        smtp_username: r.get(6)?,
        smtp_password_enc: r.get(7)?,
        smtp_use_tls: r.get(8)?,
        smtp_use_ssl: r.get(9)?,
        webhook_enabled: r.get(10)?,
        webhook_url: r.get(11)?,
        webhook_method: r.get(12)?,
        webhook_headers_enc: r.get(13)?,
        webhook_timeout_s: r.get(14)?,
        webhook_max_attempts: r.get(15)?,
        updated_at: r.get(16)?,
        email_lang: r.get(17)?,
        webhook_lifecycle: r.get(18)?,
        public_url: r.get(19)?,
    })
}

impl Storage {
    /// The global email/webhook config singleton (seeded by V5 with
    /// rest.md's defaults: provider `none`, from_name `ProXimos Monitor`,
    /// webhook disabled/POST/10 s/3 attempts).
    pub fn get_notify_global_config(&self) -> Result<NotifyGlobalConfig> {
        self.conn
            .query_row(
                "SELECT provider, from_email, from_name, sendgrid_api_key_enc,
                        smtp_host, smtp_port, smtp_username, smtp_password_enc,
                        smtp_use_tls, smtp_use_ssl, webhook_enabled, webhook_url,
                        webhook_method, webhook_headers_enc, webhook_timeout_s,
                        webhook_max_attempts, updated_at, email_lang, webhook_lifecycle,
                        public_url
                 FROM notify_global_config WHERE id = 1",
                [],
                row_to_global,
            )
            .map_err(Into::into)
    }

    /// Replace the global config (all fields; keep-existing-secret logic is
    /// the caller's job — read, merge, write). `cfg.updated_at` is ignored;
    /// the repo stamps `datetime('now')`.
    pub fn upsert_notify_global_config(&self, cfg: &NotifyGlobalConfig) -> Result<()> {
        self.conn.execute(
            "INSERT INTO notify_global_config
                (id, provider, from_email, from_name, sendgrid_api_key_enc,
                 smtp_host, smtp_port, smtp_username, smtp_password_enc,
                 smtp_use_tls, smtp_use_ssl, webhook_enabled, webhook_url,
                 webhook_method, webhook_headers_enc, webhook_timeout_s,
                 webhook_max_attempts, email_lang, webhook_lifecycle, public_url,
                 updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, datetime('now'))
             ON CONFLICT (id) DO UPDATE SET
                provider = excluded.provider,
                from_email = excluded.from_email,
                from_name = excluded.from_name,
                sendgrid_api_key_enc = excluded.sendgrid_api_key_enc,
                smtp_host = excluded.smtp_host,
                smtp_port = excluded.smtp_port,
                smtp_username = excluded.smtp_username,
                smtp_password_enc = excluded.smtp_password_enc,
                smtp_use_tls = excluded.smtp_use_tls,
                smtp_use_ssl = excluded.smtp_use_ssl,
                webhook_enabled = excluded.webhook_enabled,
                webhook_url = excluded.webhook_url,
                webhook_method = excluded.webhook_method,
                webhook_headers_enc = excluded.webhook_headers_enc,
                webhook_timeout_s = excluded.webhook_timeout_s,
                webhook_max_attempts = excluded.webhook_max_attempts,
                email_lang = excluded.email_lang,
                webhook_lifecycle = excluded.webhook_lifecycle,
                public_url = excluded.public_url,
                updated_at = excluded.updated_at",
            params![
                cfg.provider,
                cfg.from_email,
                cfg.from_name,
                cfg.sendgrid_api_key_enc,
                cfg.smtp_host,
                cfg.smtp_port,
                cfg.smtp_username,
                cfg.smtp_password_enc,
                cfg.smtp_use_tls,
                cfg.smtp_use_ssl,
                cfg.webhook_enabled,
                cfg.webhook_url,
                cfg.webhook_method,
                cfg.webhook_headers_enc,
                cfg.webhook_timeout_s,
                cfg.webhook_max_attempts,
                cfg.email_lang,
                cfg.webhook_lifecycle,
                cfg.public_url,
            ],
        )?;
        Ok(())
    }

    /// Per-device report config, or `None` when the device was never
    /// configured (the API layer answers with rest.md's defaults then).
    pub fn get_device_report_config(&self, hostname: &str) -> Result<Option<DeviceReportConfig>> {
        let row = self
            .conn
            .query_row(
                "SELECT hostname, recipients, enabled, notify_on_alarm,
                        notify_on_clear, notify_on_ack, updated_at
                 FROM device_report_config WHERE hostname = ?1",
                params![hostname],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(hostname, recipients, enabled, on_alarm, on_clear, on_ack, updated_at)| {
                Ok(DeviceReportConfig {
                    hostname,
                    recipients: serde_json::from_str(&recipients)?,
                    enabled,
                    notify_on_alarm: on_alarm,
                    notify_on_clear: on_clear,
                    notify_on_ack: on_ack,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    /// Insert or replace a device's report config. `cfg.updated_at` is
    /// ignored; the repo stamps `datetime('now')`.
    pub fn upsert_device_report_config(&self, cfg: &DeviceReportConfig) -> Result<()> {
        let recipients = serde_json::to_string(&cfg.recipients)?;
        self.conn.execute(
            "INSERT INTO device_report_config
                (hostname, recipients, enabled, notify_on_alarm,
                 notify_on_clear, notify_on_ack, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT (hostname) DO UPDATE SET
                recipients = excluded.recipients,
                enabled = excluded.enabled,
                notify_on_alarm = excluded.notify_on_alarm,
                notify_on_clear = excluded.notify_on_clear,
                notify_on_ack = excluded.notify_on_ack,
                updated_at = excluded.updated_at",
            params![
                cfg.hostname,
                recipients,
                cfg.enabled,
                cfg.notify_on_alarm,
                cfg.notify_on_clear,
                cfg.notify_on_ack,
            ],
        )?;
        Ok(())
    }

    /// Removes a Hub's own report config, so its alarm e-mails fall back to
    /// the default recipients (decisions.md #31). Removing a missing row is
    /// not an error.
    pub fn delete_device_report_config(&self, hostname: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM device_report_config WHERE hostname = ?1",
            params![hostname],
        )?;
        Ok(())
    }

    /// The default alarm recipients (V20, decisions.md #31): who gets the
    /// sensor-alarm e-mails of a Hub without its own report config. Kept out
    /// of [`NotifyGlobalConfig`] so a save of the channels never touches it.
    pub fn default_alarm_recipients(&self) -> Result<Vec<String>> {
        let raw: String = self.conn.query_row(
            "SELECT default_alarm_recipients FROM notify_global_config WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Replaces the default alarm recipients.
    pub fn set_default_alarm_recipients(&self, recipients: &[String]) -> Result<()> {
        let raw = serde_json::to_string(recipients)?;
        self.conn.execute(
            "UPDATE notify_global_config SET default_alarm_recipients = ?1 WHERE id = 1",
            params![raw],
        )?;
        Ok(())
    }

    /// The system-alert config singleton (seeded by V5 with rest.md's
    /// defaults: battery 20 %, storage 90 %, everything but
    /// version-mismatch enabled).
    pub fn get_system_alert_config(&self) -> Result<SystemAlertConfig> {
        let (recipients, cfg) = self.conn.query_row(
            "SELECT recipients, battery_enabled, battery_threshold,
                    network_enabled, storage_enabled, storage_threshold,
                    device_offline_enabled, version_mismatch_enabled,
                    expected_version, updated_at
             FROM system_alert_config WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    SystemAlertConfig {
                        recipients: Vec::new(),
                        battery_enabled: r.get(1)?,
                        battery_threshold: r.get(2)?,
                        network_enabled: r.get(3)?,
                        storage_enabled: r.get(4)?,
                        storage_threshold: r.get(5)?,
                        device_offline_enabled: r.get(6)?,
                        version_mismatch_enabled: r.get(7)?,
                        expected_version: r.get(8)?,
                        updated_at: r.get(9)?,
                    },
                ))
            },
        )?;
        Ok(SystemAlertConfig {
            recipients: serde_json::from_str(&recipients)?,
            ..cfg
        })
    }

    /// Replace the system-alert config. `cfg.updated_at` is ignored; the
    /// repo stamps `datetime('now')`.
    pub fn upsert_system_alert_config(&self, cfg: &SystemAlertConfig) -> Result<()> {
        let recipients = serde_json::to_string(&cfg.recipients)?;
        self.conn.execute(
            "INSERT INTO system_alert_config
                (id, recipients, battery_enabled, battery_threshold,
                 network_enabled, storage_enabled, storage_threshold,
                 device_offline_enabled, version_mismatch_enabled,
                 expected_version, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
             ON CONFLICT (id) DO UPDATE SET
                recipients = excluded.recipients,
                battery_enabled = excluded.battery_enabled,
                battery_threshold = excluded.battery_threshold,
                network_enabled = excluded.network_enabled,
                storage_enabled = excluded.storage_enabled,
                storage_threshold = excluded.storage_threshold,
                device_offline_enabled = excluded.device_offline_enabled,
                version_mismatch_enabled = excluded.version_mismatch_enabled,
                expected_version = excluded.expected_version,
                updated_at = excluded.updated_at",
            params![
                recipients,
                cfg.battery_enabled,
                cfg.battery_threshold,
                cfg.network_enabled,
                cfg.storage_enabled,
                cfg.storage_threshold,
                cfg.device_offline_enabled,
                cfg.version_mismatch_enabled,
                cfg.expected_version,
            ],
        )?;
        Ok(())
    }

    /// Record a delivery attempt (email, webhook, or system alert); returns
    /// the row id. `sent_at = None` uses `datetime('now')`.
    pub fn insert_report_history(&self, row: &NewReportHistory) -> Result<i64> {
        let recipients = serde_json::to_string(&row.recipients)?;
        let id: i64 = self.conn.query_row(
            "INSERT INTO report_history
                (hostname, event_type, recipients, subject, message,
                 status, error_message, provider, source, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                     COALESCE(?10, datetime('now')))
             RETURNING id",
            params![
                row.hostname,
                row.event_type,
                recipients,
                row.subject,
                row.message,
                row.status,
                row.error_message,
                row.provider,
                row.source,
                row.sent_at,
            ],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Filtered, paginated delivery history, newest `sent_at` first
    /// (rest.md `GET /api/reports/history`; add `hostname` for the
    /// per-device endpoint). Returns `(page, total)` where `total` counts
    /// matching rows *before* limit/offset. `filter.limit` is clamped to
    /// `1..=1000`.
    pub fn list_report_history(
        &self,
        filter: &ReportHistoryFilter,
    ) -> Result<(Vec<ReportHistoryEntry>, i64)> {
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(status) = &filter.status {
            clauses.push("status = ?");
            params.push(Box::new(status.clone()));
        }
        if let Some(event_type) = &filter.event_type {
            clauses.push("event_type = ?");
            params.push(Box::new(event_type.clone()));
        }
        if let Some(source) = &filter.source {
            clauses.push("source = ?");
            params.push(Box::new(source.clone()));
        }
        if let Some(hostname) = &filter.hostname {
            clauses.push("hostname = ?");
            params.push(Box::new(hostname.clone()));
        }
        if let Some(since) = &filter.since {
            clauses.push("sent_at >= ?");
            params.push(Box::new(since.clone()));
        }
        if let Some(until) = &filter.until {
            clauses.push("sent_at <= ?");
            params.push(Box::new(until.clone()));
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM report_history {where_sql}");
        let total: i64 = self.conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |r| r.get(0),
        )?;

        let limit = filter.limit.clamp(1, 1000);
        let list_sql = format!(
            "SELECT id, hostname, event_type, recipients, subject, message,
                    status, error_message, provider, source, sent_at
             FROM report_history
             {where_sql}
             ORDER BY sent_at DESC, id DESC
             LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare(&list_sql)?;
        let mut all_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        all_params.push(&limit);
        all_params.push(&filter.offset);

        let rows = stmt.query_map(all_params.as_slice(), |r| {
            Ok((
                ReportHistoryEntry {
                    id: r.get(0)?,
                    hostname: r.get(1)?,
                    event_type: r.get(2)?,
                    recipients: Vec::new(),
                    subject: r.get(4)?,
                    message: r.get(5)?,
                    status: r.get(6)?,
                    error_message: r.get(7)?,
                    provider: r.get(8)?,
                    source: r.get(9)?,
                    sent_at: r.get(10)?,
                },
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut entry, recipients) = row?;
            entry.recipients = serde_json::from_str(&recipients)?;
            out.push(entry);
        }
        Ok((out, total))
    }
}
