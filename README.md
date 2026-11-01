# Butterlog X-Plane 12 Plugin (`butterlog_xp_plugin`)

This is a high-performance X-Plane 12 plugin written in Rust that listens to a UDP port and proxies multiplayer/AI aircraft in the simulator using the modern `XPLMInstance` API. It parses and supports standard CSL (Common Shape Library) package configurations to match 3D aircraft models and liveries.

## Features

- **Asynchronous UDP Listener**: Runs on a background thread to prevent simulator stuttering.
- **Main-Thread Queue Processing**: Dequeues updates and controls the `XPLMInstance` lifecycle entirely on X-Plane's main flight loop thread.
- **CSL Support**: Scans and parses `xsb_aircraft.txt` definitions. Supports matching by:
  1. `LIVERY` (ICAO + Airline + Livery name)
  2. `AIRLINE` (ICAO + Airline code)
  3. `ICAO` (Aircraft type code)
  4. Partial ICAO prefix fallback (e.g. matching `B73` for `B738`)
  5. Direct fallback to any loaded model.
- **Automatic Cleanup**: Automatically destroys instances of aircraft if no updates are received for 10 seconds, or immediately upon a `"remove": true` command.
- **Telemetry Link**: Streams the datarefs the ButterLog app asks for, every sim frame and stamped with the sim clock, over TCP on `127.0.0.1:49021`. The app records them as sim-native telemetry (`capture: "xp_plugin"`). The protocol is described in `src/link.rs`.
- **Low Memory Footprint**: Caches loaded `.obj` files in memory to avoid redundant file loads.

## Installation

1. Create a folder named `butterlog_xp_plugin` inside your X-Plane installation directory:
   `X-Plane 12/Resources/plugins/butterlog_xp_plugin/`
2. Inside that directory, create a `64` subfolder:
   `X-Plane 12/Resources/plugins/butterlog_xp_plugin/64/`
3. Copy the compiled plugin library (`butterlog_xp_plugin.dll` on Windows, `.so` on Linux, `.dylib` on macOS) to the `64` directory and rename it:
   - **Windows**: Rename `butterlog_xp_plugin.dll` to `win.xpl`
   - **Linux**: Rename `libbutterlog_xp_plugin.so` to `lin.xpl`
   - **macOS**: Rename `libbutterlog_xp_plugin.dylib` to `mac.xpl`
4. CSL models can be loaded from two paths:
   - The plugin's own CSL folder: `X-Plane 12/Resources/plugins/butterlog_xp_plugin/CSL/`
   - X-Plane's global CSL folder: `X-Plane 12/Resources/CSL/`

## UDP Network Protocol

The plugin listens on UDP port `49020`. It expects JSON packets representing updates for individual aircraft.

### Aircraft Update Schema

```json
{
  "id": "unique_aircraft_id",
  "icao": "A320",
  "airline": "DLH",
  "livery": "Retro",
  "latitude": 47.4582,
  "longitude": 8.5555,
  "elevation": 1417.3,
  "pitch": 4.2,
  "heading": 280.0,
  "roll": -1.5,
  "remove": false
}
```

### Fields

- `id` (string, required): A unique key identifying the aircraft (e.g., callsign or index).
- `icao` (string, required): The ICAO type designator (e.g., `"A320"`, `"B738"`).
- `airline` (string, optional): The 3-letter ICAO airline code (e.g., `"DLH"`, `"BAW"`).
- `livery` (string, optional): Specific livery name.
- `latitude` (f64, required): Geographic latitude in degrees.
- `longitude` (f64, required): Geographic longitude in degrees.
- `elevation` (f64, required): MSL altitude in **meters**.
- `pitch` (f32, required): Pitch angle in degrees (positive up).
- `heading` (f32, required): True heading in degrees.
- `roll` (f32, required): Roll angle in degrees (positive right/clockwise).
- `remove` (bool, optional): If `true`, the instance is immediately destroyed.

## Building from Source

To rebuild the plugin:

```bash
cd butterlog_xp_plugin
cargo build --release
```

The compiled library will be generated at `target/release/butterlog_xp_plugin.dll`.
