use rusqlite::{OptionalExtension, params};

use crate::{
    AglStore, MatrixNotificationOutboxDraft, MatrixNotificationOutboxItem,
    MatrixNotificationOutboxStatus, Result, StoreError,
    util::{store_id, timestamp, validate_non_blank},
};

impl AglStore {
    pub fn enqueue_matrix_notification(
        &self,
        draft: MatrixNotificationOutboxDraft,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(&draft.notify_ref, "matrix_notification_outbox.notify_ref")?;
        validate_non_blank(&draft.source_kind, "matrix_notification_outbox.source_kind")?;
        validate_non_blank(&draft.source_id, "matrix_notification_outbox.source_id")?;
        validate_non_blank(&draft.dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        validate_non_blank(&draft.body, "matrix_notification_outbox.body")?;

        let id = store_id("matrix_outbox");
        let now = timestamp();
        self.conn.execute(
            "INSERT INTO matrix_notification_outbox
             (id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', NULL, ?7, ?7, NULL)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                &id,
                &draft.notify_ref,
                &draft.source_kind,
                &draft.source_id,
                &draft.dedupe_key,
                &draft.body,
                &now
            ],
        )?;
        self.matrix_notification_by_dedupe_key(&draft.dedupe_key)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {}", draft.dedupe_key),
            })
    }

    pub fn queued_matrix_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<MatrixNotificationOutboxItem>> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             WHERE status = 'queued'
             ORDER BY created_at ASC, id ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], matrix_notification_from_row)?;
        let mut notifications = Vec::new();
        for row in rows {
            notifications.push(row??);
        }
        Ok(notifications)
    }

    pub fn queued_matrix_notifications_page(
        &self,
        limit: usize,
    ) -> Result<(Vec<MatrixNotificationOutboxItem>, bool)> {
        let limit = limit.max(1);
        let mut notifications = self.queued_matrix_notifications(limit.saturating_add(1))?;
        let truncated = notifications.len() > limit;
        if truncated {
            notifications.truncate(limit);
        }
        Ok((notifications, truncated))
    }

    pub fn mark_matrix_notification_sent(&self, id: &str) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'sent', error = NULL, updated_at = ?2, delivered_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn mark_matrix_notification_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        validate_non_blank(error, "matrix_notification_outbox.error")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'failed', error = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, error, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn matrix_notification(&self, id: &str) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE id = ?1",
                params![id],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn matrix_notification_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE dedupe_key = ?1",
                params![dedupe_key],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }
}

fn matrix_notification_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MatrixNotificationOutboxItem>> {
    let status: String = row.get(6)?;
    Ok((|| {
        Ok(MatrixNotificationOutboxItem {
            id: row.get(0)?,
            notify_ref: row.get(1)?,
            source_kind: row.get(2)?,
            source_id: row.get(3)?,
            dedupe_key: row.get(4)?,
            body: row.get(5)?,
            status: MatrixNotificationOutboxStatus::parse(&status)?,
            error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            delivered_at: row.get(10)?,
        })
    })())
}
