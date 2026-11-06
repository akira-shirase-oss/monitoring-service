-- Alert delivery channels (M7.1): one outbox row per (event, channel), so a webhook
-- success and an email failure track their own attempts and terminal states. Existing
-- rows are webhook rows — the v1 channel the schema predates.
ALTER TABLE notify_outbox
    ADD COLUMN channel TEXT NOT NULL DEFAULT 'webhook'
        CHECK (channel IN ('webhook', 'email'));
