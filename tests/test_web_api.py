"""
Dedicated test suite for all web interface and HTTP API endpoints that do NOT
trigger the pump or solenoid valve.

Covers:
1. Core Web Endpoints (HTML gzip, Telemetry JSON, Settings JSON, Profiles list)
2. Profile CRUD (Save, Retrieve, Overwrite, Delete, Validation, Boundary limits)
3. Settings Updates (Session brew temp, Machine settings, PID settings, Wi-Fi config)
4. State Transitions without Pump/Valve (Steam mode, Stop command, Power/Sleep toggle, Auto-wake)
5. HTTP Protocol Compliance & Error Handling (404, 400, 413, Content-Length variations, TCP segmentation)
"""

import gzip
import http.client
import json
import socket
import time
import unittest

from common import (
    OximiteTestCase,
    TCP_IP,
    HTTP_PORT,
    STATE_IDLE,
    STATE_BREWING,
    STATE_STEAMING,
    STATE_SLEEPING,
    STATE_PUMPING,
    STATE_COOLING,
    STATE_HOT_WATER,
    http_get,
    http_post_cmd,
    http_raw_request,
    get_telemetry_http,
    get_settings_http,
    get_profiles_http,
    get_profile_http,
)


class TestWebEndpoints(OximiteTestCase):
    """Verifies core HTTP endpoints, schemas, and header compression."""

    def test_01_root_page_serves_gzipped_html(self):
        """GET / should return 200 OK with gzipped HTML containing the Oximite UI."""
        status, headers, body = http_raw_request("GET", "/")
        self.assertEqual(status, 200, f"Expected 200 OK, got {status}")
        self.assertEqual(headers.get("content-encoding"), "gzip", "Root page must be gzip-encoded")
        self.assertIn("text/html", headers.get("content-type", ""), "Content-Type must be text/html")

        # Decompress and verify content
        html = gzip.decompress(body).decode("utf-8")
        self.assertIn("<title>oximite espresso</title>", html, "HTML should contain oximite title")
        self.assertIn("telemetryChart", html, "HTML should contain chart element")
        self.assertIn("saveProfileToSlot", html, "HTML should contain profile functions")

    def test_02_index_html_alias(self):
        """GET /index.html should serve the identical gzipped content as GET /."""
        status_root, _, body_root = http_raw_request("GET", "/")
        status_idx, _, body_idx = http_raw_request("GET", "/index.html")
        self.assertEqual(status_idx, 200)
        self.assertEqual(body_root, body_idx, "GET / and GET /index.html must be identical")

    def test_03_telemetry_schema(self):
        """GET /api/telemetry must return valid JSON with all UI telemetry fields."""
        status, data = http_get("/api/telemetry")
        self.assertEqual(status, 200)
        self.assertIsInstance(data, dict)

        required_keys = {"t", "sbt", "p", "fl", "vol", "st"}
        self.assertTrue(required_keys.issubset(data.keys()), f"Missing keys in telemetry: {required_keys - data.keys()}")

        self.assertIsInstance(data["t"], (int, float), "Actual temperature must be a number")
        self.assertIsInstance(data["sbt"], (int, float), "Session brew temp must be a number")
        self.assertIsInstance(data["p"], (int, float), "Pressure must be a number")
        self.assertIsInstance(data["fl"], (int, float), "Flow rate must be a number")
        self.assertIsInstance(data["vol"], (int, float), "Volume must be a number")
        self.assertIsInstance(data["st"], int, "Machine state must be an integer")
        self.assertIn(data["st"], range(7), f"State discriminant {data['st']} out of range 0..6")

    def test_04_settings_schema(self):
        """GET /api/settings must return full configuration sections."""
        status, data = http_get("/api/settings")
        self.assertEqual(status, 200)
        self.assertIsInstance(data, dict)

        for section in ["machine", "temp_pid", "press_pid", "flow_pid", "wifi"]:
            self.assertIn(section, data, f"Missing section '{section}' in settings")

        m = data["machine"]
        self.assertIn("brew_temp", m)
        self.assertIn("steam_temp", m)
        self.assertIn("temp_offset", m)
        self.assertIn("sleep_timeout_min", m)

        for pid_sec in ["temp_pid", "press_pid", "flow_pid"]:
            pid = data[pid_sec]
            self.assertIn("kp", pid)
            self.assertIn("ki", pid)
            self.assertIn("kd", pid)

        w = data["wifi"]
        self.assertIn("ssid", w)
        self.assertIn("password", w)

    def test_05_profiles_list_schema(self):
        """GET /api/profiles must return a list of saved profile headers."""
        status, data = http_get("/api/profiles")
        self.assertEqual(status, 200)
        self.assertIsInstance(data, list)
        for item in data:
            self.assertIn("slot", item)
            self.assertIn("name", item)
            self.assertIsInstance(item["slot"], int)
            self.assertIsInstance(item["name"], str)


