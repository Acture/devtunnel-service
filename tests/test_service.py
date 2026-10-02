import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

import devtunnel_service
from devtunnel_service import cli, deploy, host

ENTRY = "/bin/sh"  # Any persistent executable stands in for the installed command.


def config() -> host.Config:
    return {
        "tunnel_id": "example-api.region",
        "binary": "/usr/local/bin/devtunnel",
        "ports": [4000],
        "allow_anonymous": False,
    }


def tunnel() -> dict[str, object]:
    return {"accessControl": [], "ports": [{"portNumber": 4000}]}


def write_config(folder: str) -> Path:
    path = Path(folder) / "config.json"
    path.write_text(json.dumps(config()))
    return path


class ConfigTests(unittest.TestCase):
    def test_valid(self) -> None:
        self.assertEqual(host.validate_config(config()), config())

    def test_tunnel_id_cannot_be_flag_or_shell_command(self) -> None:
        for value in ("", "--allow-anonymous", "a;touch /tmp/a", "a\n", None):
            with self.subTest(value=value), self.assertRaises(ValueError):
                host.validate_config({**config(), "tunnel_id": value})

    def test_binary_must_be_absolute(self) -> None:
        for value in ("devtunnel", None, 7):
            with self.subTest(value=value), self.assertRaises(ValueError):
                host.validate_config({**config(), "binary": value})

    def test_ports(self) -> None:
        for ports in ([], [0], [65536], [True], ["4000"], [4000, 4000], None):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_config({**config(), "ports": ports})

    def test_identity_is_not_required(self) -> None:
        self.assertNotIn("identity", host.validate_config(config()))

    def test_legacy_identity_field_is_unused(self) -> None:
        # Old configs still load, but cannot select or switch a login anymore.
        value = {**config(), "identity": {"selector": "unused", "id": "unused"}}
        self.assertEqual(host.validate_config(value), value)

    def test_anonymous_must_be_explicit_false(self) -> None:
        for value in (True, None, "false", 0):
            with self.subTest(value=value), self.assertRaises(ValueError):
                host.validate_config({**config(), "allow_anonymous": value})


