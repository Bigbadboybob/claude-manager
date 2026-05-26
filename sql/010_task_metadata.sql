-- Free-form JSONB metadata bag on tasks, used by skills + agents to attach
-- structured context that isn't a first-class column. First consumer is the
-- design-doc bundle (`metadata.resume.design_doc_path`,
-- `metadata.resume.designer_session_uid`) — see doc/design-doc-link.md. Other
-- skills are expected to nest their own keys here rather than churn the
-- schema for every new bundle shape.
--
-- Idempotent — runs on every API startup.

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS metadata JSONB;
