import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

import deploy
import host


def config():
    return {
        "tunnel_id": "example-api.region",
        "binary": "/usr/local/bin/devtunnel",
        "ports": [4000],
        "allow_anonymous": False,
    }


def tunnel():
    return {"accessControl": [], "ports": [{"portNumber": 4000}]}


class ConfigTests(unittest.TestCase):
    def test_valid(self):
        self.assertEqual(host.validate_config(config()), config())

    def test_tunnel_id_cannot_be_flag_or_shell_command(self):
        for value in ("", "--allow-anonymous", "a;touch /tmp/a", "a\n", None):
            with self.subTest(value=value), self.assertRaises(ValueError):
                host.validate_config({**config(), "tunnel_id": value})

    def test_binary_must_be_absolute(self):
        with self.assertRaises(ValueError):
            host.validate_config({**config(), "binary": "devtunnel"})

    def test_ports(self):
        for ports in ([], [0], [65536], [True], ["4000"], [4000, 4000], None):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_config({**config(), "ports": ports})

    def test_identity_is_not_required(self):
        self.assertNotIn("identity", host.validate_config(config()))

    def test_legacy_identity_field_is_unused(self):
        # Old configs still load, but cannot select or switch a login anymore.
        value = {**config(), "identity": {"selector": "unused", "id": "unused"}}
        self.assertEqual(host.validate_config(value), value)

    def test_anonymous_must_be_explicit_false(self):
        for value in (True, None, "false", 0):
            with self.subTest(value=value), self.assertRaises(ValueError):
                host.validate_config({**config(), "allow_anonymous": value})


