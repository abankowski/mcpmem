//! Durable, network-free webhook subscription policy and SQLite persistence.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::{MCSError, Result};
use crate::events::{ChangeEvent, now_us, parse_uuid, sql_error};
use crate::mutation::ChangeOperation;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookSubscription {
    pub subscription_id: Uuid,
    pub endpoint: String,
    pub event_operations: Vec<ChangeOperation>,
    pub entity_types: Vec<String>,
    pub ignored_origins: Vec<String>,
    pub consumer_origin: String,
    pub secret_ref: String,
    pub enabled: bool,
}

impl WebhookSubscription {
    pub fn validate(self) -> Result<Self> {
        if self.endpoint.trim().is_empty()
            || self.consumer_origin.trim().is_empty()
            || self.secret_ref.trim().is_empty()
            || self.endpoint.len() > 2048
            || self.consumer_origin.len() > 256
            || self.secret_ref.len() > 512
            || self.consumer_origin.chars().any(char::is_control)
            || self.secret_ref.chars().any(char::is_control)
        {
            return Err(MCSError::InvalidParams(
                "invalid webhook subscription".into(),
            ));
        }
        Ok(self)
    }
}

pub struct SubscriptionRepository<'a> {
    conn: &'a Connection,
}

impl<'a> SubscriptionRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn upsert(&self, subscription: WebhookSubscription) -> Result<()> {
        let subscription = subscription.validate()?;
        let now = now_us();
        self.conn.execute(
            "INSERT INTO webhook_subscription(subscription_id,endpoint,event_operations,entity_types,ignored_origins,consumer_origin,secret_ref,enabled,created_at_us,updated_at_us) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9) ON CONFLICT(subscription_id) DO UPDATE SET endpoint=excluded.endpoint,event_operations=excluded.event_operations,entity_types=excluded.entity_types,ignored_origins=excluded.ignored_origins,consumer_origin=excluded.consumer_origin,secret_ref=excluded.secret_ref,enabled=excluded.enabled,updated_at_us=excluded.updated_at_us",
            params![subscription.subscription_id.to_string(), subscription.endpoint, serde_json::to_string(&subscription.event_operations)?, serde_json::to_string(&subscription.entity_types)?, serde_json::to_string(&subscription.ignored_origins)?, subscription.consumer_origin, subscription.secret_ref, subscription.enabled, now],
        ).map_err(sql_error)?;
        Ok(())
    }

    pub fn get(&self, id: Uuid) -> Result<Option<WebhookSubscription>> {
        self.conn.query_row("SELECT subscription_id,endpoint,event_operations,entity_types,ignored_origins,consumer_origin,secret_ref,enabled FROM webhook_subscription WHERE subscription_id=?1", [id.to_string()], row).optional().map_err(sql_error)?.map(decode).transpose()
    }

    pub fn delete(&self, id: Uuid) -> Result<bool> {
        self.conn
            .execute(
                "DELETE FROM webhook_subscription WHERE subscription_id=?1",
                [id.to_string()],
            )
            .map(|n| n == 1)
            .map_err(sql_error)
    }

    /// Every subscription row, oldest first. The admin API lists through
    /// this; the delivery worker reads [`matching`] instead, which applies
    /// the enabled and filter predicates.
    pub fn list(&self) -> Result<Vec<WebhookSubscription>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT subscription_id,endpoint,event_operations,entity_types,ignored_origins,consumer_origin,secret_ref,enabled \
                 FROM webhook_subscription ORDER BY created_at_us, subscription_id",
            )
            .map_err(sql_error)?;
        statement
            .query_map([], row)
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?
            .into_iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()
    }

    pub fn matching(&self, event: &ChangeEvent) -> Result<Vec<WebhookSubscription>> {
        let entity_type = event
            .change
            .after
            .as_ref()
            .or(event.change.before.as_ref())
            .map(|x| x.entity_type.as_str())
            .ok_or_else(|| MCSError::MemoryError("event missing entity snapshot".into()))?;
        let mut statement = self.conn.prepare("SELECT subscription_id,endpoint,event_operations,entity_types,ignored_origins,consumer_origin,secret_ref,enabled FROM webhook_subscription WHERE enabled=1").map_err(sql_error)?;
        statement
            .query_map([], row)
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?
            .into_iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()
            .map(|subscriptions| {
                subscriptions
                    .into_iter()
                    .filter(|subscription| {
                        subscription.consumer_origin != event.provenance.origin
                            && !subscription
                                .ignored_origins
                                .contains(&event.provenance.origin)
                            && (subscription.event_operations.is_empty()
                                || subscription
                                    .event_operations
                                    .contains(&event.change.operation))
                            && (subscription.entity_types.is_empty()
                                || subscription
                                    .entity_types
                                    .iter()
                                    .any(|kind| kind == entity_type))
                    })
                    .collect()
            })
    }

    pub fn enqueue_matching(&self, event: &ChangeEvent) -> Result<usize> {
        let matches = self.matching(event)?;
        let mut inserted = 0;
        for subscription in matches {
            inserted += self.conn.execute("INSERT INTO event_outbox(delivery_id,event_id,subscription_id) VALUES(?1,?2,?3) ON CONFLICT(event_id,subscription_id) DO NOTHING", params![Uuid::new_v4().to_string(), event.event_id.to_string(), subscription.subscription_id.to_string()]).map_err(sql_error)?;
        }
        Ok(inserted)
    }
}

type SubscriptionRow = (String, String, String, String, String, String, String, bool);
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubscriptionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}
fn decode(row: SubscriptionRow) -> Result<WebhookSubscription> {
    Ok(WebhookSubscription {
        subscription_id: parse_uuid(&row.0)?,
        endpoint: row.1,
        event_operations: serde_json::from_str(&row.2)?,
        entity_types: serde_json::from_str(&row.3)?,
        ignored_origins: serde_json::from_str(&row.4)?,
        consumer_origin: row.5,
        secret_ref: row.6,
        enabled: row.7,
    })
}
