# Control Panel Interface

The control panel features three physical buttons, arranged in the following order:
1. **Flush**
2. **Steam**
3. **Brew**

## Button Functions

### 1. Flush Button
- **First Press:** Activates the flush sequence (direct pump at 30% power).
- **Second Press:** Stops the flush early.
- **Contextual Behavior:** 
  - In **Idle Mode**, the pump activates at 30% power and the 3-way valve is opened to allow water through the brew group.
  - In **Steam Mode**, activates a Cooldown Flush. The pump runs at 30% power and the 3-way valve remains **closed** (routing water through the steam wand). The flush continues automatically until the boiler temperature drops back to the standard brew setting, at which point the machine returns to **Idle**.
  - In **Brewing Mode**, this button is ignored.

### 2. Steam Button
- **First Press:** Enables **Steam Mode**, heating the boiler to the designated steam temperature.
- **Second Press:** Disables Steam Mode and returns the machine to **Idle**, setting the boiler temperature back to the standard brew settings.

### 3. Brew Button
- **First Press:** Starts the default brewing profile.
- **Second Press:** Stops the current brew profile and returns the machine to **Idle**.
- **Contextual Behavior:** This button is ignored if the machine is currently in **Steam Mode**.

## LED Indicators

The interface includes two LED indicators to provide machine status feedback:
- **Temperature LED:** Indicates the boiler status for non-steam (brew) temperatures.
- **Status LED:** Indicates when the machine is in **Steam Mode** or when the flow limit has been reached during brewing.