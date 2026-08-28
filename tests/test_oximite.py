import json
import math
import os
import re
import socket
import struct
import threading
import time
import unittest

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

from common import (
    TCP_IP,
    TCP_PORT,
    TIMESTAMP,
    DIAG_VER,
    DIAG_MAGIC,
    DIAG_HEADER_FMT,
    DIAG_HEADER_FIELDS,
    DIAG_HEADER_LEN,
    DIAG_FRAME_FMT,
    DIAG_FRAME_FIELDS,
    DIAG_FRAME_LEN,
    decode_diag_header,
    decode_diag_frame,
    plot_results as common_plot_results,
    save_telemetry_json as common_save_telemetry_json,
    plot_stability_results as common_plot_stability_results,
    plot_step_response as common_plot_step_response,
    plot_pressure_step_response as common_plot_pressure_step_response,
)


class TestOximite(unittest.TestCase):
    telemetry_history = []
    current_state = 0
    sock = None
    sock_file = None
    read_thread = None
    running = True
    parse_errors = 0
    diag_header = None

    @classmethod
    def setUpClass(cls):
        # Create a directory for the saved graphs
        os.makedirs("test_plots", exist_ok=True)
        try:
            cls.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            cls.sock.settimeout(5.0)
            cls.sock.connect((TCP_IP, TCP_PORT))
            cls.read_thread = threading.Thread(target=cls.read_socket)
            cls.read_thread.start()
        except Exception as e:
            print(f"Failed to open socket: {e}")

    @classmethod
    def tearDownClass(cls):
        cls.running = False
        if cls.read_thread:
            cls.read_thread.join()
        if cls.sock:
            cls.sock.close()

    @classmethod
    def recv_exactly(cls, n):
        """Reads exactly `n` bytes, or returns None once the stream ends.

        Records are fixed-size and undelimited, so a short read must be
        completed rather than treated as a record — TCP is free to split a
        55-byte frame across segments.
        """
        buf = bytearray()
        while len(buf) < n and cls.running:
            try:
                chunk = cls.sock.recv(n - len(buf))
            except socket.timeout:
                continue
            except OSError:
                return None
            if not chunk:
                return None
            buf += chunk
        return bytes(buf) if len(buf) == n else None

    @classmethod
    def read_socket(cls):
        raw = cls.recv_exactly(DIAG_HEADER_LEN)
        if raw is None:
            return
        try:
            cls.diag_header = decode_diag_header(raw)
        except ValueError as e:
            print(f"Diagnostic stream rejected: {e}")
            return

        while cls.running:
            raw = cls.recv_exactly(DIAG_FRAME_LEN)
            if raw is None:
                break
            try:
                row = decode_diag_frame(raw)
            except (ValueError, struct.error):
                # Counted, not swallowed: a bad frame means the reader is off
                # a record boundary, which every later sample inherits.
                cls.parse_errors += 1
                continue
            cls.telemetry_history.append(row)
            cls.current_state = row['st']

    def setUp(self):
        self.__class__.telemetry_history.clear()

    @classmethod
    def snapshot_history(cls):
        """Copy of the telemetry collected so far.

        The reader thread keeps appending while a plot is being built, so
        reading the live list more than once yields different lengths and
        matplotlib rejects the frame with "x and y must have same first
        dimension". `list()` copies in a single bytecode, so the snapshot
        cannot tear, and the saved JSON then describes exactly the rows the
        PNG was drawn from.
        """
        return list(cls.telemetry_history)

    def send_command(self, cmd_dict):
        if self.sock:
            msg = json.dumps(cmd_dict) + "\n"
            try:
                self.sock.sendall(msg.encode('utf-8'))
            except Exception as e:
                print(f"Failed to send command: {e}")

    def wait_for_state(self, target_state, timeout=100, title="Operation"):
        """Wait for the machine state to match target_state."""
        start = time.time()
        while self.__class__.current_state != target_state:
            if time.time() - start > timeout:
                print(f"Timeout waiting for {title} state {target_state}! Current: {self.__class__.current_state}")
                self.send_command({"cmd": "stop"})
                return False
            time.sleep(0.1)
        return True

    def run_profile_and_wait(self, profile, max_timeout=150, title="Test"):
        """Helper to send a profile, wait for it to finish, and plot it."""
        print(f"\nRunning: {title}...")
        self.send_command({"cmd": "profile", "profile": profile})
        self.__class__.telemetry_history.clear()

        # Wait for machine to enter BREWING state (1)
        start = time.time()
        while self.__class__.current_state == 0:
            if time.time() - start > 5.0:
                print("Timeout waiting for command to start!")
                break
            time.sleep(0.05)

        self.wait_for_state(0, timeout=max_timeout, title=title)
        self.plot_results(title)
    def plot_results(self, title):
        common_plot_results(self.snapshot_history(), title, self.__class__.diag_header)

    def save_telemetry_json(self, history, safe_title):
        common_save_telemetry_json(history, safe_title, self.__class__.diag_header)

    def plot_stability_results(self, title):
        common_plot_stability_results(self.snapshot_history(), title, self.__class__.diag_header)

    # =========================================================
    # TESTS
    # =========================================================

    def test_01_combo_time_only(self):
        profile = {"name": "Time Only", "steps": [{"time_s": 8.0, "pressure": 6.0}]}
        self.run_profile_and_wait(profile, title="01_Combo_Time_Only")

    def test_02_combo_volume_only(self):
        profile = {"name": "Volume Only", "steps": [{"volume": 36.0, "pressure": 6.0}]}
        self.run_profile_and_wait(profile, title="02_Combo_Volume_Only")

    def test_03_combo_time_or_volume(self):
        profile = {"name": "Time or Volume", "steps": [{"time_s": 10.0, "volume": 30.0, "pressure": 9.0}]}
        self.run_profile_and_wait(profile, title="03_Combo_Time_OR_Volume")

    def test_04_combo_flow_limited(self):
        profile = {"name": "Flow Limited", "steps": [{"time_s": 20.0, "pressure": 9.0, "flow": 2.5}]}
        self.run_profile_and_wait(profile, title="04_Combo_Flow_Limited")

    def test_05_profile_standard_9_bar(self):
        vol = 58.0 + 20.0  # 78ml total
        profile = {"name": "Standard 9 Bar", "steps": [{"volume": 90.0, "pressure": 100.0}]}
        self.run_profile_and_wait(profile, title="05_Profile_Standard_9_Bar100")



    def test_stas_style(self):
        profile = {
            "name": "Stas Style",
            "steps": [
                {"time_s": 5.0, "pressure": 20.0},
                {"volume": 80.0, "pressure": 9.0},
            ],
        }
        self.run_profile_and_wait(profile, title="Default_Style")

    def test_pid_reaction(self):
        print("\nRunning: PID Reaction Test...")
        # # Step 1: Set PID coefficients (temp_pid must be top-level, not nested under "settings")
        # self.send_command({
        #     "cmd": "save_settings",
        #     "temp_pid": {"kp": 6.0, "ki": 0.5, "kd": 30.0},
        # })
        
        # # Step 2: Call stop to activate them
        # self.send_command({"cmd": "stop"})
        # time.sleep(1.0)
        
        # Step 3: Run profile 
        profile = {
            "name": "PID Reaction",
            "steps": [
                {"time_s": 5.0, "pressure": 20.0},
                {"time_s": 3.0},
                {"volume": 80.0, "pressure": 9.0, "flow": 3.0},
            ]
        }
        
        print("Starting profile...")
        # Cleared before the command, not after: the reader thread is already
        # appending, and clearing afterwards discards the first frames of the
        # shot — including the tick where the flow limiter seeds its setpoint
        # from current pressure, which is the moment worth seeing.
        self.__class__.telemetry_history.clear()
        self.send_command({"cmd": "profile", "profile": profile})
        
        # Wait for machine to enter BREWING state (1)
        start = time.time()
        while self.__class__.current_state == 0:
            if time.time() - start > 5.0:
                print("Timeout waiting for command to start!")
                break
            time.sleep(0.05)
            
        # Wait for profile to finish (return to Idle state 0)
        self.wait_for_state(0, timeout=150, title="PID_Reaction_Test")
        
        # Step 4: Record one minute after profile finish
        print("Profile finished. Recording...")
        time.sleep(30.0)
        
        # Step 5: Plot results
        self.plot_results("PID_Reaction_Test")

    def test_sweet_extraction(self):
        """Four-stage declining flow profile — no pressure target at all.

        Sized for 18.5 g of coffee: 80 ml through the machine yields ~40 g in
        the cup. Roughly 40 s of pump time, ~36 s of water/puck contact once
        the headspace has filled.

          1. Pre-infusion 2.5 ml/s, 15 ml  (6 s)  fill headspace, wet the top
          2. Soak        1.0 ml/s, 10 ml (10 s)  capillary down through the bed
          3. Extraction  2.5 ml/s, 35 ml (14 s)  body, crema, sugars
          4. Taper       2.0 ml/s, 20 ml (10 s)  gentle finish, avoid channels

        Stage 3 is capped at 2.5, not the 5.5 ml/s a pump free-flows and not
        the 3.2 tried earlier. Ten bench runs measured the ceiling at 100 %
        duty: 3.07 ml/s at 7-8 bar, 2.51 at 9-10, 2.04 at 10-11. Anything
        above ~2.5 is unreachable against a real puck, and an unreachable
        setpoint doesn't merely run slow — it pins duty at 100 % and the loop
        stops regulating entirely.
        """
        print("\nRunning: Sweet Extraction Test...")

        # `volume` is cumulative across the whole profile — `coordinator::start`
        # zeroes the counter once at profile start, not per step — so these are
        # running totals (15, +10, +35, +20), not per-step amounts.
        # No `time_s`: the stages are defined by volume alone.
        profile = {
            "name": "Sweet Extraction",
            "steps": [
#               {"volume": 10.0, "flow": 2.0},
#               {"volume": 75.0, "flow": 2.5},
#               {"volume": 80.0, "flow": 1.0},
               {"volume": 100.0, "pressure": 9.0, "flow": 3.0},

            ],
        }

        print("Starting profile...")
        self.__class__.telemetry_history.clear()
        self.send_command({"cmd": "profile", "profile": profile})

        # Wait for the machine to leave Idle
        start = time.time()
        while self.__class__.current_state == 0:
            if time.time() - start > 5.0:
                print("Timeout waiting for command to start!")
                break
            time.sleep(0.05)

        self.wait_for_state(0, timeout=150, title="Sweet_Extraction")

        print("Profile finished. Recording tail...")
        time.sleep(10.0)

        self.plot_results("Sweet_Extraction")

    def test_sweet_profile(self):
        profile = {
            "name": "Sweet Profile",
            "steps": [
                {"time_s": 10.0, "pressure": 3.0, "flow": 2.5},
                {"time_s": 15.0, "pressure": 0.0},
                {"volume": 5.5, "pressure": 8.0},
                {"volume": 5.5, "pressure": 7.5},
                {"volume": 5.5, "pressure": 7.0},
                {"volume": 5.5, "pressure": 6.5},
                {"volume": 5.5, "pressure": 6.0},
                {"volume": 5.5, "pressure": 5.5},
                {"volume": 5.5, "pressure": 5.0},
                {"volume": 6.5, "pressure": 4.5},
            ],
        }
        self.run_profile_and_wait(profile, title="Sweet_Profile")



    def test_06_profile_slayer_style(self):
        profile = {
            "name": "Slayer Style",
            "steps": [
                {"time_s": 15.0, "pressure": 9.0, "flow": 2.0},
                {"time_s": 15.0, "pressure": 9.0},
                {"time_s": 10.0, "pressure": 6.0},
            ],
        }
        self.run_profile_and_wait(profile, title="06_Profile_Slayer_Style")

    def test_07_profile_blooming(self):
        profile = {
            "name": "Blooming",
            "steps": [
                {"volume": 15.0, "pressure": 2.0},
                {"time_s": 10.0, "pressure": 0.0},
                {"volume": 130.0, "pressure": 9.0},
            ],
        }
        self.run_profile_and_wait(profile, title="07_Profile_Blooming_Espresso")

    def test_99_pressure_steps(self):
        profile = {
            "name": "Blooming",
            "steps": [
                {"time_s": 5.0, "pressure": 0.0},
                {"time_s": 5.0, "pressure": 9.0},
                {"time_s": 5.0, "pressure": 1.0},
                {"time_s": 5.0, "pressure": 9.0},
            ],
        }
        self.run_profile_and_wait(profile, title="99_pressure_steps")

    def test_12_pump_power_steps(self):
        """Steps pump power by 5% and records sustained flow after 3s."""
        print("\nRunning: Pump Power Steps Test...")
        results = []
        for pwr in range(0, 105, 5):
            # DirectPump only starts from Idle, so stop the previous step first.
            self.send_command({"cmd": "stop"})
            self.wait_for_state(0, timeout=10, title=f"Idle before {pwr}%")
            self.send_command({"cmd": "direct_pump", "power": float(pwr + 0.1)})

            # Wait for flow to stabilize
            time.sleep(3.0)

            # Get latest telemetry
            history = self.snapshot_history()
            if history:
                latest = history[-1]
                flow = latest.get('fl', 0.0)
                pressure = latest.get('p', 0.0)
            else:
                flow = 0.0
                pressure = 0.0

            results.append((pwr, flow, pressure))

        self.send_command({"cmd": "stop"})
        self.wait_for_state(0, timeout=10, title="Stop after steps")

        # Plotting the steps
        powers = [r[0] for r in results]
        flows = [r[1] for r in results]
        pressures = [r[2] for r in results]

        fig, ax1 = plt.subplots(figsize=(10, 6))

        color_f = 'tab:purple'
        ax1.set_xlabel("Pump Power (%)", fontweight='bold')
        ax1.set_ylabel("Sustained Flow (ml/s)", color=color_f, fontweight='bold')
        l1 = ax1.plot(powers, flows, color=color_f, marker='o', label="Flow (ml/s)")
        ax1.tick_params(axis='y', labelcolor=color_f)

        ax2 = ax1.twinx()
        color_p = 'tab:blue'
        ax2.set_ylabel("Sustained Pressure (Bar)", color=color_p, fontweight='bold')
        l2 = ax2.plot(powers, pressures, color=color_p, marker='x', linestyle='--', label="Pressure (Bar)")
        ax2.tick_params(axis='y', labelcolor=color_p)

        lines = l1 + l2
        labels = [l.get_label() for l in lines]
        ax1.legend(lines, labels, loc='upper left')
        ax1.grid(True, alpha=0.3)
        plt.title("Pump Power vs Sustained Flow/Pressure", fontweight='bold', fontsize=14)

        filepath = os.path.join("test_plots", f"{TIMESTAMP}12_Pump_Power_Steps.png")
        plt.savefig(filepath, dpi=150)
        plt.close(fig)
        print(f"Saved plot: {filepath}")

    def test_13_boiler_stability_10min(self):
        """Records boiler temperature for 10 minutes and plots statistics."""
        duration_s = 60
        print(f"\nRunning Boiler Stability Test (duration: {duration_s}s)...")

        # Ensure we are in a state that heats (Idle should be enough)
        # We might want to send a wake command or just ensure we are not sleeping
        self.send_command({"cmd": "stop"})  # This usually resets state to Idle
        time.sleep(1.0)

        self.__class__.telemetry_history.clear()

        start_time = time.time()
        last_print = start_time

        while time.time() - start_time < duration_s:
            time.sleep(1.0)
            now = time.time()
            if now - last_print >= 60:  # Print every minute
                elapsed = now - start_time
                print(f"Recorded {elapsed / 60.0:.0f}/{duration_s / 60.0:.0f} minutes...")
                last_print = now

        self.plot_stability_results("13_Boiler_Stability_15min")

    # def test_08_steam_mode(self):
    #     """Tests steam mode behavior and time limit."""
    #     print("\nRunning: Steam Mode Test...")
    #     # First ensure we have known settings
    #     settings = {
    #         "machine": {"brew_temp": 92.0, "steam_temp": 135.0, "temp_offset": -2.5,
    #         "steam_time_limit_s": 10.0, "sleep_timeout_min": 20.0},
    #         "temp_pid": {"kp": 2.0, "ki": 0.01, "kd": 5.0},
    #         "pump_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }
    #     self.send_command({"cmd": "save_settings", "settings": settings})
    #     time.sleep(0.5)

    #     self.send_command({"cmd": "steam"})
    #     self.__class__.telemetry_history.clear()
    #     # Should finish after steam_time_limit_s (10s)
    #     self.wait_for_state(0, timeout=20, title="Steam Mode")
    #     self.plot_results("08_Steam_Mode")

    # def test_10_safety_timeout(self):
    #     """Verifies that a step with no limits ends at 120s."""
    #     print("\nRunning: Safety Timeout Test...")
    #     # Step with 0 time and 0 volume should still end due to safety timeout
    #     profile = {"name": "Infinite Step", "steps": [{"pressure": 2.0, "time_s": 0.0, "volume": 0.0}]}
    #     # It should finish at exactly 120s
    #     self.run_profile_and_wait(profile, max_timeout=150, title="10_Safety_Timeout")

    # def test_11_save_settings_impact(self):
    #     """Verifies that changing PID coefficients actually updates targets/behavior."""
    #     print("\nRunning: PID Settings Impact Test...")
    #     # Change temp to something high to see it move
    #     settings = {
    #         "machine": {"brew_temp": 98.0, "steam_temp": 135.0, "temp_offset": -2.5,
    #         "steam_time_limit_s": 60.0, "sleep_timeout_min": 20.0},
    #         "temp_pid": {"kp": 5.0, "ki": 0.1, "kd": 10.0},
    #         "pump_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }
    #     self.send_command({"cmd": "save_settings", "settings": settings})
    #     time.sleep(2.0)
    #     history = self.__class__.telemetry_history[-10:]
    #     targets = [d.get('tt', 0) for d in history]
    #     self.assertTrue(all(t == 98.0 for t in targets), f"Target temp should be 98.0, got {targets}")

    #     # Restore defaults
    #     self.send_command({"cmd": "save_settings", "settings": {
    #         "machine": {"brew_temp": 92.0, "steam_temp": 135.0, "temp_offset": -2.5,
    #         "steam_time_limit_s": 120.0, "sleep_timeout_min": 20.0},
    #         "temp_pid": {"kp": 2.0, "ki": 0.01, "kd": 5.0},
    #         "pump_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }})
    #     print("Settings restored.")

    def plot_step_response(self, title):
        common_plot_step_response(self.snapshot_history(), title)

    def test_90_pid_tuning_sweep(self):
        """Automated PID tuning sweep for temperature control."""
        print("\nRunning: PID Tuning Sweep...")

        # Define specific combinations of Kp, Ki, Kd
        pids = [
            [9.0, 0.8, 15.0],
            [10.0, 1.0, 20.0],
            [8.0, 0.2, 5.0],
            [15.0, 0.10, 0.0],
            # [8.0, 0.10, 0.0],
            # [8.0, 0.20, 0.0],
            # [58.397, 0.2054, 1245.275],  # Gaggimate
        ]

        combinations = []
        for pid in pids:
            combinations.append({"kp": pid[0], "ki": pid[1], "kd": pid[2]})

        for pid in combinations:
            title = f"90_PID_Sweep_Kp{pid['kp']}_Ki{pid['ki']}_Kd{pid['kd']}"
            print(f"\n--- Starting {title} ---")

            # Step 1: Set PID, set target temp to 0
            cmd_payload = {
                "cmd": "save_settings",
                "machine": {"brew_temp": 0.0, "steam_temp": 135.0, "steam_time_limit_s": 120.0, "sleep_timeout_min": 20.0},
                "temp_pid": pid,
            }
            self.send_command(cmd_payload)
            self.send_command({"cmd": "stop"})
            time.sleep(1.0)

            # Step 2: Flush to cool down faster
            print("Cooling down boiler with flush...")
            self.send_command({"cmd": "direct_pump", "power": 100.0})
            time.sleep(10.0)
            self.send_command({"cmd": "stop"})
            time.sleep(5.0)  # Wait for reading to settle

            # Step 3: Set target to 90C and log step response
            print("Setting target to 90°C and logging step response...")
            self.__class__.telemetry_history.clear()
            cmd_payload["machine"]["brew_temp"] = 90.0
            self.send_command(cmd_payload)
            self.send_command({"cmd": "stop"})

            # Record telemetry for 120 seconds
            record_time = 120.0
            start_time = time.time()
            while time.time() - start_time < record_time:
                time.sleep(1.0)

            self.plot_step_response(title)

        print("\nSweep Complete.")

    def plot_pressure_step_response(self, title):
        common_plot_pressure_step_response(self.snapshot_history(), title)

    def test_91_pressure_pid_tuning_sweep(self):
        """Automated tuning sweep for the pump PID (blind basket test)."""
        print("\nRunning: Pump PID Tuning Sweep...")

        # Define specific combinations of Kp, Ki, Kd for pressure
        pids = [
            [10.0, 15.0, 0.0],
            [10.0, 20.0, 0.0],
            [10.0, 25.0, 0.0],
        ]

        combinations = []
        for pid in pids:
            combinations.append({"kp": pid[0], "ki": pid[1], "kd": pid[2]})

        for pid in combinations:
            title = f"91_Press_Sweep_Kp{pid['kp']}_Ki{pid['ki']}_Kd{pid['kd']}"
            print(f"\n--- Starting {title} ---")

            # Step 1: Set PID settings
            cmd_payload = {
                "cmd": "save_settings",
                "pump_pid": pid,
            }
            self.send_command(cmd_payload)
            self.send_command({"cmd": "stop"})
            time.sleep(1.0)

            # Step 2: Define a step response profile for the pump
            # Pre-infusion to 4 bar, then jump to 9 bar, then step down to 6 bar
            profile = {
                "name": "Pressure Tuning Step",
                "steps": [
                    {"time_s": 3.0, "pressure": 1.0},
                    {"time_s": 8.0, "pressure": 9.0},
                ]
            }

            print("Executing pressure step profile...")
            self.__class__.telemetry_history.clear()
            
            # Send profile
            self.send_command({"cmd": "profile", "profile": profile})
            
            # Wait for profile to complete (5 + 10 + 5 = 20 seconds total)
            record_time = 12.0
            start_time = time.time()
            while time.time() - start_time < record_time:
                time.sleep(1.0)
                
            self.send_command({"cmd": "stop"})

            self.plot_pressure_step_response(title)

        print("\nPressure Sweep Complete.")


if __name__ == '__main__':
    unittest.main()
