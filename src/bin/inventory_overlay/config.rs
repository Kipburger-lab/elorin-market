//! `overlay.toml` schema + loader/saver for the inventory overlay tool.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::grid::GridParams;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayConfig {
    pub window: WindowCfg,
    pub grid: GridParams,
    #[serde(default)]
    pub overlay: OverlayOpts,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCfg {
    #[serde(rename = "title_contains")]
    pub title_contains: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayOpts {
    #[serde(default = "default_true")]
    pub show_hud: bool,
}

fn default_true() -> bool {
    true
}

impl Default for OverlayOpts {
    fn default() -> Self {
        Self { show_hud: true }
    }
}

impl OverlayConfig {
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let cfg: OverlayConfig = toml::from_str(&text).context("invalid overlay.toml")?;
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))?;
        Ok(())
    }

    pub fn default_config() -> Self {
        Self {
            window: WindowCfg {
                title_contains: "Elorin".to_string(),
            },
            grid: GridParams::DEFAULT,
            overlay: OverlayOpts::default(),
        }
    }
}
