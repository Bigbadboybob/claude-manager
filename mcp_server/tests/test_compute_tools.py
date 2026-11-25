from unittest.mock import patch

from mcp_server import server


def test_compute_tools_use_daemon_proxy_and_never_send_caller_identity():
    with patch.object(server.control_client, 'call', return_value={'status': 'queued'}) as call:
        assert server.compute_request(2, 16, 300, 'normal', 'unit tests', 'burst', True)['status'] == 'queued'
        call.assert_called_with('compute.request', {'cpus': 2, 'mem_gb': 16, 'duration': 300,
            'priority': 'normal', 'purpose': 'unit tests', 'class': 'burst', 'pinned': True})
        server.compute_status('lease-id')
        call.assert_called_with('compute.status', {'lease_id': 'lease-id'})
        server.compute_extend('lease-id', 60)
        call.assert_called_with('compute.extend', {'lease_id': 'lease-id', 'duration': 60})
        server.compute_release('lease-id')
        call.assert_called_with('compute.release', {'lease_id': 'lease-id'})
