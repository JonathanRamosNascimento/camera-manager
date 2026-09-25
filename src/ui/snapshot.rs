//! Captura do quadro atual como PNG.
//!
//! Em vez de puxar o `last-sample` do sink e converter o vídeo na mão,
//! renderizamos o próprio `GdkPaintable` num `GdkTexture` usando o renderer da
//! janela. Sai na resolução nativa do vídeo (não na do widget) e não exige
//! nenhuma dependência além do GTK.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gtk::prelude::*;
use gtk::{gdk, glib};

/// Grava o quadro atual de `paintable` em `directory`.
///
/// `widget` só é usado para chegar ao renderer da janela, então qualquer widget
/// já realizado (dentro de uma janela visível) serve.
pub fn capture(
    widget: &impl IsA<gtk::Widget>,
    paintable: &gdk::Paintable,
    directory: &Path,
    slug: &str,
) -> Result<PathBuf> {
    let width = paintable.intrinsic_width();
    let height = paintable.intrinsic_height();
    if width <= 0 || height <= 0 {
        bail!("a câmera ainda não entregou nenhum quadro");
    }

    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().context("o quadro atual está vazio")?;

    let renderer = widget
        .as_ref()
        .native()
        .and_then(|native| native.renderer())
        .context("a janela ainda não tem um renderer")?;
    let texture = renderer.render_texture(&node, None);

    fs::create_dir_all(directory)
        .with_context(|| format!("não consegui criar {}", directory.display()))?;

    let stamp = glib::DateTime::now_local()
        .and_then(|now| now.format("%Y%m%d-%H%M%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "sem-data".to_string());
    let path = directory.join(format!("{slug}_{stamp}.png"));

    fs::write(&path, texture.save_to_png_bytes())
        .with_context(|| format!("não consegui gravar {}", path.display()))?;

    Ok(path)
}
