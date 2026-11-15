#!/usr/bin/env python3
"""Black-box lifecycle regressions with fake network tools; never touches host networking.
Run after cargo build: python3 tests/regression.py
"""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = Path(__file__).resolve().parents[1] / "target/debug/vpn-orchestrator"
FAKE = r'''
import json, os, sys, time
from pathlib import Path
root = Path(os.environ["VPN_TEST_ROOT"])
name, args = Path(sys.argv[0]).name, sys.argv[1:]
with (root / "commands").open("a") as log:
    log.write(json.dumps([name, *args]) + "\n")
fail = root / "fail-command"
if fail.exists() and fail.read_text() in " ".join([name, *args]):
    print("injected command failure", file=sys.stderr)
    sys.exit(1)
if name == "ip":
    if args[:3] == ["link", "show", "wg0"] and (root / "server-down").exists():
        sys.exit(1)
    if args[:2] == ["link", "show"] and (root / (args[2] + "-link-down")).exists():
        print("5: test: <POINTOPOINT,NOARP> mtu 1420 state DOWN")
        sys.exit(0)
    if args[:3] == ["link", "set", "dev"]:
        (root / (args[3] + "-link-down")).unlink(missing_ok=True)
    if args[:2] == ["link", "show"]:
        print("5: test: <POINTOPOINT,NOARP,UP> mtu 1420 state UNKNOWN")
    elif args[:3] == ["-4", "-o", "addr"]:
        print("5: wg0 inet 10.8.0.1/24 scope global wg0")
    elif args == ["-4", "route", "show", "table", "main", "default"]:
        if not (root / "no-default").exists():
            print("default via 192.0.2.1 dev eth0 metric 100")
    elif args[:2] == ["rule", "show"]:
        pass
elif name in ("wg-quick", "awg-quick"):
    if args[0] == "up":
        (root / "server-down").unlink(missing_ok=True)
elif name == "ping":
    interface = args[args.index("-I") + 1]
    if "-i" in args and (root / "slow-icmp-probe").exists():
        time.sleep(4)
    down = (root / (interface + "-down")).exists() or (root / (interface + "-link-down")).exists()
    if not down:
        print("64 bytes from 1.1.1.1: time=10.0 ms")
    print("1 packets transmitted, 0 received, 100% packet loss" if down else "1 packets transmitted, 1 received, 0% packet loss")
    sys.exit(1 if down else 0)
elif name == "curl":
    if any("%{speed_download}" in arg for arg in args):
        if (root / "slow-speed-test").exists():
            time.sleep(4)
        interface = args[args.index("--interface") + 1]
        print("302" if (root / "download-redirect").exists() else "200")
        print("0" if (root / "download-redirect").exists() else "1000000")
        print("250000" if interface == "if!wg1" else "1250000")
        print("application/octet-stream")
        print("192.0.2.1\n0.01\n0.02\n0.03\n0.04\n1\nhttps://test.invalid/file")
    else:
        if (root / "slow-https-probe").exists():
            time.sleep(4)
        print("403" if (root / "https-forbidden").exists() else "200")
    sys.exit(28 if (root / "curl-fail").exists() else 0)
elif name == "nft":
    if "-f" in args:
        sys.stdin.read()
elif name == "xray":
    with (root / "xray-starts").open("a") as log:
        log.write(str(time.time()) + "\n")
    while not (root / "xray-fail").exists():
        time.sleep(0.05)
    print("fake xray rejected configuration: exit 23", file=sys.stderr, flush=True)
    sys.exit(23)
'''

CONFIG = '''
[server]
interface = "wg0"
subnet = "10.8.0.0/24"
{server}
[[upstream]]
name = "primary"
type = "wireguard"
interface = "wg1"
health_check_interval = 1
health_check_timeout = 1
failover_threshold = 1
[[upstream]]
name = "backup"
type = "wireguard"
interface = "wg2"
health_check_interval = 1
health_check_timeout = 1
failover_threshold = 1
[routing]
enable_ip_forward = false
[nat]
masquerade = {nat}
use_nftables = false
{extra}
'''

