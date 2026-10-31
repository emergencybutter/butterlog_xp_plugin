use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::raw::c_char;
use std::path::Path;
use serde::{Deserialize, Serialize};

#[cfg(feature = "capture-spike")]
mod capture;

// Animation datarefs registered on each instanced CSL model so X-Plane animates
// them. CSL models (Bluebell/XCSL) read the `libxplanemp/controls/*` family.
// Order here must match the `dref_vals` array built in the update handler below.
const INSTANCE_DATAREFS: &[&[u8]] = &[
    b"libxplanemp/controls/gear_ratio\0",
    b"libxplanemp/controls/nav_lites_on\0",
    b"libxplanemp/controls/beacon_lites_on\0",
    b"libxplanemp/controls/strobe_lites_on\0",
    b"libxplanemp/controls/taxi_lites_on\0",
    b"libxplanemp/controls/landing_lites_on\0",
];

// Standard XPLM debug log utility
fn xplane_log(msg: &str) {
    let formatted = format!("[ButterlogXP] {}\n", msg);
    if let Ok(c_str) = CString::new(formatted) {
        unsafe {
            xplm_sys::XPLMDebugString(c_str.as_ptr());
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AircraftUpdate {
    pub id: String,
    pub icao: String,
    pub airline: Option<String>,
    pub livery: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64, // MSL altitude in meters
    pub pitch: f32,
    pub heading: f32,
    pub roll: f32,
    /// Whether the aircraft is on the ground. Absent from older app versions
    /// (deserializes to `None`).
    pub on_ground: Option<bool>,
    /// Landing-gear deployment 0.0..=1.0 from the sender. Absent from older app
    /// versions (`None`), in which case we fall back to the on-ground flag.
    #[serde(default)]
    pub gear_ratio: Option<f32>,
    /// Exterior light states (0/1). Absent from older app versions (`None` → off).
    #[serde(default)]
    pub nav_lights: Option<f32>,
    #[serde(default)]
    pub beacon_lights: Option<f32>,
    #[serde(default)]
    pub strobe_lights: Option<f32>,
    #[serde(default)]
    pub taxi_lights: Option<f32>,
    #[serde(default)]
    pub landing_lights: Option<f32>,
    pub remove: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AircraftCharacteristic {
    pub icao_code: String,
    pub manufacturer: String,
    pub model_faa: String,
    pub model_bada: String,
    pub engine_type: String, // jet, turboprop, piston, unknown
    pub num_engines: i32,
    pub wtc: String,        // Medium, Heavy, Super, Light
    pub class: String,      // Fixed-wing, Helicopter, Glider, etc.
    pub wingspan: f64,
}

impl AircraftCharacteristic {
    pub fn is_bizjet(&self) -> bool {
        let mfg = self.manufacturer.to_lowercase();
        let model = self.model_faa.to_lowercase();
        mfg.contains("gulfstream") 
            || mfg.contains("learjet") 
            || mfg.contains("dassault")
            || mfg.contains("hawker")
            || (mfg.contains("cessna") && model.contains("citation"))
            || (mfg.contains("bombardier") && (model.contains("global") || model.contains("challenger")))
            || (mfg.contains("embraer") && (model.contains("phenom") || model.contains("legacy") || model.contains("praetor")))
    }

    pub fn is_fighter_jet(&self) -> bool {
        let icao = self.icao_code.to_uppercase();
        let model = self.model_faa.to_lowercase();
        let engine = self.engine_type.to_lowercase();
        
        if engine != "jet" {
            return false;
        }
        
        let is_military_icao = icao.starts_with('F') && icao.chars().nth(1).map_or(false, |c| c.is_ascii_digit())
            || icao == "A10"
            || icao == "T38"
            || icao == "T45"
            || icao == "L39"
            || icao.starts_with("MIG")
            || icao.starts_with("SU")
            || icao.starts_with("MIR")
            || icao.starts_with("EFA")
            || icao.starts_with("JAS");

        is_military_icao
            || model.contains("f-15")
            || model.contains("f-16")
            || model.contains("f-18")
            || model.contains("f-22")
            || model.contains("f-35")
            || model.contains("f/a-18")
            || model.contains("fa-18")
            || model.contains("a-10")
            || model.contains("t-38")
            || model.contains("l-39")
            || model.contains("raptor")
            || model.contains("hornet")
            || model.contains("fighting falcon")
            || model.contains("eagle")
            || model.contains("phantom")
            || model.contains("tomcat")
            || model.contains("harrier")
            || model.contains("talon")
            || model.contains("albatros")
            || model.contains("gripen")
            || model.contains("mirage")
            || model.contains("rafale")
            || model.contains("eurofighter")
            || model.contains("typhoon")
            || model.contains("sukhoi")
            || model.contains("mikoyan")
    }
}

pub struct CharacteristicsDatabase {
    pub characteristics: HashMap<String, AircraftCharacteristic>,
}

impl CharacteristicsDatabase {
    pub fn load_from_csv<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        let mut reader = csv::Reader::from_reader(file);
        let mut map = HashMap::new();
        
        let headers = reader.headers()?.clone();
        
        let icao_idx = headers.iter().position(|h| h == "ICAO_Code");
        let mfg_idx = headers.iter().position(|h| h == "Manufacturer");
        let model_faa_idx = headers.iter().position(|h| h == "Model_FAA");
        let model_bada_idx = headers.iter().position(|h| h == "Model_BADA");
        let engine_type_idx = headers.iter().position(|h| h == "Physical_Class_Engine");
        let num_engines_idx = headers.iter().position(|h| h == "Num_Engines");
        let wtc_idx = headers.iter().position(|h| h == "ICAO_WTC");
        let class_idx = headers.iter().position(|h| h == "Class");

        let wingspan_idx = headers.iter().position(|h| h == "Wingspan_ft_without_winglets_sharklets");

        if let (Some(icao_i), Some(mfg_i), Some(model_faa_i), Some(model_bada_i), Some(engine_type_i), Some(num_engines_i), Some(wtc_i), Some(class_i), Some(wingspan_i)) = 
            (icao_idx, mfg_idx, model_faa_idx, model_bada_idx, engine_type_idx, num_engines_idx, wtc_idx, class_idx, wingspan_idx) {
            for result in reader.records() {
                if let Ok(record) = result {
                    if let Some(icao) = record.get(icao_i) {
                        let icao_upper = icao.to_uppercase();
                        let manufacturer = record.get(mfg_i).unwrap_or("").trim().to_string();
                        let model_faa = record.get(model_faa_i).unwrap_or("").trim().to_string();
                        let model_bada = record.get(model_bada_i).unwrap_or("").trim().to_string();
                        let engine_type = record.get(engine_type_i).unwrap_or("").trim().to_lowercase();
                        let num_engines = record.get(num_engines_i).unwrap_or("0").parse::<i32>().unwrap_or(0);
                        let wtc = record.get(wtc_i).unwrap_or("").trim().to_string();
                        let class = record.get(class_i).unwrap_or("").trim().to_string();
                        let wingspan = record.get(wingspan_i).unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                        
                        map.insert(icao_upper.clone(), AircraftCharacteristic {
                            icao_code: icao_upper,
                            manufacturer,
                            model_faa,
                            model_bada,
                            engine_type,
                            num_engines,
                            wtc,
                            class,
                            wingspan,
                        });
                    }
                }
            }
        }
        Ok(Self { characteristics: map })
    }
}

fn calculate_similarity_score(a: &AircraftCharacteristic, b: &AircraftCharacteristic) -> i32 {
    let mut score = 0;
    
    if a.class.to_lowercase() == b.class.to_lowercase() {
        score += 100;
    }
    
    if !a.manufacturer.is_empty() && !b.manufacturer.is_empty() && a.manufacturer.eq_ignore_ascii_case(&b.manufacturer) {
        score += 20;
    }
    
    if a.engine_type.to_lowercase() == b.engine_type.to_lowercase() {
        score += 50;
    }
    
    if a.wtc.to_lowercase() == b.wtc.to_lowercase() {
        score += 30;
    }
    
    if a.num_engines == b.num_engines {
        score += 10;
    } else if (a.num_engines - b.num_engines).abs() == 1 {
        score += 5;
    }
    
    // Categorize business jets
    if a.is_bizjet() == b.is_bizjet() {
        score += 50;
    }

    // Categorize fighter jets
    if a.is_fighter_jet() == b.is_fighter_jet() {
        score += 50;
    }

    // Compare wingspans (proxy for size)
    if a.wingspan > 0.0 && b.wingspan > 0.0 {
        let diff = (a.wingspan - b.wingspan).abs();
        let avg = (a.wingspan + b.wingspan) / 2.0;
        let rel_diff = diff / avg;
        if rel_diff < 0.1 {
            score += 40;
        } else if rel_diff < 0.25 {
            score += 20;
        } else if rel_diff < 0.5 {
            score += 10;
        }
    }
    
    score
}

pub struct CslModel {
    pub model_id: String,
    pub obj_path: String, // Path relative to X-Plane system path
    /// Meters to raise the model so its wheels sit on the ground (CSL VERT_OFFSET).
    pub vert_offset: f32,
}

pub struct CslDatabase {
    pub models: HashMap<String, CslModel>, // model_id -> CslModel
    pub livery_map: HashMap<(String, String, String), String>, // (icao, airline, livery) -> model_id
    pub airline_map: HashMap<(String, String), String>, // (icao, airline) -> model_id
    pub icao_map: HashMap<String, String>, // icao -> model_id
}

pub struct ActiveAircraft {
    pub instance: xplm_sys::XPLMInstanceRef,
    pub object: xplm_sys::XPLMObjectRef,
    pub last_updated: std::time::Instant,
    pub last_logged_update: std::time::Instant,
}

pub struct PluginState {
    pub rx: std::sync::mpsc::Receiver<AircraftUpdate>,
    pub active_aircraft: HashMap<String, ActiveAircraft>,
    pub object_cache: HashMap<String, xplm_sys::XPLMObjectRef>,
    pub csl_db: CslDatabase,
    pub system_path: String,
    pub chars_db: Option<CharacteristicsDatabase>,
    /// Reusable terrain probe for ground-clamping on-ground aircraft. Must only be used
    /// from the main thread (the flight-loop callback).
    pub terrain_probe: xplm_sys::XPLMProbeRef,
    #[cfg(feature = "capture-spike")]
    pub capture: Option<capture::Capture>,
}

/// Probe the terrain Y (local OpenGL vertical) directly below/above the given local point.
/// Returns `None` if the probe didn't hit terrain. Main thread only.
fn probe_terrain_y(probe: xplm_sys::XPLMProbeRef, x: f32, y: f32, z: f32) -> Option<f32> {
    if probe.is_null() {
        return None;
    }
    let mut info = xplm_sys::XPLMProbeInfo_t {
        structSize: std::mem::size_of::<xplm_sys::XPLMProbeInfo_t>() as std::os::raw::c_int,
        ..unsafe { std::mem::zeroed() }
    };
    let result = unsafe { xplm_sys::XPLMProbeTerrainXYZ(probe, x, y, z, &mut info) };
    if result == xplm_sys::xplm_ProbeHitTerrain as xplm_sys::XPLMProbeResult {
        Some(info.locationY)
    } else {
        None
    }
}

// Retrieves the X-Plane system path
fn get_system_path() -> String {
    unsafe {
        let mut path_buf = [0 as c_char; 1024];
        xplm_sys::XPLMGetSystemPath(path_buf.as_mut_ptr());
        std::ffi::CStr::from_ptr(path_buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

// Retrieves the plugin's base directory
fn get_plugin_directory() -> Option<String> {
    unsafe {
        let my_id = xplm_sys::XPLMGetMyID();
        let mut path_buf = [0 as c_char; 1024];
        xplm_sys::XPLMGetPluginInfo(
            my_id,
            std::ptr::null_mut(),
            path_buf.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let path_str = std::ffi::CStr::from_ptr(path_buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        
        let path = Path::new(&path_str);
        // Path is usually: <PluginDir>/64/win.xpl
        // Parent is <PluginDir>/64, and its parent is <PluginDir>
        path.parent()?.parent()?.to_str().map(|s| s.to_string())
    }
}

// Recursively scans a directory for xsb_aircraft.txt files and parses them
fn scan_csl_dir(csl_path: &str, system_path: &str, db: &mut CslDatabase) {
    let path = Path::new(csl_path);
    if !path.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let filename = entry.file_name().to_string_lossy();
            if filename.to_lowercase() == "xsb_aircraft.txt" {
                xplane_log(&format!("Parsing CSL metadata from: {}", entry.path().display()));
                parse_xsb_aircraft(entry.path(), system_path, db);
            }
        }
    }
}

fn resolve_obj_path(parent_dir: &Path, rel_path: &str) -> std::path::PathBuf {
    let cleaned_path = rel_path.replace(':', "/");
    let try_direct = parent_dir.join(&cleaned_path);
    if try_direct.exists() {
        return try_direct;
    }

    if let Some(pos) = cleaned_path.find('/') {
        let first_part = &cleaned_path[..pos];
        let rest = &cleaned_path[pos + 1..];
        
        if parent_dir.ends_with(first_part) {
            let try_stripped = parent_dir.join(rest);
            if try_stripped.exists() {
                return try_stripped;
            }
        }
        
        let try_stripped_general = parent_dir.join(rest);
        if try_stripped_general.exists() {
            return try_stripped_general;
        }
    }

    try_direct
}

// Parses a single xsb_aircraft.txt CSL package file
fn parse_xsb_aircraft(file_path: &Path, system_path: &str, db: &mut CslDatabase) {
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    let parent_dir = match file_path.parent() {
        Some(p) => p,
        None => return,
    };

    let mut current_model_id: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() || line_trimmed.starts_with('#') || line_trimmed.starts_with("//") {
            continue;
        }

        let tokens: Vec<&str> = line_trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        let cmd = tokens[0].to_uppercase();
        match cmd.as_str() {
            "OBJ8_AIRCRAFT" => {
                if tokens.len() >= 2 {
                    current_model_id = Some(tokens[1].to_string());
                }
            }
            "OBJ8" => {
                if let Some(ref model_id) = current_model_id {
                    if tokens.len() >= 4 {
                        let abs_obj_path = resolve_obj_path(parent_dir, tokens[3]);
                        
                        let rel_path_str = if let Ok(canon_obj) = abs_obj_path.canonicalize() {
                            let system_path_canon = Path::new(system_path).canonicalize().unwrap_or_else(|_| Path::new(system_path).to_path_buf());
                            if let Ok(rel_to_sys) = canon_obj.strip_prefix(&system_path_canon) {
                                rel_to_sys.to_string_lossy().replace('\\', "/")
                            } else {
                                let abs_str = canon_obj.to_string_lossy().replace('\\', "/");
                                let sys_str = system_path_canon.to_string_lossy().replace('\\', "/");
                                if abs_str.starts_with(&sys_str) {
                                    abs_str[sys_str.len()..].trim_start_matches('/').to_string()
                                } else {
                                    abs_str
                                }
                            }
                        } else {
                            let abs_obj_path_str = abs_obj_path.to_string_lossy().replace('\\', "/");
                            let sys_str = system_path.replace('\\', "/");
                            if abs_obj_path_str.starts_with(&sys_str) {
                                abs_obj_path_str[sys_str.len()..].trim_start_matches('/').to_string()
                            } else {
                                abs_obj_path_str
                            }
                        };

                        xplane_log(&format!("Discovered CSL model: {} -> {}", model_id, rel_path_str));
                        db.models.insert(
                            model_id.clone(),
                            CslModel {
                                model_id: model_id.clone(),
                                obj_path: rel_path_str,
                                vert_offset: 0.0,
                            },
                        );
                    }
                }
            }
            "VERT_OFFSET" => {
                // Meters to raise the model so its wheels touch the ground. Appears
                // after the model's OBJ8 line, so the model is already in db.models.
                if let Some(ref model_id) = current_model_id {
                    if let Some(v) = tokens.get(1).and_then(|t| t.parse::<f32>().ok()) {
                        if let Some(m) = db.models.get_mut(model_id) {
                            m.vert_offset = v;
                        }
                    }
                }
            }
            "ICAO" => {
                if let Some(ref model_id) = current_model_id {
                    if tokens.len() >= 2 {
                        let icao = tokens[1].to_uppercase();
                        db.icao_map.insert(icao, model_id.clone());
                    }
                }
            }
            "AIRLINE" => {
                if let Some(ref model_id) = current_model_id {
                    if tokens.len() >= 3 {
                        let icao = tokens[1].to_uppercase();
                        let airline = tokens[2].to_uppercase();
                        db.airline_map.insert((icao, airline), model_id.clone());
                    }
                }
            }
            "LIVERY" => {
                if let Some(ref model_id) = current_model_id {
                    if tokens.len() >= 4 {
                        let icao = tokens[1].to_uppercase();
                        let airline = tokens[2].to_uppercase();
                        let livery = tokens[3].to_uppercase();
                        db.livery_map.insert((icao, airline, livery), model_id.clone());
                    } else if tokens.len() == 3 {
                        let icao = tokens[1].to_uppercase();
                        let livery = tokens[2].to_uppercase();
                        db.livery_map.insert((icao, String::new(), livery), model_id.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

// Processes a single parsed update on the main thread
fn process_update(state: &mut PluginState, update: AircraftUpdate) {
    if let Some(true) = update.remove {
        if let Some(aircraft) = state.active_aircraft.remove(&update.id) {
            unsafe {
                xplm_sys::XPLMDestroyInstance(aircraft.instance);
            }
            xplane_log(&format!("Removed aircraft instance: {}", update.id));
        }
        return;
    }

    // Lookup best model
    let mut model_id_opt = match (&update.icao, &update.airline, &update.livery) {
        (icao, Some(airline), Some(livery)) => {
            state.csl_db.livery_map.get(&(icao.to_uppercase(), airline.to_uppercase(), livery.to_uppercase()))
                .or_else(|| state.csl_db.airline_map.get(&(icao.to_uppercase(), airline.to_uppercase())))
                .or_else(|| state.csl_db.icao_map.get(&icao.to_uppercase()))
        }
        (icao, Some(airline), None) => {
            state.csl_db.airline_map.get(&(icao.to_uppercase(), airline.to_uppercase()))
                .or_else(|| state.csl_db.icao_map.get(&icao.to_uppercase()))
        }
        (icao, None, Some(livery)) => {
            state.csl_db.livery_map.get(&(icao.to_uppercase(), String::new(), livery.to_uppercase()))
                .or_else(|| state.csl_db.icao_map.get(&icao.to_uppercase()))
        }
        (icao, None, None) => {
            state.csl_db.icao_map.get(&icao.to_uppercase())
        }
    }.cloned();

    // Similarity matching via characteristics DB if exact ICAO is not present
    if model_id_opt.is_none() {
        let icao_upper = update.icao.to_uppercase();
        if let Some(ref chars_db) = state.chars_db {
            if let Some(r_char) = chars_db.characteristics.get(&icao_upper) {
                let mut best_score = -1;
                let mut best_icao = None;
                for available_icao in state.csl_db.icao_map.keys() {
                    if let Some(l_char) = chars_db.characteristics.get(available_icao) {
                        let score = calculate_similarity_score(r_char, l_char);
                        if score > best_score {
                            best_score = score;
                            best_icao = Some(available_icao.clone());
                        }
                    }
                }
                if let Some(ref b_icao) = best_icao {
                    model_id_opt = state.csl_db.icao_map.get(b_icao).cloned();
                    xplane_log(&format!(
                        "Similarity match: requested ICAO '{}' matched to available '{}' (score: {})",
                        icao_upper, b_icao, best_score
                    ));
                }
            }
        }
    }

    let model_id = match model_id_opt {
        Some(id) => id,
        None => {
            let icao_upper = update.icao.to_uppercase();
            let mut matched_id = None;
            if icao_upper.len() >= 3 {
                let prefix = &icao_upper[..3];
                for (k, v) in &state.csl_db.icao_map {
                    if k.starts_with(prefix) {
                        matched_id = Some(v.clone());
                        break;
                    }
                }
            }
            if let Some(id) = matched_id {
                id
            } else {
                if let Some(first_model_id) = state.csl_db.models.keys().next() {
                    first_model_id.clone()
                } else {
                    return; // No models at all
                }
            }
        }
    };

    let csl_model = match state.csl_db.models.get(&model_id) {
        Some(m) => m,
        None => return,
    };

    let vert_offset = csl_model.vert_offset as f64;
    let obj_path = &csl_model.obj_path;
    let object_ref = if let Some(&obj) = state.object_cache.get(obj_path) {
        obj
    } else {
        let path_c = match CString::new(obj_path.clone()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let obj = unsafe { xplm_sys::XPLMLoadObject(path_c.as_ptr()) };
        if obj.is_null() {
            xplane_log(&format!("ERROR: Failed to load OBJ: {}", obj_path));
            return;
        }
        state.object_cache.insert(obj_path.clone(), obj);
        xplane_log(&format!("Loaded OBJ file: {}", obj_path));
        obj
    };

    // Convert GPS coords to local scene coordinates
    let mut x: f64 = 0.0;
    let mut y: f64 = 0.0;
    let mut z: f64 = 0.0;
    unsafe {
        xplm_sys::XPLMWorldToLocal(
            update.latitude,
            update.longitude,
            update.elevation,
            &mut x,
            &mut y,
            &mut z,
        );
    }

    // When the peer reports it is on the ground, clamp the vertical position to the local
    // terrain instead of trusting the transmitted MSL altitude: the sender's MSL and the
    // receiver's terrain mesh rarely agree exactly, which otherwise leaves the aircraft
    // floating above or sunk into the runway. Add the model's VERT_OFFSET (origin-to-wheel
    // distance) so it sits exactly on the surface rather than the origin being buried.
    let y = if update.on_ground == Some(true) {
        probe_terrain_y(state.terrain_probe, x as f32, y as f32, z as f32)
            .map(|ty| ty as f64 + vert_offset)
            .unwrap_or(y)
    } else {
        y
    };

    let draw_info = xplm_sys::XPLMDrawInfo_t {
        structSize: std::mem::size_of::<xplm_sys::XPLMDrawInfo_t>() as i32,
        x: x as f32,
        y: y as f32,
        z: z as f32,
        pitch: update.pitch,
        heading: update.heading,
        roll: update.roll,
    };

    // Animation datarefs to drive on the instance: gear down when on the ground,
    // up otherwise. Names are registered at XPLMCreateInstance (null-terminated);
    // values are fed every frame to XPLMInstanceSetPosition (same order/length).
    // Null-terminated dataref name list for XPLMCreateInstance (kept alive for the
    // duration of the create call).
    let mut dref_ptrs: Vec<*const c_char> =
        INSTANCE_DATAREFS.iter().map(|d| d.as_ptr() as *const c_char).collect();
    dref_ptrs.push(std::ptr::null());

    // Prefer the sender's real gear ratio; otherwise fall back to the on-ground
    // flag (covers senders that don't report gear, e.g. older clients).
    let gear = match update.gear_ratio {
        Some(g) if g > 0.0 => g.clamp(0.0, 1.0),
        _ => if update.on_ground == Some(true) { 1.0 } else { 0.0 },
    };
    let light = |v: Option<f32>| if v.unwrap_or(0.0) > 0.5 { 1.0f32 } else { 0.0f32 };
    // Order must match INSTANCE_DATAREFS.
    let dref_vals: [f32; 6] = [
        gear,
        light(update.nav_lights),
        light(update.beacon_lights),
        light(update.strobe_lights),
        light(update.taxi_lights),
        light(update.landing_lights),
    ];

    let now = std::time::Instant::now();
    let mut should_log_pos = false;

    if let Some(aircraft) = state.active_aircraft.get_mut(&update.id) {
        // Recreate instance if model changed
        if aircraft.object != object_ref {
            unsafe {
                xplm_sys::XPLMDestroyInstance(aircraft.instance);
            }
            let instance = unsafe { xplm_sys::XPLMCreateInstance(object_ref, dref_ptrs.as_ptr() as *mut *const c_char) };
            if instance.is_null() {
                state.active_aircraft.remove(&update.id);
                return;
            }
            aircraft.instance = instance;
            aircraft.object = object_ref;
        }
        unsafe {
            xplm_sys::XPLMInstanceSetPosition(aircraft.instance, &draw_info, dref_vals.as_ptr());
        }
        aircraft.last_updated = now;
        if now.duration_since(aircraft.last_logged_update).as_secs() >= 10 {
            aircraft.last_logged_update = now;
            should_log_pos = true;
        }
    } else {
        // Create new instance
        let instance = unsafe { xplm_sys::XPLMCreateInstance(object_ref, dref_ptrs.as_ptr() as *mut *const c_char) };
        if instance.is_null() {
            xplane_log(&format!("ERROR: Failed to create instance for: {}", update.id));
            return;
        }
        unsafe {
            xplm_sys::XPLMInstanceSetPosition(instance, &draw_info, dref_vals.as_ptr());
        }
        xplane_log(&format!("Created aircraft instance: {} (model: {})", update.id, model_id));
        state.active_aircraft.insert(
            update.id.clone(),
            ActiveAircraft {
                instance,
                object: object_ref,
                last_updated: now,
                last_logged_update: now,
            },
        );
        should_log_pos = true;
    }

    if should_log_pos {
        xplane_log(&format!(
            "Received UDP update for proxy aircraft {}: Lat={:.6}, Lon={:.6}, Alt={:.1}m MSL, Hdg={:.1}, OnGround={:?}",
            update.id, update.latitude, update.longitude, update.elevation, update.heading, update.on_ground
        ));
    }
}

// C-compatible flight loop callback called by X-Plane
unsafe extern "C" fn flight_loop_callback(
    _elapsed_since_last_call: f32,
    _elapsed_time_since_last_flight_loop: f32,
    _counter: i32,
    refcon: *mut std::os::raw::c_void,
) -> f32 {
    let state = &mut *(refcon as *mut PluginState);

    #[cfg(feature = "capture-spike")]
    if let Some(capture) = &mut state.capture {
        // A panic must not unwind into X-Plane; stop capturing instead.
        let sampled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture.sample(_counter)
        }));
        if sampled.is_err() {
            xplane_log("Capture: sampling panicked; capture stopped.");
            state.capture = None;
        }
    }
    
    // Dequeue and process all pending UDP updates
    while let Ok(update) = state.rx.try_recv() {
        process_update(state, update);
    }
    
    // Purge inactive/timed out proxy aircraft after 10 seconds of silence
    let now = std::time::Instant::now();
    let mut to_remove = Vec::new();
    for (id, aircraft) in &state.active_aircraft {
        if now.duration_since(aircraft.last_updated).as_secs() > 10 {
            to_remove.push(id.clone());
        }
    }
    for id in to_remove {
        if let Some(aircraft) = state.active_aircraft.remove(&id) {
            xplm_sys::XPLMDestroyInstance(aircraft.instance);
            xplane_log(&format!("Timeout: destroyed instance for aircraft: {}", id));
        }
    }
    
    // -1.0 schedules next call on the very next flight loop cycle
    -1.0
}

struct ButterlogXpPlugin {
    state_ptr: *mut PluginState,
}

impl xplm::plugin::Plugin for ButterlogXpPlugin {
    type Error = std::convert::Infallible;

    fn start() -> Result<Self, Self::Error> {
        xplane_log("Plugin starting...");

        let system_path = get_system_path();
        xplane_log(&format!("System path: {}", system_path));

        let mut csl_db = CslDatabase {
            models: HashMap::new(),
            livery_map: HashMap::new(),
            airline_map: HashMap::new(),
            icao_map: HashMap::new(),
        };

        // Scan the plugin's own CSL folder
        let plugin_dir_opt = get_plugin_directory();
        if let Some(ref plugin_dir) = plugin_dir_opt {
            xplane_log(&format!("Plugin base directory: {}", plugin_dir));
            let plugin_csl = format!("{}/CSL", plugin_dir);
            xplane_log(&format!("Scanning plugin CSL: {}", plugin_csl));
            scan_csl_dir(&plugin_csl, &system_path, &mut csl_db);
        }

        // Scan X-Plane's global CSL folder
        let global_csl = format!("{}Resources/CSL", system_path);
        xplane_log(&format!("Scanning global CSL: {}", global_csl));
        scan_csl_dir(&global_csl, &system_path, &mut csl_db);

        // Scan IVAO CSL folder (commonly used plugin)
        let ivao_csl = format!("{}Resources/plugins/IVAO_CSL/CSL", system_path);
        xplane_log(&format!("Scanning IVAO CSL: {}", ivao_csl));
        scan_csl_dir(&ivao_csl, &system_path, &mut csl_db);

        xplane_log(&format!(
            "CSL Database initialized with: {} models, {} livery maps, {} airline maps, {} ICAO maps",
            csl_db.models.len(),
            csl_db.livery_map.len(),
            csl_db.airline_map.len(),
            csl_db.icao_map.len()
        ));

        // Load aircraft-characteristics.csv database
        let mut chars_db = None;
        if let Some(ref plugin_dir) = plugin_dir_opt {
            let chars_path = format!("{}/aircraft-characteristics.csv", plugin_dir);
            xplane_log(&format!("Loading aircraft characteristics from: {}", chars_path));
            match CharacteristicsDatabase::load_from_csv(&chars_path) {
                Ok(db) => {
                    xplane_log(&format!("Successfully loaded {} aircraft characteristics definitions.", db.characteristics.len()));
                    chars_db = Some(db);
                }
                Err(e) => {
                    xplane_log(&format!("Warning: Failed to load aircraft characteristics: {}", e));
                }
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();

        // Bind and listen to UDP socket on a background thread
        std::thread::spawn(move || {
            let socket = match std::net::UdpSocket::bind("0.0.0.0:49020") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[ButterlogXP] ERROR: Failed to bind UDP port 49020: {}", e);
                    return;
                }
            };
            let mut buf = [0u8; 65536];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((amt, _src)) => {
                        let data = &buf[..amt];
                        if let Ok(update) = serde_json::from_slice::<AircraftUpdate>(data) {
                            if tx.send(update).is_err() {
                                break; // Receiver dropped, terminating thread
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[ButterlogXP] ERROR: UDP socket read failure: {}", e);
                        break;
                    }
                }
            }
        });

        let terrain_probe = unsafe {
            xplm_sys::XPLMCreateProbe(xplm_sys::xplm_ProbeY as xplm_sys::XPLMProbeType)
        };

        #[cfg(feature = "capture-spike")]
        let capture = capture::Capture::start(&system_path);

        let state = Box::new(PluginState {
            rx,
            active_aircraft: HashMap::new(),
            object_cache: HashMap::new(),
            csl_db,
            system_path,
            chars_db,
            terrain_probe,
            #[cfg(feature = "capture-spike")]
            capture,
        });
        let state_ptr = Box::into_raw(state);

        // Register with X-Plane to consume updates from the channel on the main thread
        unsafe {
            xplm_sys::XPLMRegisterFlightLoopCallback(
                Some(flight_loop_callback),
                -1.0,
                state_ptr as *mut std::os::raw::c_void,
            );
        }

        xplane_log("Plugin started successfully and listening on UDP port 49020.");
        Ok(ButterlogXpPlugin { state_ptr })
    }

    fn info(&self) -> xplm::plugin::PluginInfo {
        xplm::plugin::PluginInfo {
            name: String::from("Butterlog X-Plane 12 Plugin"),
            signature: String::from("net.flyvoyager.butterlog-xp-plugin"),
            description: String::from("UDP proxy for multiplayer aircraft using CSL packages and XPLMInstance"),
        }
    }
}

impl Drop for ButterlogXpPlugin {
    fn drop(&mut self) {
        xplane_log("Plugin stopping...");
        unsafe {
            xplm_sys::XPLMUnregisterFlightLoopCallback(
                Some(flight_loop_callback),
                self.state_ptr as *mut std::os::raw::c_void,
            );
            
            let state = Box::from_raw(self.state_ptr);
            
            // Clean up all instances
            for (id, aircraft) in &state.active_aircraft {
                xplm_sys::XPLMDestroyInstance(aircraft.instance);
                xplane_log(&format!("Destroyed instance for: {}", id));
            }
            
            // Clean up all cached object files
            for (path, &object) in &state.object_cache {
                xplm_sys::XPLMUnloadObject(object);
                xplane_log(&format!("Unloaded object: {}", path));
            }

            if !state.terrain_probe.is_null() {
                xplm_sys::XPLMDestroyProbe(state.terrain_probe);
            }
        }
        xplane_log("Plugin stopped and resources released.");
    }
}

xplm::xplane_plugin!(ButterlogXpPlugin);
