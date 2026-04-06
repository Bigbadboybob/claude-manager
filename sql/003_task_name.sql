-- Add name column to tasks. Name is the short label, prompt is optional dispatch text.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS name TEXT;
ALTER TABLE tasks ALTER COLUMN prompt DROP NOT NULL;
