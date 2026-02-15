use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
}

pub struct WindowStateManager {
    states: HashMap<String, WindowState>,
    config_path: PathBuf,
}

impl WindowStateManager {
    pub fn new(app_identifier: &str) -> Self {
        let config_path = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(app_identifier)
            .join("window-state.json");
        
        // Ensure directory exists
        if let Some(parent) = config_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        
        let states = Self::load_from_disk(&config_path);
        
        WindowStateManager {
            states,
            config_path,
        }
    }
    
    pub fn save_window_state(&mut self, key: &str, state: WindowState) {
        self.states.insert(key.to_string(), state);
        self.save_to_disk();
    }
    
    pub fn get_window_state(&self, key: &str) -> Option<&WindowState> {
        self.states.get(key)
    }
    
    fn load_from_disk(path: &PathBuf) -> HashMap<String, WindowState> {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(states) = serde_json::from_str(&content) {
                    return states;
                }
            }
        }
        HashMap::new()
    }
    
    fn save_to_disk(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.states) {
            let _ = fs::write(&self.config_path, json);
        }
    }
}