class RemoteTests(unittest.TestCase):
    def test_private_remote(self):
        host.validate_remote(tunnel(), [4000])

    def test_missing_acl_fails_closed(self):
        for acl in (None, {}, "private", [{"type": None}]):
            with self.subTest(acl=acl), self.assertRaises(ValueError):
                host.require_private_access(acl)

    def test_anonymous_allow_rejected(self):
        for rule in ({"type": "Anonymous"}, {"type": "anonymous", "isDeny": "false"}):
            with self.subTest(rule=rule), self.assertRaises(ValueError):
                host.require_private_access([rule])

    def test_anonymous_deny_is_not_a_grant(self):
        host.require_private_access([{"type": "Anonymous", "isDeny": True}])

    def test_extra_or_missing_ports(self):
        for ports in (
            [],
            [{"portNumber": 22}],
            [{"portNumber": 4000}, {"portNumber": 22}],
        ):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_remote({**tunnel(), "ports": ports}, [4000])

    def test_malformed_port_inventory(self):
        for ports in (
            [None],
            [{"portNumber": "4000"}],
            [{"portNumber": True}],
            [{"portNumber": 4000}, {"portNumber": 4000}],
        ):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_remote({**tunnel(), "ports": ports}, [4000])

    def test_inline_port_anonymous_acl(self):
        value = tunnel()
        value["ports"][0]["accessControl"] = [{"type": "Anonymous"}]
        with self.assertRaises(ValueError):
            host.validate_remote(value, [4000])

    def test_inspect_uses_actual_cli_schema_and_flag(self):
        with patch.object(
            host,
            "run_cli",
            side_effect=[{"tunnel": tunnel()}, {"accessControlEntries": []}],
        ) as run:
            host.inspect_remote(config())
        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                config()["binary"],
                "access",
                "list",
                config()["tunnel_id"],
                "--port-number",
                "4000",
                "--json",
            ],
        )

    def test_port_acl_anonymous_blocks(self):
        with (
            patch.object(
                host,
                "run_cli",
                side_effect=[
                    {"tunnel": tunnel()},
                    {"accessControlEntries": [{"type": "Anonymous"}]},
                ],
            ),
            self.assertRaises(ValueError),
        ):
            host.inspect_remote(config())

    def test_unknown_port_acl_schema_blocks(self):
        with (
            patch.object(
                host, "run_cli", side_effect=[{"tunnel": tunnel()}, {"unknown": []}]
            ),
            self.assertRaises(ValueError),
        ):
            host.inspect_remote(config())

    def test_cli_failure_does_not_expose_output(self):
        result = subprocess.CompletedProcess([], 1, "secret stdout", "secret stderr")
        with patch.object(host.subprocess, "run", return_value=result):
            with self.assertRaises(RuntimeError) as raised:
                host.run_cli(["devtunnel", "show", "example-api"])
        self.assertNotIn("secret", str(raised.exception))

    def test_renew_checks_before_update(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "config.json"
            path.write_text(json.dumps(config()))
            with (
                patch.object(sys, "argv", ["host.py", "renew", "--config", str(path)]),
                patch.object(host, "run_cli") as run,
                patch.object(host, "inspect_remote", side_effect=ValueError("blocked")),
            ):
                with self.assertRaises(ValueError):
                    host.main()
                run.assert_not_called()

    def test_actions_never_login_or_logout(self):
        for action in ("host", "renew", "doctor"):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as folder:
                path = Path(folder) / "config.json"
                path.write_text(json.dumps(config()))
                with (
                    patch.object(
                        sys, "argv", ["host.py", action, "--config", str(path)]
                    ),
                    patch.object(
                        host,
                        "run_cli",
                        side_effect=[
                            {"tunnel": tunnel()},
                            {"accessControlEntries": []},
                            None,
                        ],
                    ) as run,
                    patch.object(host.os, "execv") as execute,
                    redirect_stdout(io.StringIO()),
                ):
                    self.assertEqual(host.main(), 0)
                calls = [item.args[0] for item in run.call_args_list]
                self.assertEqual(
                    [args[1] for args in calls],
                    ["show", "access", "update"]
                    if action == "renew"
                    else ["show", "access"],
                )
                if action == "host":
                    execute.assert_called_once_with(
                        config()["binary"],
                        [config()["binary"], "host", config()["tunnel_id"]],
                    )
                else:
                    execute.assert_not_called()

    def test_auth_failure_does_not_attempt_relogin_or_host(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "config.json"
            path.write_text(json.dumps(config()))
            with (
                patch.object(sys, "argv", ["host.py", "host", "--config", str(path)]),
                patch.object(
                    host, "run_cli", side_effect=RuntimeError("not logged in")
                ) as run,
                patch.object(host.os, "execv") as execute,
            ):
                with self.assertRaises(RuntimeError):
                    host.main()
                self.assertEqual(run.call_count, 1)
                self.assertEqual(run.call_args.args[0][1], "show")
                execute.assert_not_called()


class DeploymentTests(unittest.TestCase):
    def install_args(self):
        return [
            "deploy.py",
            "--name",
            "example",
            "--tunnel-id",
            "example-api",
            "--port",
            "4000",
            "--binary",
            sys.executable,
        ]

    def test_does_not_shadow_system_units(self):
        with tempfile.TemporaryDirectory() as folder:
            with (
                patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                patch.object(sys, "argv", self.install_args()),
                patch.object(
                    deploy.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, "loaded\n"),
                ),
            ):
                with self.assertRaisesRegex(ValueError, "system unit already"):
                    deploy.main()
            self.assertEqual(list(Path(folder).iterdir()), [])

    def test_failed_system_inventory_does_not_write(self):
        with tempfile.TemporaryDirectory() as folder:
            with (
                patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                patch.object(sys, "argv", self.install_args()),
                patch.object(
                    deploy.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 1, ""),
                ),
            ):
                with self.assertRaisesRegex(ValueError, "Cannot verify"):
                    deploy.main()
            self.assertEqual(list(Path(folder).iterdir()), [])

    def test_does_not_overwrite_unmanaged_user_unit(self):
        with tempfile.TemporaryDirectory() as folder:
            unit = Path(folder) / "systemd/user/devtunnel-example.service"
            unit.parent.mkdir(parents=True)
            unit.write_text("unrelated user content")
            with (
                patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                patch.object(sys, "argv", self.install_args()),
                patch.object(
                    deploy.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, "not-found\n"),
                ),
            ):
                with self.assertRaisesRegex(ValueError, "unmanaged unit"):
                    deploy.main()
            self.assertEqual(unit.read_text(), "unrelated user content")
            self.assertFalse((Path(folder) / "devtunnel-service").exists())

    def units(self):
        return deploy.render_units(
            "example",
            Path("/tmp/config.json"),
            Path(sys.executable).resolve(),
            deploy.SOURCE,
        )

    def test_no_implicit_creation_or_anonymous(self):
        text = "\n".join(self.units().values())
        self.assertNotIn("--allow-anonymous", text)
        self.assertNotIn("RuntimeMaxSec", text)
        self.assertNotIn(" create ", text)
        self.assertIn("Restart=always", text)
        self.assertIn("StartLimitIntervalSec=0", text)
        self.assertIn("OnCalendar=daily", text)

    def test_units_do_not_configure_authentication(self):
        text = "\n".join(self.units().values())
        for value in (
            "Managed Identity",
            "--mi-",
            "user login",
            "169.254.169.254",
            "NO_PROXY",
        ):
            self.assertNotIn(value, text)

    def test_instance_name_validation(self):
        for name in ("../ssh", "SSH", "", "a\nb", "x" * 49):
            with self.subTest(name=name), self.assertRaises(ValueError):
                deploy.render_units(
                    name, Path("/config"), Path("/python"), Path("/repo")
                )

    def test_quote_systemd_specifiers_and_spaces(self):
        self.assertEqual(deploy.quote('/a b/50%/"c"'), '"/a b/50%%/\\"c\\""')
        with self.assertRaises(ValueError):
            deploy.quote("/a\nb")

    def test_private_atomic_write(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "config.json"
            deploy.write_private(path, "first")
            deploy.write_private(path, "second")
            self.assertEqual(path.read_text(), "second")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list(Path(folder).iterdir()), [path])

    def test_dry_run_has_no_writes_or_subprocesses(self):
        with tempfile.TemporaryDirectory() as folder:
            args = [
                "deploy.py",
                "--name",
                "example",
                "--tunnel-id",
                "example-api",
                "--port",
                "4000",
                "--binary",
                "/unused/devtunnel",
                "--dry-run",
            ]
            with (
                patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                patch.object(sys, "argv", args),
                patch.object(deploy.subprocess, "run") as run,
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(deploy.main(), 0)
                run.assert_not_called()
            self.assertEqual(list(Path(folder).iterdir()), [])

    @unittest.skipUnless(shutil.which("systemd-analyze"), "requires systemd-analyze")
    def test_systemd_accepts_rendered_units(self):
        with tempfile.TemporaryDirectory() as folder:
            paths = []
            for name, text in self.units().items():
                path = Path(folder) / name
                path.write_text(text)
                paths.append(str(path))
            result = subprocess.run(
                ["systemd-analyze", "--user", "verify", *paths],
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("not absolute", result.stderr)


if __name__ == "__main__":
    unittest.main()
