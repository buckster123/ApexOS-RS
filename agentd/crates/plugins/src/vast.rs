use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

// ── Recipe file types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecipeFile {
    pub docker:     DockerConfig,
    #[serde(default)]
    pub gpu_tiers:  HashMap<String, GpuTier>,
    #[serde(default)]
    pub recipes:    Vec<Recipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DockerConfig {
    pub prebuilt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuTier {
    pub vast_names:  Vec<String>,
    pub label:       String,
    pub max_price:   String,
    pub min_disk_gb: u32,
    pub vram_gb:     u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub name:        String,
    pub label:       String,
    pub gpu:         String,
    pub model_repo:  String,
    pub model_quant: String,
    pub ctx:         u32,
    pub parallel:    u32,
    pub kv_type:     String,
    pub description: String,
}

pub fn load_recipes() -> anyhow::Result<RecipeFile> {
    let path = std::env::var("RECIPES_TOML")
        .unwrap_or_else(|_| "/etc/agentd/recipes.toml".into());
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("recipes.toml not found at {path}: {e}"))?;
    let rf: RecipeFile = toml::from_str(&content)?;
    Ok(rf)
}

pub fn recipes_path() -> PathBuf {
    PathBuf::from(
        std::env::var("RECIPES_TOML").unwrap_or_else(|_| "/etc/agentd/recipes.toml".into())
    )
}

pub fn instance_json_path() -> PathBuf {
    let workspace = std::env::var("AGENTD_WORKSPACE")
        .unwrap_or_else(|_| "/var/lib/agentd/workspace".into());
    PathBuf::from(workspace).join("vast").join("instance.json")
}

// ── Runtime state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VastInstance {
    pub id:          String,
    pub recipe:      String,
    pub ssh_host:    String,
    pub ssh_port:    u16,
    pub local_port:  u16,
    pub cost_per_hr: f64,
    pub launched_at: String,
}

pub struct TunnelHandle {
    pub child:      tokio::process::Child,
    pub local_port: u16,
}

/// Phase description during vast_launch — shown in status API while booting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VastPhase {
    Idle,
    Launching { phase: String },
    Ready,
    Destroying,
}

#[derive(Clone)]
pub struct VastState {
    pub instance: Arc<RwLock<Option<VastInstance>>>,
    pub tunnel:   Arc<Mutex<Option<TunnelHandle>>>,
    pub phase:    Arc<RwLock<VastPhase>>,
}

impl Default for VastState {
    fn default() -> Self {
        Self::new()
    }
}

impl VastState {
    pub fn new() -> Self {
        Self {
            instance: Arc::new(RwLock::new(None)),
            tunnel:   Arc::new(Mutex::new(None)),
            phase:    Arc::new(RwLock::new(VastPhase::Idle)),
        }
    }

    /// Attempt to restore from a persisted instance.json on boot.
    pub async fn try_restore(&self) {
        let path = instance_json_path();
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if let Ok(inst) = serde_json::from_str::<VastInstance>(&content) {
                eprintln!("[vast] restoring instance {} from {}", inst.id, path.display());
                *self.instance.write().await = Some(inst);
                *self.phase.write().await = VastPhase::Ready;
            }
        }
    }

    pub async fn persist_instance(&self) {
        let guard = self.instance.read().await;
        if let Some(ref inst) = *guard {
            let path = instance_json_path();
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Ok(json) = serde_json::to_string_pretty(inst) {
                let _ = tokio::fs::write(&path, json).await;
            }
        }
    }

    pub async fn clear_instance(&self) {
        *self.instance.write().await = None;
        *self.phase.write().await = VastPhase::Idle;
        let _ = tokio::fs::remove_file(instance_json_path()).await;
    }
}

// ── vastai CLI helpers ────────────────────────────────────────────────────────

/// Run `vastai <args>` with VAST_API_KEY set, return trimmed stdout.
pub async fn vastai(args: &[&str]) -> anyhow::Result<String> {
    let api_key = std::env::var("VAST_API_KEY")
        .map_err(|_| anyhow::anyhow!("VAST_API_KEY not set"))?;
    let mut cmd = tokio::process::Command::new("vastai");
    cmd.args(args)
       .env("VAST_API_KEY", &api_key)
       .stdout(std::process::Stdio::piped())
       .stderr(std::process::Stdio::piped());
    let out = cmd.output().await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(anyhow::anyhow!("vastai {}: {}", args.first().unwrap_or(&""), err.trim()))
    }
}
