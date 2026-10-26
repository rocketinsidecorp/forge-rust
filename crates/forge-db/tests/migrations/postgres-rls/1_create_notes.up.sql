-- A tenant table under a fail-closed FORCE policy, as `forge generate
-- scaffold` writes it.
CREATE TABLE notes (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    body text NOT NULL
);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY notes_tenant_isolation ON notes
    FOR ALL
    USING (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid);
