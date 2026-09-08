CREATE TABLE webhook_subscription (
    subscription_id TEXT PRIMARY KEY,
    endpoint TEXT NOT NULL,
    event_operations TEXT NOT NULL,
    entity_types TEXT NOT NULL,
    ignored_origins TEXT NOT NULL,
    consumer_origin TEXT NOT NULL,
    secret_ref TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    created_at_us INTEGER NOT NULL,
    updated_at_us INTEGER NOT NULL
) STRICT;
CREATE INDEX webhook_subscription_enabled ON webhook_subscription(enabled);
