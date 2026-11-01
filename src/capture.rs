//! Capture spike for the sim-native telemetry rollout, phase 1. Built only with the
//! `capture-spike` feature.
//!
//! Every flight loop, reads the datarefs the app subscribes to through the Web API, plus the
//! sim clock and pause/replay/rate state, and appends them to
//! `<X-Plane>/Output/butterlog/capture_<unix secs>.blc`. `butterlog-wire`'s
//! `measure_capture` example replays the file through the sampler and encoder offline, so
//! rates and quanta can be tuned without flying again.
//!
//! The flight loop neither allocates nor does I/O: each frame is copied into a fixed-size
//! record and handed to a writer thread over a bounded channel. If the writer falls behind,
//! frames are dropped and counted, never waited for.
//!
//! File layout: `BLCAP1\n`, one JSON line `{"channels": [...]}`, then one record per frame:
//! sim time f64, wall time ms i64, flight loop counter i64, then one f64 per channel, all
//! little-endian.

use crate::xplane_log;
use std::ffi::CString;
use std::io::Write;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

/// The app's Web API subscription (`sim_monitor/xplane.rs`), so the capture can be compared
/// with what the app records today, plus the state a sampler needs.
const DATAREFS: &[&str] = &[
    "sim/flightmodel/position/latitude",
    "sim/flightmodel/position/longitude",
    "sim/flightmodel/position/elevation",
    "sim/flightmodel/position/indicated_airspeed",
    "sim/flightmodel/position/groundspeed",
    "sim/flightmodel/position/vh_ind",
    "sim/flightmodel/failures/onground_any",
    "sim/flightmodel/position/y_agl",
    "sim/flightmodel/position/mag_psi",
    "sim/cockpit2/fuel/fuel_quantity",
    "sim/flightmodel/forces/g_nrml",
    "sim/cockpit2/gauges/indicators/altitude_ft_pilot",
    "sim/cockpit/misc/barometer_setting",
    "sim/weather/temperature_ambient_c",
    "sim/flightmodel/position/theta",
    "sim/flightmodel/position/phi",
    "sim/flightmodel/forces/g_side",
    "sim/flightmodel/position/hpath",
    "sim/cockpit2/electrical/bus_volts",
    "sim/cockpit2/electrical/generator_amps",
    "sim/cockpit2/engine/indicators/fuel_flow_kg_sec",
    "sim/cockpit2/engine/indicators/oil_temp_deg_C",
    "sim/cockpit2/engine/indicators/oil_pressure_psi",
    "sim/cockpit2/engine/indicators/MP_in_hg",
    "sim/cockpit2/engine/indicators/engine_speed_rpm",
    "sim/cockpit2/engine/indicators/CHT_deg_C",
    "sim/cockpit2/engine/indicators/EGT_deg_C",
    "sim/cockpit2/autopilot/autopilot_on",
    "sim/cockpit2/autopilot/sync_hold_pitch_deg",
    "sim/cockpit2/autopilot/sync_hold_roll_deg",
    "sim/cockpit2/autopilot/vvi_dial_fpm",
    "sim/flightmodel2/position/ellipsoid_height",
    "sim/flightmodel/position/true_airspeed",
    "sim/cockpit2/radios/actuators/HSI_source_select_pilot",
    "sim/cockpit2/radios/actuators/hsi_obs_deg_mag_pilot",
    "sim/cockpit2/radios/actuators/nav1_frequency_hz",
    "sim/cockpit2/radios/actuators/nav2_frequency_hz",
    "sim/cockpit2/radios/actuators/com1_frequency_hz_833",
    "sim/cockpit2/radios/actuators/com2_frequency_hz_833",
    "sim/flightmodel2/position/pressure_altitude",
    "sim/aircraft/parts/acf_gear_deploy",
    "sim/flightmodel/engine/ENGHP",
    "sim/cockpit2/engine/indicators/ITT_deg_C",
    "sim/cockpit2/radios/indicators/nav1_hdef_dots_pilot",
    "sim/cockpit2/radios/indicators/nav1_vdef_dots_pilot",
    "sim/weather/wind_speed_kt",
    "sim/weather/wind_direction_degt",
    "sim/cockpit2/radios/indicators/gps_dme_distance_nm",
    "sim/cockpit2/radios/indicators/gps_bearing_deg_mag",
    "sim/flightmodel/position/magnetic_variation",
    "sim/cockpit2/autopilot/roll_status",
    "sim/cockpit2/autopilot/pitch_status",
    "sim/cockpit2/pressurization/indicators/cabin_altitude_ft",
    "sim/cockpit2/controls/parking_brake_ratio",
    "sim/cockpit2/switches/navigation_lights_on",
    "sim/cockpit2/switches/beacon_on",
    "sim/cockpit2/switches/strobe_lights_on",
    "sim/cockpit2/switches/taxi_light_on",
    "sim/cockpit2/switches/landing_lights_on",
    // Sampler state: pause, replay, time acceleration.
    "sim/time/paused",
    "sim/operation/prefs/replay_mode",
    "sim/time/sim_speed",
    "sim/time/ground_speed",
];

