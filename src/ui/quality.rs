//! Preferência de qualidade por câmera, guardada entre execuções.
//!
//! Arquivo: `<config>/nvr-dashboard/quality.toml`, com uma entrada por
//! `<nvr>/<canal>`. Falhas de leitura/escrita nunca são fatais.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::camera::Quality;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    cameras: BTreeMap<String, Quality>,
}

fn path() -> Option<PathBuf> {
    crate::config::user_config_dir().map(|dir| dir.join("nvr-dashboard").join("quality.toml"))
}

pub fn load() -> BTreeMap<String, Quality> {
    path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<Saved>(&text).ok())
        .map(|saved| saved.cameras)
        .unwrap_or_default()
}

pub fn save(cameras: &BTreeMap<String, Quality>) {
    let Some(path) = path() else { return };
    let text = toml::to_string(&Saved {
        cameras: cameras.clone(),
    })
    .unwrap_or_default();
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    if let Err(err) = result {
        tracing::warn!(%err, "não consegui salvar a qualidade das câmeras");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializa_e_le_de_volta() {
        let mut map = BTreeMap::new();
        map.insert("nvr/1".to_string(), Quality::Low);
        let text = toml::to_string(&Saved { cameras: map }).unwrap();
        let back: Saved = toml::from_str(&text).unwrap();
        assert_eq!(back.cameras["nvr/1"], Quality::Low);
    }
}