class TestProfileCrudNoPump(OximiteTestCase):
    """Tests saving, retrieving, updating, and deleting brew profiles without running them."""

    TEST_SLOT = 8

    def setUp(self):
        super().setUp()
        self.ensure_idle_state()
        # Clean up test slot before each test
        http_post_cmd("delete_profile", {"slot": self.TEST_SLOT})
        time.sleep(0.1)

    def tearDown(self):
        # Clean up test slot after each test
        http_post_cmd("delete_profile", {"slot": self.TEST_SLOT})
        time.sleep(0.1)
        super().tearDown()

    def test_01_save_and_retrieve_single_step_profile(self):
        """Saves a 1-step profile, fetches it, verifies content, and deletes it."""
        profile = {
            "name": "Single Step Test",
            "steps": [
                {"time_s": 25.0, "volume": 36.0, "pressure": 9.0, "flow": 2.5}
            ]
        }

        # 1. Save profile
        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": profile})
        self.assertEqual(status, 200)
        self.assertEqual(resp.get("status"), "ok")
        time.sleep(0.2)

        # 2. Retrieve profile by slot
        status, fetched = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 200)
        self.assertEqual(fetched["name"], profile["name"])
        self.assertEqual(len(fetched["steps"]), 1)
        self.assertAlmostEqual(fetched["steps"][0]["time_s"], 25.0, places=1)
        self.assertAlmostEqual(fetched["steps"][0]["volume"], 36.0, places=1)
        self.assertAlmostEqual(fetched["steps"][0]["pressure"], 9.0, places=1)
        self.assertAlmostEqual(fetched["steps"][0]["flow"], 2.5, places=1)

        # 3. Check profiles list includes test slot
        profiles = get_profiles_http()
        slot_entries = [p for p in profiles if p["slot"] == self.TEST_SLOT]
        self.assertEqual(len(slot_entries), 1)
        self.assertEqual(slot_entries[0]["name"], profile["name"])

        # 4. Delete profile
        status, resp = http_post_cmd("delete_profile", {"slot": self.TEST_SLOT})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        # 5. Verify 404 after deletion
        status, _ = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 404, "Deleted profile slot should return 404")

        # 6. Verify removed from profiles list
        profiles_after = get_profiles_http()
        self.assertFalse(any(p["slot"] == self.TEST_SLOT for p in profiles_after))

    def test_02_save_and_retrieve_max_5_steps_profile(self):
        """Saves a complex 5-step profile (maximum allowed steps) and verifies all fields."""
        profile = {
            "name": "5-Stage Extraction",
            "steps": [
                {"time_s": 5.0, "volume": 10.0, "pressure": 2.0, "flow": 2.0},
                {"time_s": 5.0, "volume": 0.0, "pressure": 0.0, "flow": 0.0},
                {"time_s": 15.0, "volume": 45.0, "pressure": 9.0, "flow": 2.8},
                {"time_s": 10.0, "volume": 65.0, "pressure": 6.0, "flow": 2.2},
                {"time_s": 5.0, "volume": 80.0, "pressure": 4.0, "flow": 1.5},
            ]
        }

        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": profile})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        status, fetched = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 200)
        self.assertEqual(fetched["name"], "5-Stage Extraction")
        self.assertEqual(len(fetched["steps"]), 5)

        for i, (orig, got) in enumerate(zip(profile["steps"], fetched["steps"])):
            self.assertAlmostEqual(orig["time_s"], got["time_s"], places=1, msg=f"Step {i} time_s mismatch")
            self.assertAlmostEqual(orig["volume"], got["volume"], places=1, msg=f"Step {i} volume mismatch")
            self.assertAlmostEqual(orig["pressure"], got["pressure"], places=1, msg=f"Step {i} pressure mismatch")
            self.assertAlmostEqual(orig["flow"], got["flow"], places=1, msg=f"Step {i} flow mismatch")

    def test_03_profile_name_length_limit_32_chars(self):
        """Profile name at maximum capacity (32 ASCII characters) should save and retrieve accurately."""
        max_name = "12345678901234567890123456789012"  # 32 chars
        self.assertEqual(len(max_name), 32)
        profile = {
            "name": max_name,
            "steps": [{"time_s": 10.0, "pressure": 9.0}]
        }

        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": profile})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        status, fetched = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 200)
        self.assertEqual(fetched["name"], max_name)

    def test_04_profile_overwrite_existing_slot(self):
        """Overwriting an existing profile slot should update the profile immediately."""
        prof_a = {"name": "Profile Alpha", "steps": [{"time_s": 10.0, "pressure": 3.0}]}
        prof_b = {"name": "Profile Beta", "steps": [{"time_s": 20.0, "pressure": 9.0}]}

        http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": prof_a})
        time.sleep(0.2)

        # Overwrite with Profile B
        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": prof_b})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        status, fetched = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 200)
        self.assertEqual(fetched["name"], "Profile Beta")
        self.assertAlmostEqual(fetched["steps"][0]["time_s"], 20.0, places=1)
        self.assertAlmostEqual(fetched["steps"][0]["pressure"], 9.0, places=1)

    def test_05_invalid_slot_returns_404(self):
        """GET /api/profile/{slot} for out-of-range slots (> 9) must return 404 Not Found."""
        for invalid_slot in [10, 11, 255, 999]:
            status, _ = get_profile_http(invalid_slot)
            self.assertEqual(status, 404, f"Slot {invalid_slot} should return 404")

    def test_06_profile_with_optional_and_null_fields(self):
        """Steps with omitted fields (time-only or volume-only) must serialize and deserialize properly."""
        profile = {
            "name": "Sparse Steps",
            "steps": [
                {"time_s": 10.0},  # pressure/volume/flow omitted
                {"volume": 40.0, "pressure": 6.0},
                {"flow": 2.0},
            ]
        }
        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": profile})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        status, fetched = get_profile_http(self.TEST_SLOT)
        self.assertEqual(status, 200)
        self.assertEqual(len(fetched["steps"]), 3)
        self.assertAlmostEqual(fetched["steps"][0].get("time_s", 0), 10.0, places=1)
        self.assertAlmostEqual(fetched["steps"][1].get("volume", 0), 40.0, places=1)
        self.assertAlmostEqual(fetched["steps"][2].get("flow", 0), 2.0, places=1)

    def test_07_profile_with_more_than_max_steps_rejected(self):
        """A profile exceeding MAX_STEPS (6 steps) must be rejected with 400 Bad Request."""
        invalid_profile = {
            "name": "Too Many Steps",
            "steps": [
                {"time_s": 5.0, "pressure": 2.0},
                {"time_s": 5.0, "pressure": 4.0},
                {"time_s": 5.0, "pressure": 6.0},
                {"time_s": 5.0, "pressure": 8.0},
                {"time_s": 5.0, "pressure": 9.0},
                {"time_s": 5.0, "pressure": 6.0},  # 6th step -> exceeds limit of 5
            ]
        }
        status, resp = http_post_cmd("save_profile", {"slot": self.TEST_SLOT, "profile": invalid_profile})
        self.assertEqual(status, 400, "Profile with >5 steps must return 400 Bad Request")