const SIM_TIME: &str = "sim/time/total_running_time_sec";

/// Array datarefs are captured up to this many elements (engines, tanks, buses).
const ARRAY_ELEMENTS: usize = 8;
const MAX_VALUES: usize = 256;
/// About 17 s at 60 fps before frames are dropped.
const QUEUE_FRAMES: usize = 1024;

#[derive(Clone, Copy)]
enum Read {
    Double,
    Float,
    Int,
    FloatArray(usize),
    IntArray(usize),
}

struct Slot {
    dataref: xplm_sys::XPLMDataRef,
    read: Read,
}

#[derive(Clone, Copy)]
struct Frame {
    sim_time: f64,
    wall_ms: i64,
    counter: i64,
    values: [f64; MAX_VALUES],
}

pub struct Capture {
    slots: Vec<Slot>,
    n_values: usize,
    sim_time: xplm_sys::XPLMDataRef,
    /// `total_running_time_sec` is a float; `XPLMGetDatad` reads 0 from it.
    sim_time_is_double: bool,
    tx: Option<SyncSender<Frame>>,
    writer: Option<JoinHandle<()>>,
    frames: u64,
    dropped: u64,
    path: String,
}

fn find(name: &str) -> xplm_sys::XPLMDataRef {
    let c = CString::new(name).expect("dataref names have no NUL");
    unsafe { xplm_sys::XPLMFindDataRef(c.as_ptr()) }
}

