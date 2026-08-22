"""
Stress test suite for the Oximite embedded web server, state machine, and API.

Evaluates system robustness under high load without triggering the pump or valve:
1. Rapid sequential telemetry polling (throughput, latency, error rates)
2. Rapid settings reads (larger 2 KB payloads)
3. Concurrent connection bursts (socket backlog handling and recovery)
4. High-frequency command hammering (command queue depth and state integrity)
5. Rapid Steam <-> Stop state transitions
6. Profile CRUD stress (rapid save, retrieve, and delete cycles across multiple slots)
7. Simulated real-world UI session (concurrent 4 Hz polling + user interaction)
8. Abrupt client disconnects / connection drop stress (socket leak prevention)
9. Simultaneous TCP telemetry (port 8080) + heavy HTTP load (port 80)
"""

import concurrent.futures
import json
import socket
import threading
import time
import unittest
from typing import List, Tuple

from common import (
    OximiteTestCase,
    TCP_IP,
    HTTP_PORT,
    TCP_PORT,
    STATE_IDLE,
    STATE_STEAMING,
    STATE_SLEEPING,
    STATE_COOLING,
    STATE_HOT_WATER,
    TcpDiagClient,
    http_get,
    http_post_cmd,
    http_raw_request,
    get_telemetry_http,
    get_settings_http,
    get_profile_http,
)