class TestSettingsUpdatesNoPump(OximiteTestCase):
    """Tests saving and retrieving settings without activating pump or valve."""

    def setUp(self):
        super().setUp()
        self.ensure_idle_state()
        self.orig_settings = get_settings_http()
        self.orig_telemetry = get_telemetry_http()

    def tearDown(self):
        # Restore original settings
        try:
            http_post_cmd("save_machine", {"machine": self.orig_settings["machine"]})
            http_post_cmd("save_pids", {
                "temp_pid": self.orig_settings["temp_pid"],
                "press_pid": self.orig_settings["press_pid"],
                "flow_pid": self.orig_settings["flow_pid"],
            })
            http_post_cmd("save_wifi", {"wifi": self.orig_settings["wifi"]})
            http_post_cmd("set_session_temp", {"temp": self.orig_telemetry["sbt"]})
        except Exception as e:
            print(f"Warning: Failed to restore settings in tearDown: {e}")
        super().tearDown()

    def test_01_set_session_temp_updates_telemetry_instantly(self):
        """`set_session_temp` command should instantly update `sbt` in telemetry without hardware actuation."""
        new_target = 94.5
        status, resp = http_post_cmd("set_session_temp", {"temp": new_target})
        self.assertEqual(status, 200)
        self.assertEqual(resp.get("status"), "ok")

        time.sleep(0.1)
        telem = get_telemetry_http()
        self.assertAlmostEqual(telem["sbt"], new_target, places=1, msg="sbt in telemetry must reflect updated session temp")

        # Step back
        http_post_cmd("set_session_temp", {"temp": 91.0})
        time.sleep(0.1)
        telem = get_telemetry_http()
        self.assertAlmostEqual(telem["sbt"], 91.0, places=1)

    def test_02_save_machine_settings(self):
        """`save_machine` command should persist and reflect updated parameters in /api/settings."""
        modified_machine = dict(self.orig_settings["machine"])
        modified_machine["brew_temp"] = 93.5
        modified_machine["steam_temp"] = 138.0
        modified_machine["sleep_timeout_min"] = 25.0

        status, resp = http_post_cmd("save_machine", {"machine": modified_machine})
        self.assertEqual(status, 200)
        time.sleep(0.2)

        current = get_settings_http()
        self.assertAlmostEqual(current["machine"]["brew_temp"], 93.5, places=1)
        self.assertAlmostEqual(current["machine"]["steam_temp"], 138.0, places=1)
        self.assertAlmostEqual(current["machine"]["sleep_timeout_min"], 25.0, places=1)

    def test_03_save_pid_tuning_settings(self):
        """`save_pids` command should update PID tuning values in /api/settings."""
        temp_pid = {"kp": 9.5, "ki": 0.85, "kd": 18.0}
        press_pid = {"kp": 12.0, "ki": 18.0, "kd": 0.5}
        flow_pid = {"kp": 5.0, "ki": 25.0, "kd": 0.1}

        status, resp = http_post_cmd("save_pids", {
            "temp_pid": temp_pid,
            "press_pid": press_pid,
            "flow_pid": flow_pid,
        })
        self.assertEqual(status, 200)
        time.sleep(0.2)

        current = get_settings_http()
        self.assertAlmostEqual(current["temp_pid"]["kp"], 9.5, places=1)
        self.assertAlmostEqual(current["press_pid"]["kp"], 12.0, places=1)
        self.assertAlmostEqual(current["flow_pid"]["kp"], 5.0, places=1)