class RemoteTests(unittest.TestCase):
    def test_private_remote(self) -> None:
        host.validate_remote(tunnel(), [4000])

    def test_missing_acl_fails_closed(self) -> None:
        for acl in (None, {}, "private", [{"type": None}]):
            with self.subTest(acl=acl), self.assertRaises(ValueError):
                host.require_private_access(acl)

    def test_anonymous_allow_rejected(self) -> None:
        for rule in ({"type": "Anonymous"}, {"type": "anonymous", "isDeny": "false"}):
            with self.subTest(rule=rule), self.assertRaises(ValueError):
                host.require_private_access([rule])

    def test_anonymous_deny_is_not_a_grant(self) -> None:
        host.require_private_access([{"type": "Anonymous", "isDeny": True}])

    def test_extra_or_missing_ports(self) -> None:
        for ports in (
            [],
            [{"portNumber": 22}],
            [{"portNumber": 4000}, {"portNumber": 22}],
        ):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_remote({**tunnel(), "ports": ports}, [4000])

    def test_malformed_port_inventory(self) -> None:
        for ports in (
            [None],
            [{"portNumber": "4000"}],
            [{"portNumber": True}],
            [{"portNumber": 4000}, {"portNumber": 4000}],
        ):
            with self.subTest(ports=ports), self.assertRaises(ValueError):
                host.validate_remote({**tunnel(), "ports": ports}, [4000])

    def test_inline_port_anonymous_acl(self) -> None:
        port = {"portNumber": 4000, "accessControl": [{"type": "Anonymous"}]}
        value = {"accessControl": [], "ports": [port]}
        with self.assertRaises(ValueError):
            host.validate_remote(value, [4000])

    def test_inspect_uses_actual_cli_schema_and_flag(self) -> None:
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

    def test_port_acl_anonymous_blocks(self) -> None:
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

    def test_unknown_schemas_block(self) -> None:
        for responses in (
            [{"tunnel": tunnel()}, {"unknown": []}],
            [{"tunnel": tunnel()}, []],
            [[], {"accessControlEntries": []}],
            [{}, {"accessControlEntries": []}],
        ):
            with (
                self.subTest(responses=responses),
                patch.object(host, "run_cli", side_effect=responses),
                self.assertRaises(ValueError),
            ):
                host.inspect_remote(config())

    def test_cli_failure_does_not_expose_output(self) -> None:
        result = subprocess.CompletedProcess([], 1, "secret stdout", "secret stderr")
        with (
            patch.object(host.subprocess, "run", return_value=result),
            self.assertRaises(RuntimeError) as raised,
        ):
            host.run_cli(["devtunnel", "show", "example-api"])
        self.assertNotIn("secret", str(raised.exception))

    def test_renew_checks_before_update(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            path = write_config(folder)
            with (
                patch.object(host, "run_cli") as run,
                patch.object(host, "inspect_remote", side_effect=ValueError("blocked")),
            ):
                with self.assertRaises(ValueError):
                    cli.run(["renew", "--config", str(path)])
                run.assert_not_called()

    def test_actions_never_login_or_logout(self) -> None:
        for action in ("host", "renew", "doctor"):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as folder:
                path = write_config(folder)
                with (
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
                    self.assertEqual(cli.run([action, "--config", str(path)]), 0)
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

    def test_auth_failure_does_not_attempt_relogin_or_host(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            path = write_config(folder)
            with (
                patch.object(
                    host, "run_cli", side_effect=RuntimeError("not logged in")
                ) as run,
                patch.object(host.os, "execv") as execute,
            ):
                with self.assertRaises(RuntimeError):
                    cli.run(["host", "--config", str(path)])
                self.assertEqual(run.call_count, 1)
                self.assertEqual(run.call_args.args[0][1], "show")
                execute.assert_not_called()


class CommandLineTests(unittest.TestCase):
    def test_version_and_help_need_no_system_access(self) -> None:
        for argv in (["--version"], ["--help"], ["deploy", "--help"]):
            output = io.StringIO()
            with (
                self.subTest(argv=argv),
                patch.dict(os.environ, {"PATH": "", "HOME": "/nonexistent"}),
                patch.object(subprocess, "run") as run,
                redirect_stdout(output),
                self.assertRaises(SystemExit) as exited,
            ):
                cli.run(argv)
            self.assertEqual(exited.exception.code, 0)
            run.assert_not_called()
            self.assertIn("devtunnel-service", output.getvalue())

    def test_version_output(self) -> None:
        output = io.StringIO()
        with redirect_stdout(output), self.assertRaises(SystemExit):
            cli.run(["--version"])
        self.assertEqual(
            output.getvalue().strip(),
            f"devtunnel-service {devtunnel_service.__version__}",
        )

    def test_command_is_required(self) -> None:
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exited:
            cli.run([])
        self.assertEqual(exited.exception.code, 2)

    def test_main_reports_errors_in_one_line(self) -> None:
        error = io.StringIO()
        with (
            patch.object(
                sys, "argv", ["devtunnel-service", "doctor", "--config", "/missing"]
            ),
            redirect_stderr(error),
        ):
            self.assertEqual(cli.main(), 1)
        self.assertRegex(error.getvalue(), r"^FileNotFoundError: .*\n$")

    def test_python_module_entry(self) -> None:
        source = str(Path(devtunnel_service.__file__).resolve().parents[1])
        result = subprocess.run(
            [sys.executable, "-m", "devtunnel_service", "--version"],
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONPATH": source},
            check=True,
        )
        self.assertEqual(
            result.stdout.strip(), f"devtunnel-service {devtunnel_service.__version__}"
        )


class DeploymentTests(unittest.TestCase):
    def install_args(self) -> list[str]:
        return [
            "deploy",
            "--name",
            "example",
            "--tunnel-id",
            "example-api",
            "--port",
            "4000",
            "--binary",
            sys.executable,
            "--entry-point",
            ENTRY,
        ]

    def install(self, folder: str, result: subprocess.CompletedProcess[str]) -> None:
        with (
            patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
            patch.object(deploy.sys, "platform", "linux"),
            patch.object(deploy.subprocess, "run", return_value=result),
        ):
            cli.run(self.install_args())

    def test_does_not_shadow_system_units(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaisesRegex(ValueError, "system unit already"):
                self.install(folder, subprocess.CompletedProcess([], 0, "loaded\n"))
            self.assertEqual(list(Path(folder).iterdir()), [])

    def test_failed_system_inventory_does_not_write(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaisesRegex(ValueError, "Cannot verify"):
                self.install(folder, subprocess.CompletedProcess([], 1, ""))
            self.assertEqual(list(Path(folder).iterdir()), [])

    def test_does_not_overwrite_unmanaged_user_unit(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            unit = Path(folder) / "systemd/user/devtunnel-example.service"
            unit.parent.mkdir(parents=True)
            unit.write_text("unrelated user content")
            with self.assertRaisesRegex(ValueError, "unmanaged unit"):
                self.install(folder, subprocess.CompletedProcess([], 0, "not-found\n"))
            self.assertEqual(unit.read_text(), "unrelated user content")
            self.assertFalse((Path(folder) / "devtunnel-service").exists())

    def test_installs_private_config_and_units(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            output = io.StringIO()
            with redirect_stdout(output):
                self.install(folder, subprocess.CompletedProcess([], 0, "not-found\n"))
            config_path = Path(folder).resolve() / "devtunnel-service/example.json"
            self.assertEqual(config_path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(config_path.parent.stat().st_mode & 0o777, 0o700)
            self.assertFalse(json.loads(config_path.read_text())["allow_anonymous"])
            service = Path(folder) / "systemd/user/devtunnel-example.service"
            self.assertIn(
                f'ExecStart="{ENTRY}" host --config "{config_path}"',
                service.read_text(),
            )
            self.assertIn(f"Entry point: {ENTRY}", output.getvalue())
            self.assertIn("Installed only", output.getvalue())

    def units(self) -> dict[str, str]:
        return deploy.render_units("example", Path("/tmp/config.json"), Path(ENTRY))

    def test_no_implicit_creation_or_anonymous(self) -> None:
        text = "\n".join(self.units().values())
        self.assertNotIn("--allow-anonymous", text)
        self.assertNotIn("RuntimeMaxSec", text)
        self.assertNotIn(" create ", text)
        self.assertIn("Restart=always", text)
        self.assertIn("StartLimitIntervalSec=0", text)
        self.assertIn("OnCalendar=daily", text)

    def test_units_run_installed_command_without_checkout(self) -> None:
        units = self.units()
        text = "\n".join(units.values())
        self.assertNotIn("WorkingDirectory", text)
        self.assertNotIn(".py", text)
        self.assertIn(
            f'ExecStart="{ENTRY}" host --config "/tmp/config.json"',
            units["devtunnel-example.service"],
        )
        self.assertIn(
            f'ExecStart="{ENTRY}" renew --config "/tmp/config.json"',
            units["devtunnel-example-renew.service"],
        )

    def test_units_do_not_configure_authentication(self) -> None:
        text = "\n".join(self.units().values())
        for value in (
            "Managed Identity",
            "--mi-",
            "user login",
            "169.254.169.254",
            "NO_PROXY",
        ):
            self.assertNotIn(value, text)

    def test_instance_name_validation(self) -> None:
        for name in ("../ssh", "SSH", "", "a\nb", "x" * 49):
            with self.subTest(name=name), self.assertRaises(ValueError):
                deploy.render_units(name, Path("/config"), Path("/entry"))

    def test_quote_systemd_specifiers_and_spaces(self) -> None:
        self.assertEqual(deploy.quote('/a b/50%/"c"'), '"/a b/50%%/\\"c\\""')
        with self.assertRaises(ValueError):
            deploy.quote("/a\nb")

    def test_private_atomic_write(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "config.json"
            deploy.write_private(path, "first")
            deploy.write_private(path, "second")
            self.assertEqual(path.read_text(), "second")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list(Path(folder).iterdir()), [path])

    def dry_run(self, *extra: str) -> tuple[str, str]:
        output, error = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as folder:
            args = [
                "deploy",
                "--name",
                "example",
                "--tunnel-id",
                "example-api",
                "--port",
                "4000",
                "--binary",
                "/unused/devtunnel",
                "--dry-run",
                *extra,
            ]
            with (
                patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                patch.object(deploy.subprocess, "run") as run,
                redirect_stdout(output),
                redirect_stderr(error),
            ):
                self.assertEqual(cli.run(args), 0)
                run.assert_not_called()
            self.assertEqual(list(Path(folder).iterdir()), [])
        return output.getvalue(), error.getvalue()

    def test_dry_run_has_no_writes_or_subprocesses(self) -> None:
        output, error = self.dry_run("--entry-point", ENTRY)
        self.assertIn(f'ExecStart="{ENTRY}" host', output)
        self.assertEqual(error, "")

    def test_entry_point_defaults_to_command_on_path(self) -> None:
        found = {"devtunnel-service": "/opt/tool/bin/devtunnel-service"}
        with patch.object(deploy.shutil, "which", side_effect=found.get):
            output, _ = self.dry_run()
        self.assertIn('ExecStart="/opt/tool/bin/devtunnel-service" host', output)

    def test_missing_entry_point_is_explicit(self) -> None:
        with (
            patch.object(deploy.shutil, "which", return_value=None),
            self.assertRaisesRegex(ValueError, "--entry-point"),
        ):
            self.dry_run()

    def test_uvx_cache_entry_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as cache:
            entry = Path(cache) / "archive-v0/abc/bin/devtunnel-service"
            with patch.dict(os.environ, {"UV_CACHE_DIR": cache}):
                output, error = self.dry_run("--entry-point", str(entry))
                self.assertIn("would refuse", error)
                self.assertIn("uv tool install devtunnel-service", error)
                with (
                    tempfile.TemporaryDirectory() as folder,
                    patch.dict(os.environ, {"XDG_CONFIG_HOME": folder}),
                    patch.object(deploy.subprocess, "run") as run,
                    self.assertRaisesRegex(ValueError, "non-persistent"),
                ):
                    cli.run(
                        [*self.install_args()[:-1], str(entry)],
                    )
                run.assert_not_called()
        self.assertIn(str(entry), output)


class PersistenceTests(unittest.TestCase):
    def setUp(self) -> None:
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name).resolve()

    def make(self, relative: str, text: str = "") -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def problem(self, entry: Path, roots: list[Path] | None = None) -> str | None:
        with patch.object(deploy, "transient_roots", return_value=roots or []):
            return deploy.persistence_problem(entry)

    def test_temporary_directory_is_transient(self) -> None:
        roots = deploy.transient_roots()
        self.assertIn(Path(tempfile.gettempdir()).resolve(), roots)
        self.assertIsNotNone(deploy.persistence_problem(self.root / "bin/tool"))

    def test_cache_directories_are_transient(self) -> None:
        with patch.dict(
            os.environ, {"XDG_CACHE_HOME": "/srv/cache", "UV_CACHE_DIR": "/srv/uv"}
        ):
            roots = deploy.transient_roots()
        self.assertIn(Path("/srv/cache").resolve(), roots)
        self.assertIn(Path("/srv/uv").resolve(), roots)

    def test_tool_environment_link_is_persistent(self) -> None:
        self.make("share/tools/devtunnel-service/pyvenv.cfg")
        target = self.make("share/tools/devtunnel-service/bin/devtunnel-service")
        link = self.root / "bin/devtunnel-service"
        link.parent.mkdir()
        link.symlink_to(target)
        self.assertIsNone(self.problem(link))

    def test_checkout_environment_is_refused(self) -> None:
        self.make("checkout/pyproject.toml")
        self.make("checkout/.venv/pyvenv.cfg")
        entry = self.make("checkout/.venv/bin/devtunnel-service")
        self.assertIn("source tree", self.problem(entry) or "")

    def test_link_into_cache_is_refused(self) -> None:
        target = self.make("cache/archive-v0/x/bin/devtunnel-service")
        link = self.root / "bin/devtunnel-service"
        link.parent.mkdir()
        link.symlink_to(target)
        self.assertIn("cache", self.problem(link, [self.root / "cache"]) or "")

    def test_entry_point_keeps_stable_link(self) -> None:
        link = self.root / "bin/devtunnel-service"
        self.assertEqual(deploy.entry_point(link), link)


@unittest.skipUnless(shutil.which("systemd-analyze"), "requires systemd-analyze")
class SystemdTests(unittest.TestCase):
    def test_systemd_accepts_rendered_units(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            paths = []
            units = deploy.render_units("example", Path(folder) / "c.json", Path(ENTRY))
            for name, text in units.items():
                path = Path(folder) / name
                path.write_text(text)
                paths.append(str(path))
            result = subprocess.run(
                ["systemd-analyze", "--user", "verify", *paths],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("not absolute", result.stderr)


if __name__ == "__main__":
    unittest.main()
