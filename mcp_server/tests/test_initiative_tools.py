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


    def test_kind_reaches_the_daemon(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(control_client, "call", return_value={}) as call:
            server.propose_initiative("Trader", "coordinator-id", kind="division")
            self.assertEqual(call.call_args.args[1]["kind"], "division")
            server.list_initiatives(kind="division")
            self.assertEqual(call.call_args.args[1]["kind"], "division")

    def test_migration_adds_division_kind_idempotently(self):
        migration = (Path(__file__).parents[2] / "sql" / "021_initiative_kind.sql").read_text()
        self.assertIn("ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'initiative'", migration)
        self.assertIn("IF NOT EXISTS", migration.split("initiatives_kind_check")[0])

    def test_org_routes_to_the_daemon_with_only_given_params(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(control_client, "call", return_value={"view": "me"}) as call:
            server.org()
            call.assert_called_once_with("org", {}, socket_path=self.route.path)
            call.reset_mock()
            server.org(action="set_lead", session_uid="ts-1-2")
            call.assert_called_once_with(
                "org", {"action": "set_lead", "session_uid": "ts-1-2"},
                socket_path=self.route.path,
            )
        with mock.patch.object(control_client, "resolve_socket_route",
                               return_value=control_client.SocketRoute(Path("/tmp/tui.sock"), False)):
            with self.assertRaises(RuntimeError):
                server.org()

    def test_propose_task_forwards_reports_to(self):
        with mock.patch.object(control_client, "resolve_socket_route", return_value=self.route), \
             mock.patch.object(server, "_git_origin_url", return_value="git@x:y.git"), \
             mock.patch.object(control_client, "call", return_value={"id": "t"}) as call:
            server.propose_task("claude-manager", "Report", reports_to="me")
            self.assertEqual(call.call_args.args[1]["reports_to"], "me")
            server.propose_task("claude-manager", "Standalone")
            self.assertNotIn("reports_to", call.call_args.args[1])


if __name__ == "__main__":
    unittest.main()
