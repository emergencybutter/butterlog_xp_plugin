use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::raw::c_char;
use std::path::Path;
use serde::{Deserialize, Serialize};

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

        if let (Some(icao_i), Some(mfg_i), Some(model_faa_i), Some(model_bada_i), Some(engine_type_i), Some(num_engines_i), Some(wtc_i), Some(class_i)) = 
            (icao_idx, mfg_idx, model_faa_idx, model_bada_idx, engine_type_idx, num_engines_idx, wtc_idx, class_idx) {
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
                        
                        map.insert(icao_upper.clone(), AircraftCharacteristic {
                            icao_code: icao_upper,
                            manufacturer,
                            model_faa,
                            model_bada,
                            engine_type,
                            num_engines,
                            wtc,
                            class,
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
    
    score
}

pub struct CslModel {
    pub model_id: String,
    pub obj_path: String, // Path relative to X-Plane system path
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
}

pub struct PluginState {
    pub rx: std::sync::mpsc::Receiver<AircraftUpdate>,
    pub active_aircraft: HashMap<String, ActiveAircraft>,
    pub object_cache: HashMap<String, xplm_sys::XPLMObjectRef>,
    pub csl_db: CslDatabase,
    pub system_path: String,
    pub chars_db: Option<CharacteristicsDatabase>,
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
                        let rel_obj_path = tokens[3];
                        let abs_obj_path = parent_dir.join(rel_obj_path);
                        
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

                        db.models.insert(
                            model_id.clone(),
                            CslModel {
                                model_id: model_id.clone(),
                                obj_path: rel_path_str,
                            },
                        );
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
                    if best_score >= 100 { // Share at least some basic class traits
                        model_id_opt = state.csl_db.icao_map.get(b_icao).cloned();
                        xplane_log(&format!(
                            "Similarity match: requested ICAO '{}' matched to available '{}' (score: {})",
                            icao_upper, b_icao, best_score
                        ));
                    }
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

    let draw_info = xplm_sys::XPLMDrawInfo_t {
        structSize: std::mem::size_of::<xplm_sys::XPLMDrawInfo_t>() as i32,
        x: x as f32,
        y: y as f32,
        z: z as f32,
        pitch: update.pitch,
        heading: update.heading,
        roll: update.roll,
    };

    if let Some(aircraft) = state.active_aircraft.get_mut(&update.id) {
        // Recreate instance if model changed
        if aircraft.object != object_ref {
            unsafe {
                xplm_sys::XPLMDestroyInstance(aircraft.instance);
            }
            let instance = unsafe { xplm_sys::XPLMCreateInstance(object_ref, std::ptr::null_mut()) };
            if instance.is_null() {
                state.active_aircraft.remove(&update.id);
                return;
            }
            aircraft.instance = instance;
            aircraft.object = object_ref;
        }
        unsafe {
            xplm_sys::XPLMInstanceSetPosition(aircraft.instance, &draw_info, std::ptr::null());
        }
        aircraft.last_updated = std::time::Instant::now();
    } else {
        // Create new instance
        let instance = unsafe { xplm_sys::XPLMCreateInstance(object_ref, std::ptr::null_mut()) };
        if instance.is_null() {
            xplane_log(&format!("ERROR: Failed to create instance for: {}", update.id));
            return;
        }
        unsafe {
            xplm_sys::XPLMInstanceSetPosition(instance, &draw_info, std::ptr::null());
        }
        xplane_log(&format!("Created aircraft instance: {} (model: {})", update.id, model_id));
        state.active_aircraft.insert(
            update.id.clone(),
            ActiveAircraft {
                instance,
                object: object_ref,
                last_updated: std::time::Instant::now(),
            },
        );
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

        let state = Box::new(PluginState {
            rx,
            active_aircraft: HashMap::new(),
            object_cache: HashMap::new(),
            csl_db,
            system_path,
            chars_db,
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
        }
        xplane_log("Plugin stopped and resources released.");
    }
}

xplm::xplane_plugin!(ButterlogXpPlugin);
