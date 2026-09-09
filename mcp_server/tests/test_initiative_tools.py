"""Initiative MCP calls keep the headless daemon path and draft-only surface."""

from pathlib import Path
import unittest
from unittest import mock

from mcp_server import control_client, server


class InitiativeToolsTests(unittest.TestCase):
    def setUp(self):
        self.route = control_client.SocketRoute(Path("/tmp/initiative-daemon.sock"), True)

    def test_list_and_get_use_daemon_without_cli(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(server, "PlanningClient", side_effect=RuntimeError("no cli")), \
             mock.patch.object(control_client, "call", return_value=[]) as call:
            server.list_initiatives(project="claude-manager", include_archived=True)
            call.assert_called_once_with("list_initiatives", {
                "project": "claude-manager", "include_archived": True,
            }, socket_path=self.route.path)
            call.reset_mock()
            server.get_initiative("swarm-design")
            call.assert_called_once_with("get_initiative", {
                "initiative_id": "swarm-design",
            }, socket_path=self.route.path)

    def test_proposals_never_send_active_or_approved_status(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(control_client, "call", return_value={}) as call:
            server.propose_initiative("Swarm Design", "coordinator-id", slug="swarm-design")
            method, body = call.call_args.args
            self.assertEqual(method, "propose_initiative")
            self.assertEqual(body["coordinator_task_id"], "coordinator-id")
            self.assertNotIn("status", body)
            server.add_initiative_project("initiative-id", "predictionTrading", role="consumer")
            method, body = call.call_args.args
            self.assertEqual(method, "propose_initiative_project")
            self.assertEqual(body["initiative_id"], "initiative-id")
            self.assertNotIn("status", body)

    def test_task_filter_survives_daemon_projection(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(control_client, "call", return_value=[{
                 "id": "task-id", "initiative_id": "initiative-id",
             }]) as call:
            tasks = server.list_tasks(initiative_id="initiative-id")
            call.assert_called_once_with("list_tasks", {
                "initiative_id": "initiative-id",
            }, socket_path=self.route.path)
            self.assertEqual(tasks[0]["initiative_id"], "initiative-id")

    def test_migration_has_first_class_object_and_nullable_task_link(self):
        migration = (Path(__file__).parents[2] / "sql" / "015_initiatives.sql").read_text()
        self.assertIn("CREATE TABLE IF NOT EXISTS initiatives", migration)
        self.assertIn("CREATE TABLE IF NOT EXISTS initiative_projects", migration)
        self.assertIn("CREATE TABLE IF NOT EXISTS initiative_events", migration)
        self.assertIn("ALTER TABLE tasks ADD COLUMN IF NOT EXISTS initiative_id UUID", migration)
        self.assertIn("ON DELETE SET NULL", migration)


if __name__ == "__main__":
    unittest.main()