impl Capture {
    /// Resolves the datarefs and starts the writer. Main thread only.
    pub fn start(system_path: &str) -> Option<Self> {
        let sim_time = find(SIM_TIME);
        if sim_time.is_null() {
            xplane_log("Capture: sim time dataref not found; not capturing.");
            return None;
        }
        let sim_time_is_double = unsafe { xplm_sys::XPLMGetDataRefTypes(sim_time) } as u32
            & xplm_sys::xplmType_Double
            != 0;
        let mut slots = Vec::new();
        let mut names = Vec::new();
        for name in DATAREFS {
            let dataref = find(name);
            if dataref.is_null() {
                xplane_log(&format!("Capture: dataref not found, skipped: {name}"));
                continue;
            }
            let types = unsafe { xplm_sys::XPLMGetDataRefTypes(dataref) } as u32;
            let has = |t: u32| types & t != 0;
            let array_len = |size: i32| (size.max(0) as usize).min(ARRAY_ELEMENTS);
            let read = if has(xplm_sys::xplmType_Double) {
                Read::Double
            } else if has(xplm_sys::xplmType_Float) {
                Read::Float
            } else if has(xplm_sys::xplmType_Int) {
                Read::Int
            } else if has(xplm_sys::xplmType_FloatArray) {
                let size = unsafe { xplm_sys::XPLMGetDatavf(dataref, std::ptr::null_mut(), 0, 0) };
                Read::FloatArray(array_len(size))
            } else if has(xplm_sys::xplmType_IntArray) {
                let size = unsafe { xplm_sys::XPLMGetDatavi(dataref, std::ptr::null_mut(), 0, 0) };
                Read::IntArray(array_len(size))
            } else {
                xplane_log(&format!("Capture: unsupported dataref type, skipped: {name}"));
                continue;
            };
            match read {
                Read::FloatArray(n) | Read::IntArray(n) => {
                    names.extend((0..n).map(|i| format!("{name}[{i}]")));
                }
                _ => names.push((*name).to_string()),
            }
            slots.push(Slot { dataref, read });
        }
        let n_values = names.len().min(MAX_VALUES);
        names.truncate(n_values);

        let dir = format!("{system_path}Output/butterlog");
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let path = format!("{dir}/capture_{secs}.blc");
        let file = std::fs::create_dir_all(&dir).and_then(|_| std::fs::File::create(&path));
        let mut out = match file {
            Ok(f) => std::io::BufWriter::with_capacity(1 << 20, f),
            Err(e) => {
                xplane_log(&format!("Capture: cannot create {path}: {e}; not capturing."));
                return None;
            }
        };
        let header = serde_json::json!({ "channels": names });
        let written = out
            .write_all(b"BLCAP1\n")
            .and_then(|_| out.write_all(format!("{header}\n").as_bytes()));
        if let Err(e) = written {
            xplane_log(&format!("Capture: cannot write {path}: {e}; not capturing."));
            return None;
        }

        let (tx, rx) = sync_channel::<Frame>(QUEUE_FRAMES);
        let writer = std::thread::spawn(move || {
            let mut since_flush = 0u32;
            let mut record = Vec::with_capacity(24 + 8 * n_values);
            for frame in rx {
                record.clear();
                record.extend_from_slice(&frame.sim_time.to_le_bytes());
                record.extend_from_slice(&frame.wall_ms.to_le_bytes());
                record.extend_from_slice(&frame.counter.to_le_bytes());
                for v in &frame.values[..n_values] {
                    record.extend_from_slice(&v.to_le_bytes());
                }
                if out.write_all(&record).is_err() {
                    return;
                }
                since_flush += 1;
                if since_flush >= 300 {
                    since_flush = 0;
                    let _ = out.flush();
                }
            }
            let _ = out.flush();
        });

        xplane_log(&format!("Capture: recording {n_values} values per frame to {path}"));
        Some(Self {
            slots,
            n_values,
            sim_time,
            sim_time_is_double,
            tx: Some(tx),
            writer: Some(writer),
            frames: 0,
            dropped: 0,
            path,
        })
    }

    /// Reads every dataref once. Called from the flight loop: no allocation, no I/O.
    pub fn sample(&mut self, counter: i32) {
        let Some(tx) = &self.tx else { return };
        let mut frame = Frame {
            sim_time: if self.sim_time_is_double {
                unsafe { xplm_sys::XPLMGetDatad(self.sim_time) }
            } else {
                unsafe { xplm_sys::XPLMGetDataf(self.sim_time) }.into()
            },
            wall_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
            counter: counter.into(),
            values: [0.0; MAX_VALUES],
        };
        let mut at = 0;
        for slot in &self.slots {
            if at >= self.n_values {
                break;
            }
            let d = slot.dataref;
            match slot.read {
                Read::Double => {
                    frame.values[at] = unsafe { xplm_sys::XPLMGetDatad(d) };
                    at += 1;
                }
                Read::Float => {
                    frame.values[at] = unsafe { xplm_sys::XPLMGetDataf(d) }.into();
                    at += 1;
                }
                Read::Int => {
                    frame.values[at] = unsafe { xplm_sys::XPLMGetDatai(d) }.into();
                    at += 1;
                }
                Read::FloatArray(n) => {
                    let mut buf = [0f32; ARRAY_ELEMENTS];
                    unsafe { xplm_sys::XPLMGetDatavf(d, buf.as_mut_ptr(), 0, n as i32) };
                    for v in &buf[..n.min(self.n_values - at)] {
                        frame.values[at] = (*v).into();
                        at += 1;
                    }
                }
                Read::IntArray(n) => {
                    let mut buf = [0i32; ARRAY_ELEMENTS];
                    unsafe { xplm_sys::XPLMGetDatavi(d, buf.as_mut_ptr(), 0, n as i32) };
                    for v in &buf[..n.min(self.n_values - at)] {
                        frame.values[at] = (*v).into();
                        at += 1;
                    }
                }
            }
        }
        self.frames += 1;
        match tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.dropped += 1,
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        xplane_log(&format!(
            "Capture: stopped; {} frames, {} dropped, in {}",
            self.frames,
            self.dropped,
            self.path
        ));
    }
}