class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="vpn-regression-")
        self.root = Path(self.temp.name)
        self.env = dict(os.environ, VPN_TEST_ROOT=str(self.root))
        self.env.pop("RUST_LOG", None)
        self.env["PATH"] = str(self.root) + os.pathsep + self.env["PATH"]
        for tool in ("ip", "ping", "curl", "wg", "awg", "wg-quick", "awg-quick", "iptables", "nft", "xray"):
            path = self.root / tool
            path.write_text("#!" + sys.executable + "\n" + FAKE)
            path.chmod(0o755)
        self.process = None
        self.log = (self.root / "output").open("w")

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()
        self.temp.cleanup()

    def start(self, *, nat=False, extra="", server="", disabled=""):
        config = self.root / "config.toml"
        config.write_text(CONFIG.format(nat=str(nat).lower(), extra=extra, server=server).replace("[routing]", disabled + "\n[routing]"))
        self.process = subprocess.Popen([str(BINARY), "-c", str(config)], env=self.env, stdout=self.log, stderr=self.log)

    def commands(self):
        path = self.root / "commands"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def wait_for(self, condition, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if condition():
                return
            if self.process.poll() is not None:
                break
            time.sleep(0.05)
        self.fail((self.root / "output").read_text())

    def selected(self, interface):
        return ["ip", "route", "replace", "default", "dev", interface, "table", "100"] in self.commands()

    def start_failing_xray(self):
        (self.root / "xray-fail").touch()
        (self.root / "xray.json").write_text('{}')
        self.start(
            extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false\n[speed_test]\nenabled=true\ninterval=1',
            disabled=f'[[upstream]]\nname="xray-backup"\ntype="xray"\ninterface="tun3"\nconfig_path="{self.root}/xray.json"\nhealth_check_interval=1\nhealth_check_timeout=1',
        )

    def xray_failures(self):
        return [line for line in (self.root / "output").read_text().splitlines()
                if 'event="upstream_recovery_failed" upstream="xray-backup"' in line]

    def xray_starts(self):
        path = self.root / "xray-starts"
        return [float(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_xray_exit_23_obeys_shared_backoff_without_blocking_active_check(self):
        self.start_failing_xray()
        self.wait_for(lambda: len(self.xray_failures()) >= 2, timeout=20)
        failures = self.xray_failures()
        first_retry_ms = int(re.search(r'retry_ms=(\d+)', failures[0])[1])
        second_retry_ms = int(re.search(r'retry_ms=(\d+)', failures[1])[1])
        self.assertTrue(5_000 <= first_retry_ms <= 10_000)
        self.assertTrue(10_000 <= second_retry_ms <= 20_000)
        starts = self.xray_starts()
        self.assertEqual(len(starts), 2)
        # Две секунды стартовой проверки Xray предшествуют отсчёту backoff.
        self.assertGreaterEqual(starts[1] - starts[0], 2 + first_retry_ms / 1000 - 0.1)
        self.assertIn('failures=2', failures[1])
        self.assertIn('xray завершился преждевременно: exit status: 23', failures[1])
        self.assertIn('fake xray rejected configuration: exit 23', (self.root / "output").read_text())
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"), timeout=3)
        self.assertEqual(len(self.xray_starts()), 2)
        # ICMP, периодический опрос и speed test продолжают запрашивать recovery.
        self.wait_for(lambda: len(self.xray_failures()) >= 3, timeout=25)
        starts = self.xray_starts()
        self.assertEqual(len(starts), 3)
        self.assertGreaterEqual(starts[2] - starts[1], 2 + second_retry_ms / 1000 - 0.1)
        self.assertIn('failures=3', self.xray_failures()[2])
        for failure in self.xray_failures():
            self.assertRegex(failure, r'next_retry_unix_ms=\d+')

    def test_xray_backoff_resets_only_after_confirmed_recovery(self):
        self.start_failing_xray()
        self.wait_for(lambda: len(self.xray_failures()) == 1)
        (self.root / "tun3-down").touch()
        (self.root / "xray-fail").unlink()
        # Процесс запустился, но ping ещё не проходит: backoff продолжает расти.
        self.wait_for(lambda: len(self.xray_failures()) == 2, timeout=15)
        self.assertIn('failures=2', self.xray_failures()[1])
        self.assertNotIn('upstream_recovery_succeeded', (self.root / "output").read_text())
        (self.root / "tun3-down").unlink()
        self.wait_for(lambda: 'event="upstream_recovery_succeeded" upstream="xray-backup" attempts=3' in (self.root / "output").read_text(), timeout=25)
        # Уже работающий процесс переиспользуется; после подтверждения счётчик — с нуля.
        self.assertEqual(len(self.xray_starts()), 2)
        (self.root / "xray-fail").touch()
        self.wait_for(lambda: len(self.xray_failures()) >= 3)
        failure = self.xray_failures()[2]
        self.assertIn('failures=1', failure)
        self.assertTrue(5_000 <= int(re.search(r'retry_ms=(\d+)', failure)[1]) <= 10_000)

    def test_failover_keeps_nat_disabled(self):
        self.start()
        self.wait_for(lambda: self.selected("wg1"))
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"))
        self.wait_for(lambda: "vpn_failover_completed" in (self.root / "output").read_text())
        self.process.send_signal(signal.SIGTERM)
        self.assertEqual(self.process.wait(timeout=5), 0)
        output = (self.root / "output").read_text()
        self.assertIn('event="selection_config"', output)
        self.assertIn('event="vpn_failover_started"', output)
        self.assertIn('verification="pre_activation_ping_only"', output)
        self.assertIn('status=exit status: 1', output)
        self.assertFalse(any(c[0] in ("nft", "iptables") for c in self.commands()))
        self.assertIn(["ip", "rule", "add", "priority", "98", "oif", "wg2", "table", "102"], self.commands())
        self.assertFalse(any(c[0].endswith("quick") for c in self.commands()))

    def test_cold_start_waits_for_recovery(self):
        for name in ("wg1", "wg2"):
            (self.root / (name + "-down")).touch()
        self.start()
        self.wait_for(lambda: "direct_fallback_enabled" in (self.root / "output").read_text())
        self.assertIsNone(self.process.poll())
        self.assertIn(["ip", "rule", "add", "priority", "101", "from", "10.8.0.0/24", "type", "unreachable"], self.commands())
        (self.root / "wg2-down").unlink()
        self.wait_for(lambda: self.selected("wg2"))

    def test_monitoring_without_selector_preserves_failover(self):
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false\n[logging]\nlevel="debug"')
        self.wait_for(lambda: self.selected("wg1"))
        self.wait_for(lambda: 'reason="disabled"' in (self.root / "output").read_text())
        output = (self.root / "output").read_text()
        self.assertIn('event="upstream_score"', output)
        for field in ('raw_score=', 'ewma_score=', 'final_score=', 'ewma_loss_pct=', 'health_probe_age_secs='):
            self.assertIn(field, output)
        self.assertIn('source="health_score"', output)
        self.assertIn('interface=wg1 selected=true', output)
        self.assertIn('interface=wg2 selected=false', output)
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"))

    def test_monitoring_reports_no_selected_vpn_when_routes_fail(self):
        (self.root / "fail-command").write_text("ip route replace default dev wg")
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false\n[monitoring.selector]\nenabled=true\n[logging]\nlevel="debug"')
        self.wait_for(lambda: (self.root / "output").read_text().count('event="upstream_score" vpn=backup') >= 2)
        output = (self.root / "output").read_text()
        self.assertIn('event="direct_fallback_enabled"', output)
        self.assertNotIn('event="vpn_activated"', output)
        for line in output.splitlines():
            if 'event="upstream_score"' in line:
                self.assertIn('state=HEALTHY', line)
                self.assertIn('selected=false', line)

    def test_speed_test_switches_to_faster_upstream(self):
        self.start(extra='[speed_test]\nenabled=true\ninterval=1\ntimeout=1\nmin_mbps=5.0\n[monitoring.selector]\nminimum_hold_secs=0\ncooldown_secs=0\nconsecutive_wins=1')
        self.wait_for(lambda: self.selected("wg2"))
        self.wait_for(lambda: 'event="vpn_switch_verified"' in (self.root / "output").read_text())
        output = (self.root / "output").read_text()
        self.assertIn('source="download_speed"', output)
        self.assertIn('verification="post_activation_ping"', output)
        curl = next(c for c in self.commands() if c[0] == "curl")
        self.assertIn("--noproxy", curl)
        self.assertIn("--fail", curl)
        self.assertIn("if!wg1", curl)

    def test_health_check_is_not_blocked_by_speed_test(self):
        (self.root / "slow-speed-test").touch()
        self.start(extra='[speed_test]\nenabled=true\ninterval=1\ntimeout=10')
        self.wait_for(lambda: self.selected("wg1"))
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"), timeout=3)

    def test_health_check_is_not_blocked_by_https_probes(self):
        (self.root / "slow-https-probe").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=true\nhttps_targets=["https://one.invalid/", "https://two.invalid/"]\nhttps_interval=1\nhttps_timeout=10')
        self.wait_for(lambda: self.selected("wg1"))
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"), timeout=3)

    def test_https_probe_accepts_reachable_403(self):
        (self.root / "https-forbidden").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=true\nhttps_targets=["https://chatgpt.com/"]\nhttps_interval=1')
        self.wait_for(lambda: any(c[0] == "curl" and "%{http_code}" in c for c in self.commands()))
        self.wait_for(lambda: "https_probe_failed" not in (self.root / "output").read_text(), timeout=2)
        https_curl = next(c for c in self.commands() if c[0] == "curl" and "%{http_code}" in c)
        self.assertNotIn("--fail", https_curl)
        self.assertIn("--head", https_curl)

    def test_speed_test_skips_monitor_unhealthy_upstream(self):
        (self.root / "wg2-down").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false\n[speed_test]\nenabled=true\ninterval=1\ntimeout=1')
        self.wait_for(lambda: "upstream_unhealthy" in (self.root / "output").read_text())
        self.wait_for(lambda: any(c[0] == "curl" and "if!wg1" in c for c in self.commands()))
        self.assertFalse(any(c[0] == "curl" and "if!wg2" in c for c in self.commands()))

    def test_speed_test_logs_curl_error(self):
        (self.root / "curl-fail").touch()
        self.start(extra='[speed_test]\nenabled=true\ninterval=1\ntimeout=1\nmin_mbps=5.0\n[logging]\nlevel="debug"')
        self.wait_for(lambda: 'reason="active_measurement_missing"' in (self.root / "output").read_text())
        self.assertIn("curl завершился с", (self.root / "output").read_text())
        self.assertFalse(self.selected("wg2"))

    def test_route_error_tries_backup_and_cleans_managed_server(self):
        (self.root / "server-down").touch()
        (self.root / "fail-command").write_text("ip route replace default dev wg1 table 100")
        self.start(server=f'type="amneziawg"\nconfig_path="{self.root}/wg0.conf"')
        self.wait_for(lambda: self.selected("wg2"))
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=5), 0)
        self.assertIn(["awg-quick", "down", str(self.root / "wg0.conf")], self.commands())
        self.assertIn(["ip", "route", "flush", "table", "100"], self.commands())
        self.assertIn("injected command failure", (self.root / "output").read_text())

    def test_nat_failure_rolls_route_back(self):
        self.start(nat=True)
        self.wait_for(lambda: "NAT и FORWARD" in (self.root / "output").read_text())
        (self.root / "fail-command").write_text("iptables -A FORWARD -i wg0 -o wg2")
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: "не удалось активировать upstream" in (self.root / "output").read_text())
        routes = [c for c in self.commands() if c[:4] == ["ip", "route", "replace", "default"] and c[-1] == "100"]
        self.assertEqual([c[5] for c in routes if "via" not in c][-3:], ["wg1", "wg2", "wg1"])

    def test_failed_rollback_blocks_routes_and_recovers(self):
        self.start(nat=True)
        self.wait_for(lambda: "NAT и FORWARD" in (self.root / "output").read_text())
        (self.root / "fail-command").write_text("iptables -A")
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: "восстановление NAT/маршрута также не удалось" in (self.root / "output").read_text())
        self.assertIn(["ip", "route", "flush", "table", "100"], self.commands())
        (self.root / "fail-command").unlink()
        self.wait_for(lambda: (self.root / "output").read_text().count("активный upstream") >= 2)

    def test_http_200_with_curl_failure_is_not_success(self):
        (self.root / "curl-fail").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_interval=1')
        self.wait_for(lambda: "https_probe_failed" in (self.root / "output").read_text())

    def test_existing_managed_interfaces_are_not_stopped(self):
        self.start(server=f'config_path="{self.root}/wg0.conf"')
        self.wait_for(lambda: self.selected("wg1"))
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=5), 0)
        self.assertFalse(any(c[0].endswith("quick") for c in self.commands()))

    def test_rust_log_overrides_configured_level(self):
        self.env["RUST_LOG"] = "error"
        self.start(extra='[logging]\nlevel="trace"')
        self.wait_for(lambda: self.selected("wg1"))
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=5), 0)
        self.assertEqual((self.root / "output").read_text(), "")

    def test_all_upstreams_down_uses_main_gateway_and_nat_then_returns_to_vpn(self):
        for name in ("wg1", "wg2"):
            (self.root / (name + "-down")).touch()
        self.start(nat=True)
        self.wait_for(lambda: "direct_fallback_enabled" in (self.root / "output").read_text())
        self.assertIn(["ip", "route", "replace", "default", "via", "192.0.2.1", "onlink", "dev", "eth0", "table", "100"], self.commands())
        self.assertTrue(any(c[0] == "iptables" and "eth0" in c and "MASQUERADE" in c for c in self.commands()))
        self.assertIn("vpn_unavailable", (self.root / "output").read_text())
        (self.root / "wg2-down").unlink()
        self.wait_for(lambda: "vpn_restored" in (self.root / "output").read_text())
        self.assertTrue(self.selected("wg2"))
        self.assertTrue(any(c[0] == "iptables" and "eth0" in c and "-D" in c for c in self.commands()))

    def test_no_default_route_is_reported_and_retried(self):
        for name in ("wg1", "wg2"):
            (self.root / (name + "-down")).touch()
        (self.root / "no-default").touch()
        self.start(nat=True)
        self.wait_for(lambda: "direct_fallback_failed" in (self.root / "output").read_text())
        self.assertNotIn("direct_fallback_enabled", (self.root / "output").read_text())
        (self.root / "no-default").unlink()
        self.wait_for(lambda: "direct_fallback_enabled" in (self.root / "output").read_text())

    def test_disabled_upstream_never_runs_or_gets_routes_or_probes(self):
        self.start(extra='[monitoring]\nenabled=true\nhealth_probe_count=1\nhttps_enabled=false\n[speed_test]\nenabled=true\ninterval=1', disabled='[[upstream]]\nname="disabled"\ntype="wireguard"\ninterface="wg9"\nconfig_path="/not-found/wg9.conf"\nenabled=false')
        self.wait_for(lambda: any(c[0] == "curl" and "if!wg2" in c for c in self.commands()))
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=5), 0)
        self.assertFalse(any("wg9" in " ".join(c) for c in self.commands()))

    def test_quality_probe_does_not_block_failover(self):
        (self.root / "slow-speed-test").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false\nquality_enabled=true\nquality_timeout=10')
        self.wait_for(lambda: any(c[0] == "curl" for c in self.commands()))
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"), timeout=3)

    def test_slow_monitor_icmp_does_not_block_active_check(self):
        (self.root / "slow-icmp-probe").touch()
        self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false')
        self.wait_for(lambda: any(c[0] == "ping" and "-i" in c for c in self.commands()))
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg2"), timeout=3)

    def test_speed_respects_hold(self):
        self.start(extra='[monitoring.selector]\nminimum_hold_secs=3600\ncooldown_secs=3600\n[speed_test]\nenabled=true\ninterval=1\ntimeout=1\nmin_mbps=5.0\n[logging]\nlevel="debug"')
        self.wait_for(lambda: (self.root / "output").read_text().count('speed test upstream="backup"') >= 3)
        self.assertIn('reason="minimum_hold"', (self.root / "output").read_text())
        self.assertFalse(self.selected("wg2"))

    def test_redirect_does_not_trigger_speed_switch(self):
        (self.root / "download-redirect").touch()
        self.start(extra='[speed_test]\nenabled=true\ninterval=1\ntimeout=1\nmin_mbps=5.0\n[monitoring.selector]\nminimum_hold_secs=0\ncooldown_secs=0\nconsecutive_wins=1')
        self.wait_for(lambda: "expected HTTP 200, got 302" in (self.root / "output").read_text())
        self.assertFalse(self.selected("wg2"))

    def test_existing_down_server_is_restored_without_quick(self):
        (self.root / "wg0-link-down").touch()
        self.start(server=f'config_path="{self.root}/wg0.conf"')
        self.wait_for(lambda: self.selected("wg1"))
        self.assertIn(["ip", "link", "set", "dev", "wg0", "up"], self.commands())
        self.assertFalse(any(c[0].endswith("quick") for c in self.commands()))

    def test_existing_down_upstream_is_restored_without_quick(self):
        original = globals()["CONFIG"]
        globals()["CONFIG"] = CONFIG.replace('interface = "wg1"', f'interface = "wg1"\nconfig_path = "{self.root}/wg1.conf"')
        try:
            (self.root / "wg1-link-down").touch()
            self.start()
            self.wait_for(lambda: self.selected("wg1"))
            self.assertIn(["ip", "link", "set", "dev", "wg1", "up"], self.commands())
            self.assertFalse(any(c[0].endswith("quick") for c in self.commands()))
        finally:
            globals()["CONFIG"] = original

    def start_slow_recovery(self):
        extra_fake = r'''
if name == "ip" and args[:3] == ["link", "show", "wg2"] and (root / "backup-missing").exists():
    sys.exit(1)
if name == "wg-quick" and args[0] == "up":
    import subprocess
    (root / "backup-missing").unlink(missing_ok=True)
    (root / "up-started").touch()
    subprocess.Popen([sys.executable, "-c", "import time,pathlib; time.sleep(4); pathlib.Path(" + repr(str(root / "descendant-completed")) + ").touch()"])
    time.sleep(4)
    (root / "up-completed").touch()
if name == "wg-quick" and args[0] == "down":
    (root / "down-completed").touch()
'''
        fake = FAKE.replace('if name == "ip":', extra_fake + '\nif name == "ip":', 1)
        for tool in ("ip", "wg-quick"):
            (self.root / tool).write_text("#!" + sys.executable + "\n" + fake)
        original = globals()["CONFIG"]
        globals()["CONFIG"] = CONFIG.replace('interface = "wg2"', f'interface = "wg2"\nconfig_path = "{self.root}/wg2.conf"')
        try:
            self.start(extra='[monitoring]\nenabled=true\nhealth_interval=1\nhealth_probe_count=1\nhttps_enabled=false', disabled='[[upstream]]\nname="third"\ntype="wireguard"\ninterface="wg3"\nhealth_check_interval=1\nfailover_threshold=1')
        finally:
            globals()["CONFIG"] = original
        self.wait_for(lambda: self.selected("wg1"))
        (self.root / "wg2-down").touch()
        (self.root / "backup-missing").touch()
        self.wait_for(lambda: (self.root / "up-started").exists())

    def test_shutdown_cancels_recovery_and_cleans_partial_interface(self):
        self.start_slow_recovery()
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=3), 0)
        self.assertTrue((self.root / "down-completed").exists())
        time.sleep(4.2)
        self.assertFalse((self.root / "up-completed").exists())
        self.assertFalse((self.root / "descendant-completed").exists())

    def test_slow_recovery_does_not_block_failover_to_ready_backup(self):
        self.start_slow_recovery()
        (self.root / "wg1-down").touch()
        self.wait_for(lambda: self.selected("wg3"), timeout=3)
        self.assertFalse((self.root / "up-completed").exists())

    def test_cleanup_reports_errors_and_continues(self):
        self.start()
        self.wait_for(lambda: self.selected("wg1"))
        (self.root / "fail-command").write_text("ip rule del priority 100")
        self.process.terminate()
        self.assertNotEqual(self.process.wait(timeout=5), 0)
        self.assertIn("injected command failure", (self.root / "output").read_text())
        self.assertIn(["ip", "route", "flush", "table", "100"], self.commands())

    def test_nat_cleanup_reports_error_and_continues_routes(self):
        self.start(nat=True)
        self.wait_for(lambda: "NAT и FORWARD" in (self.root / "output").read_text())
        (self.root / "fail-command").write_text("iptables -D")
        self.process.terminate()
        self.assertNotEqual(self.process.wait(timeout=5), 0)
        self.assertIn("ошибка очистки NAT", (self.root / "output").read_text())
        self.assertIn(["ip", "route", "flush", "table", "100"], self.commands())

    def test_nft_cleanup_error_is_reported(self):
        original = globals()["CONFIG"]
        globals()["CONFIG"] = CONFIG.replace("use_nftables = false", "use_nftables = true")
        try:
            self.start(nat=True)
        finally:
            globals()["CONFIG"] = original
        self.wait_for(lambda: "NAT и FORWARD" in (self.root / "output").read_text())
        (self.root / "fail-command").write_text("nft delete")
        self.process.terminate()
        self.assertNotEqual(self.process.wait(timeout=5), 0)
        self.assertIn("ошибка очистки NAT", (self.root / "output").read_text())
        self.assertIn(["ip", "route", "flush", "table", "100"], self.commands())

    def test_missing_cleanup_rules_are_not_errors(self):
        self.start(nat=True)
        self.wait_for(lambda: "NAT и FORWARD" in (self.root / "output").read_text())
        fake = FAKE.replace('if name == "ip":', '''
if name == "ip" and args[:2] == ["rule", "del"]:
    print("RTNETLINK answers: No such file or directory", file=sys.stderr)
    sys.exit(2)
if name == "iptables" and "-D" in args:
    print("Bad rule (does a matching rule exist in that chain?).", file=sys.stderr)
    sys.exit(1)
if name == "ip":''', 1)
        for tool in ("ip", "iptables"):
            (self.root / tool).write_text("#!" + sys.executable + "\n" + fake)
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=5), 0)

    def test_zero_cli_interval_is_rejected(self):
        result = subprocess.run([str(BINARY), "--monitor-interval", "0"], env=self.env, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"monitor-interval", result.stderr)

if __name__ == "__main__":
    unittest.main()
