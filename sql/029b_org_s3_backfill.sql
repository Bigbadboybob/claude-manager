-- Org model S3 one-time data moves (doc/org-model.md §7.2, §7.3), each behind
-- its migrations_applied marker so the restart-every-boot migration runner
-- applies them once per database. The marker INSERT is the claim: a second
-- runner blocks on the unique name until the first commits, then sees
-- nothing inserted (FOUND is false) and skips.

-- items.grp free text -> one top-level heading group per distinct value per
-- board. items.grp stays (old readers) and is kept in step by the write path.
DO $$
BEGIN
    INSERT INTO migrations_applied (name) VALUES ('029b_grp_to_heading_groups')
        ON CONFLICT (name) DO NOTHING;
    IF FOUND THEN
        INSERT INTO groups (board_id, name, kind, created_by)
        SELECT DISTINCT ON (board_id, lower(grp)) board_id, grp, 'heading', 'system:migration'
          FROM items
         WHERE grp IS NOT NULL AND btrim(grp) <> ''
         ORDER BY board_id, lower(grp), grp
        ON CONFLICT DO NOTHING;
        UPDATE items i SET group_id = g.id
          FROM groups g
         WHERE g.board_id = i.board_id AND g.parent_group_id IS NULL
           AND lower(g.name) = lower(i.grp) AND i.group_id IS NULL;
    END IF;
END $$;

-- Holder roles: one holder -> owner; several -> the legacy `holder` role
-- (co-owner) plus a one-time pick_owner flag for the board's steward on open
-- items, so someone names the coordinator. The single-owner rule applies
-- only to writes after this.
DO $$
BEGIN
    INSERT INTO migrations_applied (name) VALUES ('029b_holder_roles')
        ON CONFLICT (name) DO NOTHING;
    IF FOUND THEN
        UPDATE item_holders h SET role = CASE WHEN c.n = 1 THEN 'owner' ELSE 'holder' END
          FROM (SELECT item_id, count(*) AS n FROM item_holders GROUP BY item_id) c
         WHERE c.item_id = h.item_id AND h.role IS NULL;
        INSERT INTO item_flags (item_id, kind, detail)
        SELECT i.id, 'pick_owner', jsonb_build_object('holders', count(*))
          FROM items i JOIN item_holders h ON h.item_id = i.id
         WHERE i.closed_at IS NULL AND i.archived_at IS NULL
         GROUP BY i.id HAVING count(*) > 1
        ON CONFLICT DO NOTHING;
    END IF;
END $$;