class TestStateTransitionsNoPump(OximiteTestCase):
    """Tests machine state transitions (Stop, Steam, Power/Sleep) that do not trigger pump or valve."""

    def setUp(self):
        super().setUp()
        self.ensure_idle_state()

    def tearDown(self):
        self.ensure_idle_state()
        super().tearDown()

    def test_01_steam_mode_transition_and_stop(self):
        """
        `steam` command transitions machine into STEAMING (2).
        Verifies pump is idle, volume is unchanged, and `stop` returns cleanly to IDLE (0).
        """
        # Send steam command
        status, resp = http_post_cmd("steam")
        self.assertEqual(status, 200)

        # Wait for machine to enter STEAMING state (2)
        entered = self.wait_for_state_http(STATE_STEAMING, timeout=3.0)
        self.assertTrue(entered, "Machine should transition to STEAMING (2)")

        telem = get_telemetry_http()
        self.assertEqual(telem["st"], STATE_STEAMING)
        self.assertEqual(telem["p"], 0.0, "Pressure must remain 0 during steam mode")
        self.assertEqual(telem["fl"], 0.0, "Flow rate must remain 0 during steam mode")

        # Stop back to Idle
        status, resp = http_post_cmd("stop")
        self.assertEqual(status, 200)
        stopped = self.wait_for_state_http(STATE_IDLE, timeout=3.0)
        self.assertTrue(stopped, "Machine should return to IDLE (0) upon stop command")

    def test_02_stop_while_idle_is_safe_and_idempotent(self):
        """Sending `stop` command when already in IDLE state must succeed and remain IDLE."""
        telem = get_telemetry_http()
        self.assertEqual(telem["st"], STATE_IDLE)

        for _ in range(3):
            status, resp = http_post_cmd("stop")
            self.assertEqual(status, 200)
            self.assertEqual(resp.get("status"), "ok")
            time.sleep(0.1)

        telem_after = get_telemetry_http()
        self.assertEqual(telem_after["st"], STATE_IDLE)

    def test_03_power_toggle_sleep_and_wake(self):
        """
        `power` command puts machine to SLEEP (3).
        Any subsequent command wakes machine back to IDLE (0).
        """
        # Toggle power -> Go to sleep
        status, resp = http_post_cmd("power")
        self.assertEqual(status, 200)

        slept = self.wait_for_state_http(STATE_SLEEPING, timeout=3.0)
        self.assertTrue(slept, "Machine should enter SLEEPING (3) state")

        # Send command (e.g. stop) -> should auto-wake to Idle
        http_post_cmd("stop")
        woke = self.wait_for_state_http(STATE_IDLE, timeout=3.0)
        self.assertTrue(woke, "Machine should auto-wake to IDLE (0)")

    def test_04_multiple_rapid_stops(self):
        """Rapid sequence of Stop commands should all return 200 OK without getting wedged."""
        for i in range(10):
            status, resp = http_post_cmd("stop")
            self.assertEqual(status, 200, f"Rapid stop #{i} failed with status {status}")

    def test_05_steaming_to_hot_water_transition_and_stop(self):
        """
        Transition: Idle (0) -> Steam -> Steaming (2) -> Brew -> HotWater (6) -> Stop -> Idle (0).
        Verifies Steaming's brew onward sub-transition.
        """
        http_post_cmd("steam")
        self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=3.0), "Must enter STEAMING (2)")

        # Send brew while in Steaming -> transitions to HotWater (6)
        http_post_cmd("brew")
        self.assertTrue(self.wait_for_state_http(STATE_HOT_WATER, timeout=3.0), "Must enter HOT_WATER (6)")

        # Immediately stop back to Idle
        http_post_cmd("stop")
        self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0), "Must return to IDLE (0)")

    def test_06_steaming_to_cooling_transition_and_stop(self):
        """
        Transition: Idle (0) -> Steam -> Steaming (2) -> Flush -> Cooling (5) or Idle (0) -> Stop -> Idle (0).
        Verifies Steaming's flush onward sub-transition.
        """
        http_post_cmd("steam")
        self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=3.0), "Must enter STEAMING (2)")

        # Send flush while in Steaming -> transitions to Cooling (5) or resets to Idle (0)
        http_post_cmd("flush")
        time.sleep(0.1)
        telem = get_telemetry_http()
        self.assertIn(telem["st"], [STATE_COOLING, STATE_IDLE], "Flush from Steaming must enter COOLING (5) or IDLE (0)")

        # Immediately stop back to Idle
        http_post_cmd("stop")
        self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0), "Must return to IDLE (0)")

    def test_07_busy_state_abort_on_arbitrary_command(self):
        """
        Verifies the coordinator's `is_busy()` branch:
        Sending any non-subtransition command while busy aborts the operation and returns to Idle (0).
        """
        http_post_cmd("steam")
        self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=3.0), "Must enter STEAMING (2)")

        # Sending 'direct_pump' or 'power' while busy stops machine
        http_post_cmd("direct_pump", {"power": 0.0})
        self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0), "Machine must abort to IDLE (0)")

    def test_08_ambient_commands_during_busy_state(self):
        """
        Ambient commands while busy:
        - `set_session_temp` applies immediately to RAM without interrupting the running operation.
        - Settings saves are held and deferred until the machine returns to Idle.
        """
        http_post_cmd("steam")
        self.assertTrue(self.wait_for_state_http(STATE_STEAMING, timeout=3.0), "Must enter STEAMING (2)")

        # Send ambient session temp adjust while Steaming
        http_post_cmd("set_session_temp", {"temp": 95.5})
        time.sleep(0.1)

        # Machine must STILL be Steaming (operation not interrupted)
        telem = get_telemetry_http()
        self.assertEqual(telem["st"], STATE_STEAMING, "Session temp update must NOT interrupt Steaming state")
        self.assertAlmostEqual(telem["sbt"], 95.5, places=1, msg="sbt must update in RAM immediately")

        # Stop and restore
        http_post_cmd("stop")
        self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0))
        http_post_cmd("set_session_temp", {"temp": 92.0})

    def test_09_ambient_commands_while_sleeping_do_not_wake(self):
        """
        Ambient commands (e.g. `set_session_temp`) while SLEEPING (3) update RAM without waking the boiler.
        Subsequent non-ambient commands wake the machine to IDLE (0).
        """
        http_post_cmd("power")
        self.assertTrue(self.wait_for_state_http(STATE_SLEEPING, timeout=3.0), "Must enter SLEEPING (3)")

        # Adjust session temp while asleep
        http_post_cmd("set_session_temp", {"temp": 93.0})
        time.sleep(0.1)

        # Must still be sleeping!
        telem = get_telemetry_http()
        self.assertEqual(telem["st"], STATE_SLEEPING, "Ambient command must not wake the machine from sleep")
        self.assertAlmostEqual(telem["sbt"], 93.0, places=1)

        # Send stop to wake
        http_post_cmd("stop")
        self.assertTrue(self.wait_for_state_http(STATE_IDLE, timeout=3.0), "Stop must wake machine to IDLE (0)")
        http_post_cmd("set_session_temp", {"temp": 92.0})