class TestOximiteStress(OximiteTestCase):
    """Stress tests evaluating system robustness, throughput, and stability."""

    def setUp(self):
        super().setUp()
        self.ensure_idle_state()

    def tearDown(self):
        self.ensure_idle_state()
        super().tearDown()

    def test_01_rapid_sequential_telemetry_polling(self):
        """
        Sends 50 rapid sequential GET /api/telemetry requests.
        Measures total time, average latency, throughput (req/s), and success rate.
        """
        total_requests = 50
        latencies: List[float] = []
        successes = 0

        t_start = time.time()
        for i in range(total_requests):
            t0 = time.time()
            try:
                status, data = http_get("/api/telemetry", timeout=3.0)
                t1 = time.time()
                if status == 200 and isinstance(data, dict) and "st" in data:
                    successes += 1
                    latencies.append(t1 - t0)
            except Exception as e:
                print(f"Req #{i} failed: {e}")

        total_elapsed = time.time() - t_start
        throughput = successes / total_elapsed if total_elapsed > 0 else 0
        avg_latency = (sum(latencies) / len(latencies) * 1000.0) if latencies else 0.0
        min_latency = min(latencies) * 1000.0 if latencies else 0.0
        max_latency = max(latencies) * 1000.0 if latencies else 0.0

        print(f"\n[STRESS 1] Telemetry Polling: {successes}/{total_requests} succeeded in {total_elapsed:.2f}s")
        print(f"           Throughput: {throughput:.1f} req/s | Latency: min={min_latency:.1f}ms, avg={avg_latency:.1f}ms, max={max_latency:.1f}ms")

        self.assertGreaterEqual(successes, total_requests * 0.95, f"Success rate below 95%: {successes}/{total_requests}")

    def test_02_rapid_sequential_settings_reads(self):
        """
        Sends 25 rapid sequential GET /api/settings requests (full 2 KB JSON payload).
        Verifies parser and memory buffer stability.
        """
        total_requests = 25
        successes = 0

        t_start = time.time()
        for i in range(total_requests):
            try:
                status, data = http_get("/api/settings", timeout=3.0)
                if status == 200 and isinstance(data, dict) and "machine" in data:
                    successes += 1
            except Exception as e:
                print(f"Settings Req #{i} failed: {e}")

        total_elapsed = time.time() - t_start
        print(f"\n[STRESS 2] Settings Reads: {successes}/{total_requests} in {total_elapsed:.2f}s ({successes/total_elapsed:.1f} req/s)")
        self.assertGreaterEqual(successes, total_requests * 0.95)

    def test_03_concurrent_connection_bursts_and_recovery(self):
        """
        Sends parallel bursts of 4 concurrent HTTP requests (matching Pico W socket pool).
        Verifies that all bursts complete and that the server remains responsive.
        """
        bursts = 5
        concurrency = 4
        total_successes = 0

        print(f"\n[STRESS 3] Running {bursts} bursts of {concurrency} concurrent requests...")
        for b in range(bursts):
            results: List[Tuple[int, bool]] = []

            def worker(idx: int):
                try:
                    status, data = http_get("/api/telemetry", timeout=3.0)
                    results.append((idx, status == 200 and isinstance(data, dict)))
                except Exception:
                    results.append((idx, False))

            threads = [threading.Thread(target=worker, args=(i,)) for i in range(concurrency)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=4.0)

            burst_success = sum(1 for _, ok in results if ok)
            total_successes += burst_success
            time.sleep(0.1)

        total_expected = bursts * concurrency
        print(f"           Concurrent Bursts: {total_successes}/{total_expected} succeeded")
        # Ensure server recovers and responds cleanly
        status, telem = http_get("/api/telemetry", timeout=3.0)
        self.assertEqual(status, 200, "Server must remain responsive after concurrent bursts")
        self.assertGreaterEqual(total_successes, total_expected * 0.70, "At least 70% of concurrent burst connections should succeed")

    def test_04_command_queue_hammering(self):
        """
        Rapidly fires 20 state/settings commands in sequence (`set_session_temp`, `stop`).
        Verifies that the command queue does not overflow, drop into a wedged state, or corrupt settings.
        """
        total_cmds = 20
        successes = 0

        t_start = time.time()
        for i in range(total_cmds):
            temp_val = 90.0 + (i % 10) * 0.5
            cmd = "set_session_temp" if i % 2 == 0 else "stop"
            payload = {"temp": temp_val} if cmd == "set_session_temp" else {}

            status, resp = http_post_cmd(cmd, payload, timeout=3.0)
            if status == 200 and isinstance(resp, dict) and resp.get("status") == "ok":
                successes += 1

        total_elapsed = time.time() - t_start
        print(f"\n[STRESS 4] Command Hammering: {successes}/{total_cmds} in {total_elapsed:.2f}s ({successes/total_elapsed:.1f} cmd/s)")
        self.assertEqual(successes, total_cmds, "All sequential commands must succeed")

        # Verify state is clean
        self.ensure_idle_state()

    def test_05_rapid_steam_stop_cycles(self):
        """
        Executes 5 rapid Steam -> Stop cycles via HTTP.
        Tests state machine transitions between Steaming (2) and Idle (0) without pump/valve actuation.
        """
        cycles = 5
        print(f"\n[STRESS 5] Running {cycles} rapid Steam <-> Stop transition cycles...")
        for c in range(cycles):
            # Steam
            status, _ = http_post_cmd("steam")
            self.assertEqual(status, 200, f"Cycle {c}: Failed to send steam command")
            self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=3.0), f"Cycle {c}: Failed to enter STEAMING")

            # Stop
            status, _ = http_post_cmd("stop")
            self.assertEqual(status, 200, f"Cycle {c}: Failed to send stop command")
            self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0), f"Cycle {c}: Failed to return to IDLE")

        print(f"           Completed {cycles} Steam <-> Stop cycles successfully")

    def test_06_profile_crud_stress(self):
        """
        Executes 10 rapid Save -> Get -> Delete profile cycles across slots 7, 8, 9.
        Validates RAM cache, Flash persistence signaling, and JSON serialization under repeated writes.
        """
        cycles = 10
        slots = [7, 8, 9]
        successes = 0

        print(f"\n[STRESS 6] Running {cycles} Profile CRUD cycles...")
        for i in range(cycles):
            slot = slots[i % len(slots)]
            profile = {
                "name": f"Stress Prof {i}",
                "steps": [
                    {"time_s": float(i + 1), "pressure": 6.0 + (i % 4), "flow": 2.0},
                    {"volume": 30.0 + i, "pressure": 9.0},
                ]
            }

            # 1. Save
            st_save, resp = http_post_cmd("save_profile", {"slot": slot, "profile": profile})
            # 2. Get
            st_get, fetched = get_profile_http(slot)
            # 3. Delete
            st_del, _ = http_post_cmd("delete_profile", {"slot": slot})
            # 4. Verify 404
            st_verify, _ = get_profile_http(slot)

            if st_save == 200 and st_get == 200 and fetched.get("name") == profile["name"] and st_del == 200 and st_verify == 404:
                successes += 1

        print(f"           Profile CRUD Stress: {successes}/{cycles} cycles passed perfectly")
        self.assertEqual(successes, cycles, "All profile CRUD cycles should pass without error")

    def test_07_simulated_ui_session(self):
        """
        Simulates an active browser session for 10 seconds:
        - A background thread continuously polls /api/telemetry every 250 ms (4 Hz).
        - Concurrently, the main thread performs user actions (adjusting temp, saving settings, stopping).
        Verifies no HTTP lockup, dropped connections, or UI freeze during simultaneous operations.
        """
        duration_s = 10.0
        poll_count = 0
        poll_errors = 0
        running = True

        def telemetry_poller():
            nonlocal poll_count, poll_errors
            while running:
                try:
                    status, _ = http_get("/api/telemetry", timeout=1.5)
                    if status == 200:
                        poll_count += 1
                    else:
                        poll_errors += 1
                except Exception:
                    poll_errors += 1
                time.sleep(0.25)

        poller_thread = threading.Thread(target=telemetry_poller, daemon=True)
        poller_thread.start()

        # Foreground user actions
        user_actions = 0
        start_time = time.time()
        while time.time() - start_time < duration_s:
            # User adjusts session temp
            temp = 91.0 + (user_actions % 5) * 0.5
            http_post_cmd("set_session_temp", {"temp": temp})
            time.sleep(0.8)

            # User fetches profiles
            http_get("/api/profiles")
            time.sleep(0.8)

            # User hits stop
            http_post_cmd("stop")
            time.sleep(0.8)
            user_actions += 1

        running = False
        poller_thread.join(timeout=2.0)

        print(f"\n[STRESS 7] Simulated UI Session ({duration_s}s):")
        print(f"           Telemetry Polls: {poll_count} ok, {poll_errors} errors | User Actions: {user_actions}")
        self.assertGreater(poll_count, 15, "Telemetry poller should have executed at least 15 polls")
        self.assertLessEqual(poll_errors, poll_count * 0.20, "Telemetry error rate under user interaction should be < 20%")

    def test_08_abrupt_disconnect_stress(self):
        """
        Repeatedly connects and immediately closes or aborts TCP sockets on port 80.
        Verifies that graceful_close reclaims the socket without wedging the server.
        """
        iterations = 15
        print(f"\n[STRESS 8] Testing {iterations} abrupt socket disconnects...")
        for i in range(iterations):
            try:
                s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                s.settimeout(2.0)
                s.connect((TCP_IP, HTTP_PORT))
                # Send half an HTTP head then abruptly close
                s.sendall(b"GET /api/telem")
                s.close()
            except Exception:
                pass
            time.sleep(0.05)

        # Confirm server is still functioning properly
        time.sleep(0.2)
        status, telem = http_get("/api/telemetry", timeout=3.0)
        self.assertEqual(status, 200, "Server must remain functional after abrupt client disconnects")
        print("           Server successfully recovered after abrupt disconnects")

    def test_09_tcp_telemetry_streaming_plus_heavy_http_load(self):
        """
        Connects to port 8080 (binary telemetry stream at 50 Hz) and streams data
        while concurrently issuing 30 HTTP requests to port 80.
        Verifies that both ports operate simultaneously without starving the MCU or dropping frames.
        """
        client = TcpDiagClient()
        try:
            client.start(timeout=5.0)
            time.sleep(0.5)
            self.assertTrue(client.running, "TCP diagnostic client should be connected and running")

            initial_frames = len(client.telemetry_history)

            # Bombard port 80 with 30 HTTP requests
            http_successes = 0
            for _ in range(30):
                try:
                    status, _ = http_get("/api/telemetry", timeout=2.0)
                    if status == 200:
                        http_successes += 1
                except Exception:
                    pass
                time.sleep(0.05)

            final_frames = len(client.telemetry_history)
            frames_captured = final_frames - initial_frames

            print(f"\n[STRESS 9] Simultaneous TCP (8080) + HTTP (80) Load:")
            print(f"           Captured {frames_captured} binary frames at 50 Hz | HTTP Successes: {http_successes}/30")
            print(f"           Parse errors on TCP stream: {client.parse_errors}")

            self.assertGreater(frames_captured, 20, "Should capture binary telemetry frames during HTTP load")
            self.assertEqual(client.parse_errors, 0, "Binary telemetry frames should have 0 parse errors")
            self.assertGreaterEqual(http_successes, 25, "At least 25/30 HTTP requests should succeed")
        finally:
            client.stop()


    def test_10_multithreaded_telemetry_and_commands_stress(self):
        """
        Multithreaded concurrency stress test:
        - Thread 1: Rapid Telemetry Poller (GET /api/telemetry)
        - Thread 2: Command Dispatcher (POST /api/cmd with temp adjustments and stops)
        - Thread 3: Settings & Profile Reader (GET /api/settings, GET /api/profiles)
        - Thread 4: TCP 50 Hz Binary Telemetry Stream (Port 8080)
        Runs concurrently for 10 seconds to verify multi-socket server throughput and reliability.
        """
        duration_s = 10.0
        running = True

        stats = {
            "telemetry_ok": 0, "telemetry_err": 0,
            "cmd_ok": 0, "cmd_err": 0,
            "reads_ok": 0, "reads_err": 0,
        }

        # 1. Start TCP stream on port 8080
        tcp_client = TcpDiagClient()
        try:
            tcp_client.start(timeout=5.0)
            time.sleep(0.2)
            init_tcp_frames = len(tcp_client.telemetry_history)

            # Thread 1: Telemetry Poller
            def poller_worker():
                while running:
                    try:
                        status, data = http_get("/api/telemetry", timeout=1.5)
                        if status == 200 and isinstance(data, dict):
                            stats["telemetry_ok"] += 1
                        else:
                            stats["telemetry_err"] += 1
                    except Exception:
                        stats["telemetry_err"] += 1
                    time.sleep(0.1)

            # Thread 2: Command Dispatcher
            def cmd_worker():
                count = 0
                while running:
                    try:
                        if count % 3 == 0:
                            temp = 90.0 + (count % 8) * 0.5
                            status, resp = http_post_cmd("set_session_temp", {"temp": temp}, timeout=2.0)
                        elif count % 3 == 1:
                            status, resp = http_post_cmd("stop", {}, timeout=2.0)
                        else:
                            slot = 9
                            prof = {"name": f"MT-{count}", "steps": [{"time_s": 5.0, "pressure": 6.0}]}
                            status, resp = http_post_cmd("save_profile", {"slot": slot, "profile": prof}, timeout=2.0)

                        if status == 200:
                            stats["cmd_ok"] += 1
                        else:
                            stats["cmd_err"] += 1
                    except Exception:
                        stats["cmd_err"] += 1
                    count += 1
                    time.sleep(0.2)

            # Thread 3: Settings & Profiles Reader
            def reader_worker():
                toggle = False
                while running:
                    try:
                        path = "/api/settings" if toggle else "/api/profiles"
                        toggle = not toggle
                        status, data = http_get(path, timeout=2.0)
                        if status == 200:
                            stats["reads_ok"] += 1
                        else:
                            stats["reads_err"] += 1
                    except Exception:
                        stats["reads_err"] += 1
                    time.sleep(0.25)

            t_poller = threading.Thread(target=poller_worker, daemon=True)
            t_cmd = threading.Thread(target=cmd_worker, daemon=True)
            t_reader = threading.Thread(target=reader_worker, daemon=True)

            print(f"\n[STRESS 10] Starting Multithreaded Concurrency Test ({duration_s}s)...")
            t_start = time.time()
            t_poller.start()
            t_cmd.start()
            t_reader.start()

            time.sleep(duration_s)
            running = False

            t_poller.join(timeout=3.0)
            t_cmd.join(timeout=3.0)
            t_reader.join(timeout=3.0)
            t_elapsed = time.time() - t_start

            tcp_frames = len(tcp_client.telemetry_history) - init_tcp_frames

            print(f"            Duration: {t_elapsed:.2f}s")
            print(f"            HTTP Telemetry Polls : {stats['telemetry_ok']} ok, {stats['telemetry_err']} err")
            print(f"            HTTP Commands        : {stats['cmd_ok']} ok, {stats['cmd_err']} err")
            print(f"            HTTP Settings/Prof   : {stats['reads_ok']} ok, {stats['reads_err']} err")
            print(f"            TCP 50Hz Frames      : {tcp_frames} frames, {tcp_client.parse_errors} parse err")

            # Clean up test slot 9
            http_post_cmd("delete_profile", {"slot": 9})
            self.ensure_idle_state()

            # Assertions
            self.assertGreaterEqual(stats["telemetry_ok"], 15, "Should complete >=15 telemetry polls")
            self.assertGreaterEqual(stats["cmd_ok"], 5, "Should complete >=5 commands concurrently")
            self.assertGreaterEqual(stats["reads_ok"], 5, "Should complete >=5 settings/profile reads")
            self.assertGreater(tcp_frames, 50, "TCP 50 Hz diagnostic stream should not starve")
            self.assertEqual(tcp_client.parse_errors, 0, "0 TCP parse errors expected")
            self.assertLessEqual(stats["cmd_err"], 2, "Command error count must be <= 2")

        finally:
            tcp_client.stop()


    def test_11_back_to_back_and_concurrent_profile_retrieval(self):
        """
        Stress test for back-to-back and concurrent profile retrievals:
        1. Saves 3 distinct profiles into slots 7, 8, and 9.
        2. Executes 20 rapid sequential back-to-back retrieval pairs (Slot 7 -> Slot 8 -> Slot 9).
        3. Executes 10 concurrent parallel bursts requesting multiple profiles simultaneously.
        4. Executes rapid back-to-back profile switching while background telemetry is polling at 4 Hz.
        5. Verifies data consistency, correct schema/values, and zero dropped responses.
        """
        prof7 = {
            "name": "Profile Seven",
            "steps": [
                {"time_s": 5.0, "pressure": 3.0, "flow": 2.0},
                {"time_s": 25.0, "pressure": 9.0, "flow": 2.5},
            ]
        }
        prof8 = {
            "name": "Profile Eight",
            "steps": [
                {"time_s": 8.0, "pressure": 2.0},
                {"time_s": 10.0, "pressure": 8.0},
                {"volume": 36.0, "pressure": 6.0},
            ]
        }
        prof9 = {
            "name": "Profile Nine",
            "steps": [
                {"time_s": 3.0, "pressure": 4.0},
                {"time_s": 12.0, "pressure": 9.0},
                {"volume": 40.0, "pressure": 7.0},
                {"time_s": 5.0, "pressure": 4.0},
            ]
        }

        # Setup profiles in slots 7, 8, 9
        http_post_cmd("save_profile", {"slot": 7, "profile": prof7})
        http_post_cmd("save_profile", {"slot": 8, "profile": prof8})
        http_post_cmd("save_profile", {"slot": 9, "profile": prof9})
        time.sleep(0.3)

        try:
            # Phase 1: Rapid Back-to-Back Sequential Retrievals (20 iterations)
            print("\n[STRESS 11] Phase 1: 20 rapid back-to-back sequential profile retrievals...")
            seq_successes = 0
            t0 = time.time()
            for i in range(20):
                st7, d7 = get_profile_http(7, timeout=2.0)
                st8, d8 = get_profile_http(8, timeout=2.0)
                st9, d9 = get_profile_http(9, timeout=2.0)

                if (st7 == 200 and d7.get("name") == "Profile Seven" and
                    st8 == 200 and d8.get("name") == "Profile Eight" and
                    st9 == 200 and d9.get("name") == "Profile Nine"):
                    seq_successes += 1
            t_seq = time.time() - t0
            print(f"            Sequential Pairs: {seq_successes}/20 perfect in {t_seq:.2f}s ({60/t_seq:.1f} fetches/s)")

            # Phase 2: Parallel Concurrent Retrievals (10 bursts of 3 simultaneous threads)
            print("            Phase 2: 10 bursts of concurrent multi-profile requests...")
            burst_successes = 0
            for b in range(10):
                burst_results = []

                def fetch_worker(slot, expected_name):
                    try:
                        st, d = get_profile_http(slot, timeout=2.5)
                        burst_results.append(st == 200 and d.get("name") == expected_name)
                    except Exception:
                        burst_results.append(False)

                threads = [
                    threading.Thread(target=fetch_worker, args=(7, "Profile Seven")),
                    threading.Thread(target=fetch_worker, args=(8, "Profile Eight")),
                    threading.Thread(target=fetch_worker, args=(9, "Profile Nine")),
                ]
                for t in threads:
                    t.start()
                for t in threads:
                    t.join(timeout=3.0)

                if all(burst_results) and len(burst_results) == 3:
                    burst_successes += 1
                time.sleep(0.05)
            print(f"            Concurrent Bursts: {burst_successes}/10 perfect")

            # Phase 3: Back-to-Back Retrievals with Active Background Telemetry
            print("            Phase 3: Back-to-back profile fetches under 4 Hz background telemetry...")
            running = True
            telem_ok = 0
            telem_err = 0

            def telemetry_worker():
                nonlocal telem_ok, telem_err
                while running:
                    try:
                        st, _ = http_get("/api/telemetry", timeout=1.5)
                        if st == 200:
                            telem_ok += 1
                        else:
                            telem_err += 1
                    except Exception:
                        telem_err += 1
                    time.sleep(0.25)

            t_poller = threading.Thread(target=telemetry_worker, daemon=True)
            t_poller.start()

            phase3_successes = 0
            for i in range(15):
                st7, d7 = get_profile_http(7, timeout=2.0)
                st8, d8 = get_profile_http(8, timeout=2.0)
                if (st7 == 200 and d7.get("name") == "Profile Seven" and
                    st8 == 200 and d8.get("name") == "Profile Eight"):
                    phase3_successes += 1
                time.sleep(0.05)

            running = False
            t_poller.join(timeout=2.0)
            print(f"            Phase 3 Fetches: {phase3_successes}/15 perfect | Background Telemetry: {telem_ok} ok, {telem_err} err")

            # Assertions
            self.assertGreaterEqual(seq_successes, 18, "At least 18/20 sequential retrieval cycles must succeed")
            self.assertGreaterEqual(burst_successes, 7, "At least 7/10 concurrent bursts must succeed")
            self.assertGreaterEqual(phase3_successes, 13, "At least 13/15 profile fetches under telemetry must succeed")

        finally:
            # Clean up test slots
            http_post_cmd("delete_profile", {"slot": 7})
            http_post_cmd("delete_profile", {"slot": 8})
            http_post_cmd("delete_profile", {"slot": 9})
            self.ensure_idle_state()


    def test_12_all_commands_matrix_stress(self):
        """
        Stress test executing EVERY supported API command type in a rapid round-robin loop:
        1. `set_session_temp`
        2. `save_machine`
        3. `save_pids`
        4. `save_wifi`
        5. `save_profile`
        6. `delete_profile`
        7. `steam` -> `stop`
        8. `power` -> `stop`
        9. `stop`
        While background telemetry polling is running concurrently at 4 Hz.
        """
        orig_settings = get_settings_http()
        orig_telem = get_telemetry_http()

        commands_to_test = [
            ("set_session_temp", {"temp": 93.5}),
            ("save_machine", {"machine": orig_settings["machine"]}),
            ("save_pids", {
                "temp_pid": orig_settings["temp_pid"],
                "press_pid": orig_settings["press_pid"],
                "flow_pid": orig_settings["flow_pid"],
            }),
            ("save_wifi", {"wifi": orig_settings["wifi"]}),
            ("save_profile", {
                "slot": 8,
                "profile": {
                    "name": "Matrix Prof",
                    "steps": [{"time_s": 10.0, "volume": 30.0, "pressure": 9.0, "flow": 2.5}]
                }
            }),
            ("delete_profile", {"slot": 8}),
            ("steam", {}),
            ("stop", {}),
            ("power", {}),
            ("stop", {}),
        ]

        rounds = 5
        total_cmd_attempts = rounds * len(commands_to_test)
        cmd_successes = 0
        running = True
        telem_count = 0

        # Background telemetry poller
        def poller():
            nonlocal telem_count
            while running:
                try:
                    st, _ = http_get("/api/telemetry", timeout=1.5)
                    if st == 200:
                        telem_count += 1
                except Exception:
                    pass
                time.sleep(0.25)

        t_poll = threading.Thread(target=poller, daemon=True)
        t_poll.start()

        print(f"\n[STRESS 12] Testing Full Command Matrix ({rounds} rounds x {len(commands_to_test)} command types = {total_cmd_attempts} cmds)...")
        t_start = time.time()
        try:
            for r in range(rounds):
                for cmd_name, payload in commands_to_test:
                    st, resp = http_post_cmd(cmd_name, payload, timeout=3.0)
                    if st == 200 and isinstance(resp, dict) and resp.get("status") == "ok":
                        cmd_successes += 1
                    else:
                        print(f"            Warning: Cmd '{cmd_name}' round {r} returned {st}: {resp}")
                    time.sleep(0.05)

            running = False
            t_poll.join(timeout=2.0)
            t_elapsed = time.time() - t_start

            print(f"            Command Matrix: {cmd_successes}/{total_cmd_attempts} succeeded in {t_elapsed:.2f}s ({cmd_successes/t_elapsed:.1f} cmd/s)")
            print(f"            Concurrent Telemetry Polls: {telem_count} ok")

            self.assertGreaterEqual(cmd_successes, total_cmd_attempts * 0.95, "At least 95% of matrix commands must succeed")
            self.assertGreater(telem_count, 10, "Telemetry polling should remain active during command matrix execution")

        finally:
            # Restore original settings
            try:
                http_post_cmd("save_machine", {"machine": orig_settings["machine"]})
                http_post_cmd("save_pids", {
                    "temp_pid": orig_settings["temp_pid"],
                    "press_pid": orig_settings["press_pid"],
                    "flow_pid": orig_settings["flow_pid"],
                })
                http_post_cmd("save_wifi", {"wifi": orig_settings["wifi"]})
                http_post_cmd("set_session_temp", {"temp": orig_telem["sbt"]})
                http_post_cmd("delete_profile", {"slot": 8})
                http_post_cmd("stop")
            except Exception:
                pass
            self.ensure_idle_state()


    def test_13_rapid_state_machine_transition_churn(self):
        """
        Stress test for state machine transition churn:
        Rapidly cycles through all possible coordinator state branches:
        - Branch 1: Idle -> Steaming -> HotWater -> Idle
        - Branch 2: Idle -> Steaming -> Cooling -> Idle
        - Branch 3: Idle -> Sleeping -> Auto-wake -> Idle
        - Branch 4: Idle -> Steaming -> DirectPump Abort -> Idle
        Executes 5 complete loops (20 complex multi-step transitions) under concurrent 4 Hz telemetry polling.
        """
        running = True
        telem_count = 0

        def poller():
            nonlocal telem_count
            while running:
                try:
                    st, _ = http_get("/api/telemetry", timeout=1.5)
                    if st == 200:
                        telem_count += 1
                except Exception:
                    pass
                time.sleep(0.25)

        t_poll = threading.Thread(target=poller, daemon=True)
        t_poll.start()

        loops = 5
        successful_branches = 0
        print(f"\n[STRESS 13] Rapid State Machine Churn ({loops} loops x 4 branch sequences = 20 transitions)...")
        t_start = time.time()

        def post_with_retry(cmd, payload=None):
            for _ in range(3):
                st, r = http_post_cmd(cmd, payload, timeout=2.5)
                if st == 200:
                    return True
                time.sleep(0.05)
            return False

        try:
            for l in range(loops):
                # Branch 1: Idle -> Steam (2) -> Brew (HotWater 6) -> Stop (Idle 0)
                post_with_retry("steam")
                self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=4.0))
                post_with_retry("brew")
                self.assertTrue(self.wait_for_state_http(STATE_HOT_WATER, timeout=4.0))
                post_with_retry("stop")
                self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=4.0))
                successful_branches += 1

                # Branch 2: Idle -> Steam (2) -> Flush (Cooling 5 / Idle 0) -> Stop (Idle 0)
                post_with_retry("steam")
                self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=4.0))
                post_with_retry("flush")
                self.assertTrue(self.wait_for_state_http([STATE_COOLING, STATE_IDLE], timeout=4.0))
                post_with_retry("stop")
                self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=4.0))
                successful_branches += 1

                # Branch 3: Idle -> Power (Sleeping 3) -> Auto-wake -> Idle (0)
                post_with_retry("power")
                self.assertTrue(self.wait_for_state_http(STATE_SLEEPING, timeout=4.0))
                post_with_retry("stop")
                self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=4.0))
                successful_branches += 1

                # Branch 4: Idle -> Steam (2) -> DirectPump (Abort to Idle 0)
                post_with_retry("steam")
                self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=4.0))
                post_with_retry("direct_pump", {"power": 0.0})
                self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=4.0))
                successful_branches += 1

            running = False
            t_poll.join(timeout=2.0)
            t_elapsed = time.time() - t_start

            print(f"            State Churn: {successful_branches}/20 transition chains completed in {t_elapsed:.2f}s")
            print(f"            Concurrent Telemetry Polls: {telem_count} ok")

            self.assertEqual(successful_branches, loops * 4, "All state transition chains must succeed")
            self.assertGreater(telem_count, 10, "Telemetry poller must stay healthy during state churn")

        finally:
            post_with_retry("stop")
            self.ensure_idle_state()


if __name__ == "__main__":
    unittest.main()




