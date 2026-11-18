//! Mutable host upscaler / sharpen config (SGSR, FSR1, RCAS).
//!
//! Startup seed comes from ATRIUM_* env (if set). Atrium may override at any time
//! via NAPI `setHostUpscale`. Shaders are compiled lazily on the GL thread when
//! a mode is first needed.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HostUpscale {
    #[default]
    Off,
    Sgsr,
    Fsr1,
    Anime4k,
}

impl HostUpscale {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "none" | "bilinear" => Some(Self::Off),
            "1" | "on" | "sgsr" => Some(Self::Sgsr),
            "fsr1" | "fsr" | "easu" => Some(Self::Fsr1),
            "anime4k" | "a4k" => Some(Self::Anime4k),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Sgsr => "sgsr",
            Self::Fsr1 => "fsr1",
            Self::Anime4k => "anime4k",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostUpscaleConfig {
    pub mode: HostUpscale,
    #[serde(alias = "sharpenRcas")]
    pub sharpen_rcas: bool,
    /// SGSR edge threshold in [0,1]. Default 8/255.
    #[serde(alias = "sgsrEdgeThreshold")]
    pub sgsr_edge_threshold: f32,
    /// SGSR edge sharpness. Default 2.0.
    #[serde(alias = "sgsrEdgeSharpness")]
    pub sgsr_edge_sharpness: f32,
    /// When false, skip SGSR edge-direction enhancement (bilinear sample only).
    #[serde(alias = "sgsrUseEdgeDirection")]
    pub sgsr_use_edge_direction: bool,
    /// RCAS stops: 0 = max sharpen, higher = softer. Default 0.2.
    #[serde(alias = "rcasSharpnessStops")]
    pub rcas_sharpness_stops: f32,
    /// Anime4K mode: "a" (restore), "b" (restore soft), "c" (upscale denoise).
    #[serde(alias = "anime4kMode", default = "default_anime4k_mode")]
    pub anime4k_mode: String,
    /// Anime4K tier: "s" (Fast), "m" (Balanced), "l" (High), "vl", "ul".
    #[serde(alias = "anime4kTier", default = "default_anime4k_tier")]
    pub anime4k_tier: String,
    /// Anime4K +A secondary pass.
    #[serde(alias = "anime4kPlusA", default)]
    pub anime4k_plus_a: bool,
}

fn default_anime4k_mode() -> String {
    "a".to_string()
}

fn default_anime4k_tier() -> String {
    "m".to_string()
}

impl Default for HostUpscaleConfig {
    fn default() -> Self {
        Self {
            mode: HostUpscale::Off,
            sharpen_rcas: false,
            sgsr_edge_threshold: 8.0 / 255.0,
            sgsr_edge_sharpness: 2.0,
            sgsr_use_edge_direction: true,
            rcas_sharpness_stops: 0.2,
            anime4k_mode: "a".to_string(),
            anime4k_tier: "m".to_string(),
            anime4k_plus_a: false,
        }
    }
}

impl HostUpscaleConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("ATRIUM_UPSCALE") {
            if let Some(m) = HostUpscale::parse(&v) {
                cfg.mode = m;
            }
        }
        if let Ok(v) = std::env::var("ATRIUM_SHARPEN") {
            let l = v.to_ascii_lowercase();
            cfg.sharpen_rcas = matches!(l.as_str(), "rcas" | "cas" | "1" | "on");
        }
        if let Some(t) = parse_sgsr_threshold(std::env::var("ATRIUM_SGSR_THRESHOLD").ok().as_deref())
        {
            cfg.sgsr_edge_threshold = t;
        }
        if let Ok(v) = std::env::var("ATRIUM_SGSR_SHARPNESS") {
            if let Ok(n) = v.parse::<f32>() {
                if n.is_finite() && n >= 0.0 {
                    cfg.sgsr_edge_sharpness = n;
                }
            }
        }
        if let Ok(v) = std::env::var("ATRIUM_SGSR_EDGE_DIRECTION") {
            let l = v.to_ascii_lowercase();
            cfg.sgsr_use_edge_direction = !matches!(l.as_str(), "0" | "off" | "false" | "no");
        }
        if let Ok(v) = std::env::var("ATRIUM_RCAS_SHARPNESS") {
            if let Ok(n) = v.parse::<f32>() {
                if n.is_finite() && n >= 0.0 {
                    cfg.rcas_sharpness_stops = n;
                }
            }
        }
        if let Ok(v) = std::env::var("ATRIUM_ANIME4K_MODE") {
            let s = v.trim().to_ascii_lowercase();
            if ["a", "b", "c"].contains(&s.as_str()) {
                cfg.anime4k_mode = s;
            }
        }
        if let Ok(v) = std::env::var("ATRIUM_ANIME4K_TIER") {
            let s = v.trim().to_ascii_lowercase();
            if ["s", "m", "l", "vl", "ul"].contains(&s.as_str()) {
                cfg.anime4k_tier = s;
            }
        }
        if let Ok(v) = std::env::var("ATRIUM_ANIME4K_PLUS_A") {
            let s = v.trim().to_ascii_lowercase();
            cfg.anime4k_plus_a = matches!(s.as_str(), "1" | "true" | "on" | "yes");
        }
        cfg
    }

    pub fn clamp(mut self) -> Self {
        self.sgsr_edge_threshold = self.sgsr_edge_threshold.clamp(0.0, 1.0);
        if !self.sgsr_edge_sharpness.is_finite() || self.sgsr_edge_sharpness < 0.0 {
            self.sgsr_edge_sharpness = 2.0;
        }
        self.sgsr_edge_sharpness = self.sgsr_edge_sharpness.clamp(0.5, 4.0);
        if !self.rcas_sharpness_stops.is_finite() || self.rcas_sharpness_stops < 0.0 {
            self.rcas_sharpness_stops = 0.2;
        }
        self.rcas_sharpness_stops = self.rcas_sharpness_stops.clamp(0.0, 2.0);
        let m = self.anime4k_mode.trim().to_ascii_lowercase();
        self.anime4k_mode = if ["a", "b", "c"].contains(&m.as_str()) {
            m
        } else {
            "a".to_string()
        };
        let t = self.anime4k_tier.trim().to_ascii_lowercase();
        self.anime4k_tier = if ["s", "m", "l", "vl", "ul"].contains(&t.as_str()) {
            t
        } else {
            "m".to_string()
        };
        self
    }
}