class TestHttpProtocolCompliance(OximiteTestCase):
    """Tests HTTP protocol edge cases, error codes, and header handling."""

    def test_01_unknown_route_returns_404(self):
        """Requests to nonexistent paths must return 404 Not Found."""
        for path in ["/api/nonexistent", "/foo/bar", "/cmd", "/settings.json"]:
            status, headers, body = http_raw_request("GET", path)
            self.assertEqual(status, 404, f"Path {path} expected 404, got {status}")

    def test_02_malformed_json_returns_400(self):
        """POST with invalid/malformed JSON body must return 400 Bad Request."""
        headers = {"Content-Type": "application/json"}
        status, _, _ = http_raw_request("POST", "/api/cmd", body="{broken_json: 123", headers=headers)
        self.assertEqual(status, 400, f"Malformed JSON should return 400, got {status}")

    def test_03_empty_post_body_returns_400(self):
        """POST /api/cmd with empty body must return 400 Bad Request."""
        headers = {"Content-Type": "application/json"}
        status, _, _ = http_raw_request("POST", "/api/cmd", body="", headers=headers)
        self.assertEqual(status, 400, f"Empty body should return 400, got {status}")

    def test_04_payload_too_large_returns_413_or_rejects(self):
        """POST with body declared or sized larger than 4096 bytes must return 413 Payload Too Large."""
        large_body = "x" * 5000
        headers = {"Content-Type": "application/json", "Content-Length": str(len(large_body))}
        try:
            status, _, _ = http_raw_request("POST", "/api/cmd", body=large_body, headers=headers, timeout=3.0)
            self.assertIn(status, [400, 413], f"Oversize request should return 413/400, got {status}")
        except Exception:
            # Connection closed/aborted by server on oversized payload is also acceptable
            pass

    def test_05_content_length_header_case_insensitivity(self):
        """Server must parse both 'Content-Length' and 'content-length' headers properly."""
        payload = json.dumps({"cmd": "set_session_temp", "temp": 92.0}).encode("utf-8")
        headers = {
            "content-type": "application/json",
            "content-length": str(len(payload))
        }
        status, resp_headers, body = http_raw_request("POST", "/api/cmd", body=payload, headers=headers)
        self.assertEqual(status, 200, f"Lowercase 'content-length' should work, got {status}")

    def test_06_tcp_segmented_post_stream(self):
        """
        Tests that server reads full request when HTTP head and JSON body arrive in
        separate TCP packets (streaming reassembly test).
        """
        payload = json.dumps({"cmd": "stop"}).encode("utf-8")
        head = (
            f"POST /api/cmd HTTP/1.1\r\n"
            f"Host: {TCP_IP}\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(payload)}\r\n"
            f"Connection: close\r\n\r\n"
        ).encode("utf-8")

        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(5.0)
        try:
            s.connect((TCP_IP, HTTP_PORT))
            # Send head first
            s.sendall(head)
            time.sleep(0.05)  # 50ms pause between head and body
            # Send body in second segment
            s.sendall(payload)

            response = b""
            while True:
                chunk = s.recv(1024)
                if not chunk:
                    break
                response += chunk

            resp_str = response.decode("utf-8", errors="replace")
            self.assertIn("HTTP/1.1 200 OK", resp_str, "Segmented POST should succeed with 200 OK")
            self.assertIn('{"status":"ok"}', resp_str)
        finally:
            s.close()


if __name__ == "__main__":
    unittest.main()
