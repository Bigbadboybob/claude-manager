"""scripts/cm-usage: reading, merging hosts and summarising (doc/usage-recording.md)."""
from __future__ import annotations

import gzip
import importlib.machinery
import importlib.util
import io
import json
from contextlib import redirect_stdout
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

PATH = Path(__file__).parents[1] / 'scripts' / 'cm-usage'
loader = importlib.machinery.SourceFileLoader('cm_usage', str(PATH))
spec = importlib.util.spec_from_loader('cm_usage', loader)
cm_usage = importlib.util.module_from_spec(spec)
loader.exec_module(cm_usage)

T0 = 1_791_331_200.0  # 2026-10-07T00:00:00Z


def sample(ts, host, total, working, level='focused', input_s=0):
    return {'v': 1, 'type': 'sample', 'ts': ts, 'host': host,
            'owner': {'level': level}, 'agents': {'total': total, 'continuous': 0, 'owner': total,
                                                  'by_state': {'working': working, 'idle': total - working}},
            'subagents': {'total': 1, 'active': 1}, 'shells': {'running': 1},
            'owner_input': {'input_s': input_s}}


class CmUsageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, name, rows, gz=False):
        text = ''.join(json.dumps(r) + '\n' for r in rows)
        if gz:
            with gzip.open(self.dir / (name + '.gz'), 'wt') as f:
                f.write(text)
        else:
            (self.dir / name).write_text(text)

    def test_reads_plain_and_gzipped_days_within_range(self):
        self.write('2026-10-05.jsonl', [sample(T0 - 2 * 86400, 'a', 1, 1)], gz=True)
        self.write('2026-10-07.jsonl', [sample(T0 + 60, 'a', 2, 1), sample(T0 + 120, 'a', 3, 1)])
        rows = list(cm_usage.records(self.dir, T0 - 3 * 86400, T0 + 90, None))
        self.assertEqual([r['agents']['total'] for r in rows], [1, 2])

    def test_merge_dedupes_replicated_availability_and_orders_by_time(self):
        a = [sample(T0 + 60, 'a', 1, 1), {'type': 'availability', 'ts': T0 + 30, 'event_id': 'e1', 'to': 'away'}]
        b = [sample(T0 + 61, 'b', 2, 0), {'type': 'availability', 'ts': T0 + 31, 'event_id': 'e1', 'to': 'away'}]
        merged = cm_usage.merge([a, b])
        self.assertEqual([r['type'] for r in merged], ['availability', 'sample', 'sample'])

    def test_multi_host_summary_is_one_row_per_minute(self):
        rows = [cm_usage.flat(sample(T0 + 60, 'a', 2, 1, input_s=40)),
                cm_usage.flat(sample(T0 + 75, 'b', 3, 2, input_s=35))]
        (row,) = cm_usage.by_minute(rows)
        self.assertEqual((row['agents'], row['working'], row['subagents'], row['shells_running']), (5, 3, 2, 2))
        self.assertEqual(row['owner_input_s'], 60, 'capped at a minute')
        self.assertEqual(row['host'], 'all')

    def test_summary_cli_merges_ssh_hosts(self):
        self.write('2026-10-07.jsonl', [sample(T0 + 60, 'laptop', 1, 1)])
        remote = [sample(T0 + 65, 'cm-sessions', 4, 2),
                  {'type': 'availability', 'ts': T0 + 10, 'event_id': 'e9', 'from': 'away', 'to': 'focused'}]
        out = io.StringIO()
        argv = ['cm-usage', '--dir', str(self.dir), '--ssh', 'cm-sessions',
                '--since', str(T0), '--until', str(T0 + 3600)]
        with patch.object(cm_usage, 'remote_records', return_value=remote), patch('sys.argv', argv), \
                redirect_stdout(out):
            cm_usage.main()
        lines = out.getvalue().splitlines()
        self.assertIn('availability away -> focused', lines[0])
        self.assertIn('agents   5', lines[1])
        self.assertIn('[all]', lines[1])


if __name__ == '__main__':
    unittest.main()
