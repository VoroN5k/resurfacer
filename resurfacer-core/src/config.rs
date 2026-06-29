use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Config {
    pub watch_later: WatchLaterConfig,
    pub rabbit_hole: RabbitHoleConfig,
    pub idle_detection: IdleDetectionConfig,
    pub llm: LlmConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct WatchLaterConfig {
    pub grace_period_seconds: u64,
    pub video_domains: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RabbitHoleConfig {
    pub cluster_window_minutes: u64,
    pub no_focus_threshold_minutes: u64,
    pub min_cluster_size: usize,
}

#[derive(Deserialize, Debug, Clone)]
pub struct IdleDetectionConfig {
    pub presence_threshold_seconds: u64,
    pub debounce_seconds: u64,
    pub heavy_process_denylist: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LlmConfig {
    pub model_path: String,
    pub max_tabs_per_summary_batch: usize,
    pub excerpt_word_limit: usize,
}

impl Default for WatchLaterConfig {
    fn default() -> Self {
        Self {
            grace_period_seconds: 20,
            video_domains: vec![
                "youtube.com".into(),
                "twitch.tv".into(),
                "vimeo.com".into(),
            ],
        }
    }
}

impl Default for RabbitHoleConfig {
    fn default() -> Self {
        Self {
            cluster_window_minutes: 10,
            no_focus_threshold_minutes: 15,
            min_cluster_size: 3,
        }
    }
}

impl Default for IdleDetectionConfig {
    fn default() -> Self {
        Self {
            presence_threshold_seconds: 120,
            debounce_seconds: 15,
            heavy_process_denylist: vec!["obs64.exe".into(), "premiere.exe".into()],
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model_path: "./models/qwen2.5-7b-instruct-q4_k_m.gguf".into(),
            max_tabs_per_summary_batch: 30,
            excerpt_word_limit: 300,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let contents = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&contents)?)
    }
}

// Returns the path for config.toml, resolved relative to the running executable
pub fn config_path() -> PathBuf {
    exe_dir().join("config.toml")
}

// Returns the directory containing the running executable
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}