fn parse_sgsr_threshold(raw: Option<&str>) -> Option<f32> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let v: f32 = s.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    if v > 1.0 {
        Some((v / 255.0).clamp(0.0, 1.0))
    } else {
        Some(v)
    }
}

static HOST_UPSCALE: Mutex<Option<HostUpscaleConfig>> = Mutex::new(None);

fn with_cfg<R>(f: impl FnOnce(&HostUpscaleConfig) -> R) -> R {
    let mut guard = HOST_UPSCALE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(HostUpscaleConfig::from_env().clamp());
    }
    f(guard.as_ref().unwrap())
}

pub fn host_upscale_config() -> HostUpscaleConfig {
    with_cfg(|c| c.clone())
}

pub fn set_host_upscale_config(cfg: HostUpscaleConfig) -> HostUpscaleConfig {
    let next = cfg.clamp();
    let mut guard = HOST_UPSCALE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(next.clone());
    next
}

pub fn host_upscale() -> HostUpscale {
    with_cfg(|c| c.mode)
}

pub fn sgsr_upscale_enabled() -> bool {
    host_upscale() == HostUpscale::Sgsr
}

pub fn fsr1_upscale_enabled() -> bool {
    host_upscale() == HostUpscale::Fsr1
}

pub fn anime4k_upscale_enabled() -> bool {
    host_upscale() == HostUpscale::Anime4k
}

pub fn host_anime4k_mode() -> String {
    with_cfg(|c| c.anime4k_mode.clone())
}

pub fn host_anime4k_tier() -> String {
    with_cfg(|c| c.anime4k_tier.clone())
}

pub fn host_anime4k_plus_a() -> bool {
    with_cfg(|c| c.anime4k_plus_a)
}

pub fn host_sharpen_rcas() -> bool {
    with_cfg(|c| c.sharpen_rcas)
}

pub fn sgsr_edge_threshold() -> f32 {
    with_cfg(|c| c.sgsr_edge_threshold)
}

pub fn sgsr_edge_sharpness() -> f32 {
    with_cfg(|c| c.sgsr_edge_sharpness)
}

pub fn sgsr_use_edge_direction() -> bool {
    with_cfg(|c| c.sgsr_use_edge_direction)
}

pub fn rcas_sharpness_stops() -> f32 {
    with_cfg(|c| c.rcas_sharpness_stops)
}

/// True if any ATRIUM_UPSCALE / ATRIUM_SHARPEN / SGSR / RCAS / ANIME4K env was set at process start.
pub fn env_overrides_present() -> bool {
    const KEYS: &[&str] = &[
        "ATRIUM_UPSCALE",
        "ATRIUM_SHARPEN",
        "ATRIUM_SGSR_THRESHOLD",
        "ATRIUM_SGSR_SHARPNESS",
        "ATRIUM_SGSR_EDGE_DIRECTION",
        "ATRIUM_RCAS_SHARPNESS",
        "ATRIUM_ANIME4K_MODE",
        "ATRIUM_ANIME4K_TIER",
        "ATRIUM_ANIME4K_PLUS_A",
        "ATRIUM_INTERNAL_WIDTH",
        "ATRIUM_INTERNAL_HEIGHT",
    ];
    KEYS.iter().any(|k| std::env::var_os(k).is_some())
}
