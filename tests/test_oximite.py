import json
import math
import os
import re
import socket
import threading
import time
import unittest

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import serial

# IMPORTANT: Update these to match your RP2040's networking
TCP_IP = '192.168.1.224'
TCP_PORT = 8080

# IMPORTANT: Update this to match your RP2040's serial port (e.g., 'COM3' or '/dev/ttyACM0')
SERIAL_PORT = 'COM5'
BAUD_RATE = 2_000_000

TIMESTAMP = time.strftime("%Y%m%d_%H%M_")


class TestOximite(unittest.TestCase):
    telemetry_history = []
    current_state = 0
    sock = None
    sock_file = None
    read_thread = None
    running = True

    @classmethod
    def setUpClass(cls):
        # Create a directory for the saved graphs
        os.makedirs("test_plots", exist_ok=True)
        try:
            cls.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            cls.sock.settimeout(5.0)
            cls.sock.connect((TCP_IP, TCP_PORT))
            cls.sock_file = cls.sock.makefile('rw', encoding='utf-8')
            cls.read_thread = threading.Thread(target=cls.read_socket)
            cls.read_thread.start()
        except Exception as e:
            print(f"Failed to open socket: {e}")

    @classmethod
    def tearDownClass(cls):
        cls.running = False
        if cls.read_thread:
            cls.read_thread.join()
        if cls.sock_file:
            cls.sock_file.close()
        if cls.sock:
            cls.sock.close()

    @classmethod
    def read_socket(cls):
        while cls.running:
            try:
                line = cls.sock_file.readline().strip()
                if line:
                    data = json.loads(line)
                    cls.telemetry_history.append(data)
                    cls.current_state = data.get('st', 0)
            except Exception as e:
                time.sleep(0.01)
                pass

    def setUp(self):
        self.__class__.telemetry_history.clear()

    def send_command(self, cmd_dict):
        if self.sock and self.sock_file:
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
        history = self.__class__.telemetry_history
        if not history:
            print(f"No data collected for {title}")
            return

        x = [i * 0.02 for i in range(len(history))]  # 50Hz = 20ms steps
        p = [d.get('p', 0) for d in history]
        tp = [d.get('tp', 0) for d in history]
        vol = [d.get('vol', 0) for d in history]
        fl = [d.get('fl', 0) for d in history]
        t = [d.get('t', 0) for d in history]
        tt = [d.get('tt', 0) for d in history]

        fig, (ax_temp, ax_press) = plt.subplots(2, 1, figsize=(12, 9), sharex=True)

        # --- Top Panel: Temperature ---
        ax_temp.set_title(title.replace('_', ' '), fontweight='bold', fontsize=14)
        ax_temp.set_ylabel("Temperature (°C)", color='tab:red', fontweight='bold')
        ax_temp.plot(x, tt, label="Target Temp", linestyle="--", color='grey')
        ax_temp.plot(x, t, label="Actual Temp", color='tab:red', linewidth=2)
        ax_temp.legend(loc='upper left')
        ax_temp.grid(True, alpha=0.3)

        # --- Bottom Panel: Pressure ---
        color_p = 'tab:blue'
        ax_press.set_xlabel("Time (Seconds)", fontweight='bold')
        ax_press.set_ylabel("Pressure (Bar)", color=color_p, fontweight='bold')
        line1 = ax_press.plot(x, tp, label="Target Pressure", linestyle="--", color='grey')
        line2 = ax_press.plot(x, p, label="Actual Pressure", color=color_p)
        ax_press.tick_params(axis='y', labelcolor=color_p)
        ax_press.set_ylim(bottom=0)

        # --- Bottom Panel: Volume & Flow (Twin Axis) ---
        ax_vol = ax_press.twinx()
        color_v = 'tab:green'
        color_f = 'tab:purple'
        ax_vol.set_ylabel("Volume (ml) / Flow (ml/s)", color=color_v, fontweight='bold')
        line3 = ax_vol.plot(x, vol, label="Accumulated Volume (ml)", color=color_v, linewidth=2)
        line4 = ax_vol.plot(x, fl, label="Flow Rate (ml/s)", color=color_f, linestyle=":", alpha=0.7)
        ax_vol.tick_params(axis='y', labelcolor=color_v)
        ax_vol.set_ylim(bottom=0)

        # Combine legends
        lines = line1 + line2 + line3 + line4
        labels = [l.get_label() for l in lines]
        ax_press.legend(lines, labels, loc='upper left')
        ax_press.grid(True, alpha=0.3)

        fig.tight_layout()

        # --- SAVE TO DISK ---
        safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
        filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.png")
        plt.savefig(filepath, dpi=150)
        plt.close(fig)
        print(f"Saved plot: {filepath}")

        # Save telemetry JSON
        telemetry_filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.json")
        try:
            with open(telemetry_filepath, 'w') as f:
                for rep in history:
                    json.dump(rep, f)
                    f.write("\n")
            print(f"Saved telemetry: {telemetry_filepath}")
        except Exception as e:
            print(f"Failed to save telemetry JSON: {e}")

    def plot_stability_results(self, title):
        history = self.__class__.telemetry_history
        if not history:
            print(f"No data collected for {title}")
            return

        # Extract temperature data
        temps = [d.get('t', 0) for d in history]
        targets = [d.get('tt', 0) for d in history]

        # Use 50Hz sample rate (0.02s)
        times = [i * 0.02 / 60.0 for i in range(len(temps))]  # In minutes

        # Calculate statistics
        mean_t = sum(temps) / len(temps)
        min_t = min(temps)
        max_t = max(temps)
        variance = sum((x - mean_t) ** 2 for x in temps) / len(temps)
        std_dev = math.sqrt(variance)

        # Create plot
        fig, ax = plt.subplots(figsize=(12, 7))
        ax.plot(times, targets, label="Target Temp (°C)", linestyle="--", color='grey', alpha=0.7)
        ax.plot(times, temps, label="Actual Temp (°C)", color='tab:red', linewidth=1.5)

        ax.set_title(f"Boiler Temperature Stability", fontweight='bold', fontsize=16)
        ax.set_xlabel("Time (Minutes)", fontweight='bold')
        ax.set_ylabel("Temperature (°C)", fontweight='bold')
        ax.grid(True, alpha=0.3)
        ax.legend(loc='upper right')

        # Add statistics text box
        stats_text = (
            f"Statistics:\nMean: {mean_t:.2f}°C\nMin:  {min_t:.2f}°C\nMax:  {max_t:.2f}°C\nStdDev: {std_dev:.3f}°C"
        )
        props = dict(boxstyle='round', facecolor='wheat', alpha=0.5)
        ax.text(
            0.02,
            0.95,
            stats_text,
            transform=ax.transAxes,
            fontsize=12,
            verticalalignment='top',
            bbox=props,
            family='monospace',
        )

        fig.tight_layout()

        # Save to disk
        safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
        filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.png")
        plt.savefig(filepath, dpi=150)
        plt.close(fig)
        print(f"Saved stability plot: {filepath}")

        # Save telemetry JSON
        telemetry_filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.json")
        try:
            with open(telemetry_filepath, 'w') as f:
                for rep in history:
                    json.dump(rep, f)
                    f.write("\n")
            print(f"Saved telemetry: {telemetry_filepath}")
        except Exception as e:
            print(f"Failed to save telemetry JSON: {e}")

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
                {"time_s": 3.0, "pressure": 30.0},
                {"time_s": 2.0, "pressure": 0.0},
                {"volume": 115.0, "pressure": 9.0},
            ],
        }
        self.run_profile_and_wait(profile, title="Default_Style")

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
            self.send_command({"cmd": "direct_pump", "power": float(pwr + 0.1)})

            # Wait for flow to stabilize
            time.sleep(3.0)

            # Get latest telemetry
            history = self.__class__.telemetry_history
            if history:
                latest = history[-1]
                flow = latest.get('fl', 0.0)
                pressure = latest.get('p', 0.0)
            else:
                flow = 0.0
                pressure = 0.0

            print(f"Power: {pwr}%, Flow: {flow:.2f} ml/s, Pressure: {pressure:.2f} bar")
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
        duration_s = 300
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

        self.plot_stability_results("13_Boiler_Stability_steam_5min")

    # def test_08_steam_mode(self):
    #     """Tests steam mode behavior and time limit."""
    #     print("\nRunning: Steam Mode Test...")
    #     # First ensure we have known settings
    #     settings = {
    #         "machine": {"brew_temp": 92.0, "steam_temp": 135.0, "temp_offset": -2.5,
    #         "steam_time_limit_s": 10.0, "steam_pressure": 1.5},
    #         "temp_pid": {"kp": 2.0, "ki": 0.01, "kd": 5.0},
    #         "press_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }
    #     self.send_command({"cmd": "save_settings", "settings": settings})
    #     time.sleep(0.5)

    #     self.send_command({"cmd": "steam"})
    #     self.__class__.telemetry_history.clear()
    #     # Should finish after steam_time_limit_s (10s)
    #     self.wait_for_state(0, timeout=20, title="Steam Mode")
    #     self.plot_results("08_Steam_Mode")

    # def test_09_descale_mode(self):
    #     """Tests descale mode sequence."""
    #     print("\nRunning: Descale Mode Test...")
    #     self.send_command({"cmd": "descale"})
    #     self.__class__.telemetry_history.clear()
    #     # Descale takes a long time (10 min soak), so for HIL test we might want
    #     # to just verify it starts and reaches 60 deg, but here we wait for finish.
    #     # NOTE: In a real HIL test, you'd probably mock the 10min timer or shorten it for tests.
    #     # For this exercise, I'll set a large timeout.
    #     self.wait_for_state(0, timeout=15*60, title="Descale Mode")
    #     self.plot_results("09_Descale_Mode")

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
    #         "steam_time_limit_s": 60.0, "steam_pressure": 1.5},
    #         "temp_pid": {"kp": 5.0, "ki": 0.1, "kd": 10.0},
    #         "press_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }
    #     self.send_command({"cmd": "save_settings", "settings": settings})
    #     time.sleep(2.0)
    #     history = self.__class__.telemetry_history[-10:]
    #     targets = [d.get('tt', 0) for d in history]
    #     self.assertTrue(all(t == 98.0 for t in targets), f"Target temp should be 98.0, got {targets}")

    #     # Restore defaults
    #     self.send_command({"cmd": "save_settings", "settings": {
    #         "machine": {"brew_temp": 92.0, "steam_temp": 135.0, "temp_offset": -2.5,
    #         "steam_time_limit_s": 120.0, "steam_pressure": 1.5},
    #         "temp_pid": {"kp": 2.0, "ki": 0.01, "kd": 5.0},
    #         "press_pid": {"kp": 2.0, "ki": 0.1, "kd": 0.5}
    #     }})
    #     print("Settings restored.")

    def plot_step_response(self, title):
        history = self.__class__.telemetry_history
        if not history:
            print(f"No data collected for {title}")
            return

        temps = [d.get('t', 0) for d in history]
        targets = [d.get('tt', 0) for d in history]
        times = [i * 0.02 for i in range(len(temps))]  # 50Hz = 0.02s

        # Determine index for last 60 seconds (50Hz * 60s = 3000 samples)
        last_60_idx = max(0, len(temps) - 3000)
        times_60 = times[last_60_idx:]
        temps_60 = temps[last_60_idx:]
        targets_60 = targets[last_60_idx:]

        fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 10))

        # Main plot
        ax1.plot(times, targets, label="Target Temp (°C)", linestyle="--", color='grey', alpha=0.7)
        ax1.plot(times, temps, label="Actual Temp (°C)", color='tab:red', linewidth=2)
        ax1.set_title(f"Step Response: {title.replace('_', ' ')}", fontweight='bold', fontsize=14)
        ax1.set_xlabel("Time (Seconds)", fontweight='bold')
        ax1.set_ylabel("Temperature (°C)", fontweight='bold')
        ax1.grid(True, alpha=0.3)
        ax1.legend(loc='lower right')

        # Amplified plot of last 60 seconds
        ax2.plot(times_60, targets_60, label="Target Temp (°C)", linestyle="--", color='grey', alpha=0.7)
        ax2.plot(times_60, temps_60, label="Actual Temp (°C)", color='tab:red', linewidth=2)
        ax2.set_title("Zoomed View: Last 60 Seconds", fontweight='bold', fontsize=12)
        ax2.set_xlabel("Time (Seconds)", fontweight='bold')
        ax2.set_ylabel("Temperature (°C)", fontweight='bold')
        ax2.grid(True, alpha=0.5)

        # Set y-axis limits to zoom in around the target temperature
        if len(targets_60) > 0:
            target_val = targets_60[-1]
            # Zoom to +/- 2 degrees around target, but ensure actual data is visible
            y_min = min(target_val - 2.0, min(temps_60) - 0.5)
            y_max = max(target_val + 2.0, max(temps_60) + 0.5)
            ax2.set_ylim(y_min, y_max)

        ax2.legend(loc='lower right')

        fig.tight_layout()
        safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
        filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.png")
        plt.savefig(filepath, dpi=150)
        plt.close(fig)
        print(f"Saved step response plot: {filepath}")

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
                "machine": {"brew_temp": 0.0, "steam_temp": 135.0, "steam_time_limit_s": 120.0, "steam_pressure": 1.5},
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
        history = self.__class__.telemetry_history
        if not history:
            print(f"No data collected for {title}")
            return

        pressures = [d.get('p', 0) for d in history]
        targets = [d.get('tp', 0) for d in history]
        times = [i * 0.02 for i in range(len(pressures))]  # 50Hz = 0.02s

        fig, ax = plt.subplots(figsize=(10, 5))

        ax.plot(times, targets, label="Target Pressure (Bar)", linestyle="--", color='grey', alpha=0.7)
        ax.plot(times, pressures, label="Actual Pressure (Bar)", color='tab:blue', linewidth=2)
        ax.set_title(f"Step Response: {title.replace('_', ' ')}", fontweight='bold', fontsize=14)
        ax.set_xlabel("Time (Seconds)", fontweight='bold')
        ax.set_ylabel("Pressure (Bar)", fontweight='bold')
        ax.grid(True, alpha=0.3)
        ax.legend(loc='lower right')
        
        ax.set_ylim(bottom=0)

        fig.tight_layout()
        safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
        filepath = os.path.join("test_plots", f"{TIMESTAMP}{safe_title}.png")
        plt.savefig(filepath, dpi=150)
        plt.close(fig)
        print(f"Saved pressure step response plot: {filepath}")

    def test_91_pressure_pid_tuning_sweep(self):
        """Automated PID tuning sweep for pressure control (blind basket test)."""
        print("\nRunning: Pressure PID Tuning Sweep...")

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
                "press_pid": pid,
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
