import asyncio
from unittest.mock import patch

import pytest

from mcp_server import control_client, server


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


def test_compute_request_preserves_no_enabled_box_error():
    message = 'no compute box is enabled for this request yet; use lq until cutover'
    error = control_client.ControlError('conflict', message)
    with patch.object(server.control_client, 'call', side_effect=error):
        with pytest.raises(control_client.ControlError) as exc:
            server.compute_request(2, 16, 300, 'normal', 'unit test')
        assert exc.value.message == message and exc.value.code == 'conflict'
        with pytest.raises(Exception) as wrapped:
            asyncio.run(server.mcp._tool_manager.call_tool('compute_request', {
                'cpus': 2, 'mem_gb': 16, 'duration': 300,
                'priority': 'normal', 'purpose': 'unit test'}))
        assert message in str(wrapped.value)


@pytest.mark.parametrize('requested_class', [None, 'long'])
def test_compute_request_accepts_class_alias_through_mcp_validation(requested_class):
    args = {'cpus': 2, 'mem_gb': 16, 'duration': 300,
            'priority': 'normal', 'purpose': 'unit test'}
    if requested_class is not None:
        args['class'] = requested_class
    with patch.object(server.control_client, 'call', return_value={'status': 'queued'}) as call:
        assert asyncio.run(server.mcp._tool_manager.call_tool('compute_request', args)) == {'status': 'queued'}
    assert call.call_args.args[0] == 'compute.request'
    assert call.call_args.args[1]['class'] == (requested_class or 'burst')
    schema = server.mcp._tool_manager.get_tool('compute_request').parameters
    assert schema['properties']['class']['default'] == 'burst'
    assert schema['properties']['justification']['default'] == ''
    assert set(schema['required']) == {'cpus', 'mem_gb', 'duration', 'priority', 'purpose'}
    assert 'justification' in server.mcp._tool_manager.get_tool('compute_request').description


def test_compute_request_forwards_explicit_justification():
    args = {'cpus': 10, 'mem_gb': 80, 'duration': 300, 'priority': 'normal',
            'purpose': 'tests', 'justification': 'five parallel workers'}
    with patch.object(server.control_client, 'call', return_value={'status': 'queued'}) as call:
        asyncio.run(server.mcp._tool_manager.call_tool('compute_request', args))
    assert call.call_args.args[1]['justification'] == 'five parallel workers'
