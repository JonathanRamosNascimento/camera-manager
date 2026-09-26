//! Caixas da identificação de objetos desenhadas por cima do vídeo.
//!
//! É uma `DrawingArea` transparente e "intocável" (não rouba cliques do card)
//! que consulta o [`DetectionState`] da câmera. As caixas vêm em frações do
//! quadro analisado; aqui elas são levadas para o retângulo que o vídeo de fato
//! ocupa no widget (`ContentFit::Contain` deixa faixas nas laterais ou em
//! cima/embaixo).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{cairo, glib};

use crate::detection::{DetectionState, classes};

/// De quanto em quanto tempo checar se há algo novo para desenhar.
const REFRESH: Duration = Duration::from_millis(150);

/// Cores das caixas (RGB 0–1), repetidas por classe.
const PALETTE: [(f64, f64, f64); 8] = [
    (0.20, 0.85, 0.35), // verde
    (1.00, 0.72, 0.10), // âmbar
    (0.25, 0.65, 1.00), // azul
    (1.00, 0.35, 0.35), // vermelho
    (0.75, 0.45, 1.00), // roxo
    (0.15, 0.85, 0.85), // ciano
    (1.00, 0.55, 0.20), // laranja
    (0.95, 0.40, 0.75), // rosa
];

pub struct DetectionOverlay {
    area: gtk::DrawingArea,
    source: Rc<RefCell<Option<Arc<DetectionState>>>>,
}

impl DetectionOverlay {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .can_target(false)
            .focusable(false)
            .build();
        let source: Rc<RefCell<Option<Arc<DetectionState>>>> = Rc::new(RefCell::new(None));

        {
            let source = Rc::clone(&source);
            area.set_draw_func(move |_, cr, width, height| {
                if let Some(state) = &*source.borrow() {
                    draw(cr, f64::from(width), f64::from(height), state);
                }
            });
        }

        // Redesenha só quando algo mudou: resultado novo, caixas que expiraram
        // ou mensagem de status diferente.
        {
            let weak = area.downgrade();
            let source = Rc::clone(&source);
            let last = Cell::new(None::<(u64, bool, Option<String>)>);
            glib::timeout_add_local(REFRESH, move || {
                let Some(area) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let key = source.borrow().as_ref().map(|state| {
                    (
                        state.seq(),
                        !state.snapshot().detections.is_empty(),
                        state.status().describe(),
                    )
                });
                let previous = last.replace(key.clone());
                if key != previous {
                    area.queue_draw();
                }
                glib::ControlFlow::Continue
            });
        }

        Self { area, source }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Passa a mostrar as detecções de outra câmera (ou de nenhuma).
    pub fn set_source(&self, state: Option<Arc<DetectionState>>) {
        *self.source.borrow_mut() = state;
        self.area.queue_draw();
    }
}

/// Retângulo `(x, y, largura, altura)` que um quadro de proporção
/// `frame_w`:`frame_h` ocupa, centralizado e sem cortes, num widget `w`×`h`.
fn fit(w: f64, h: f64, frame_w: f64, frame_h: f64) -> (f64, f64, f64, f64) {
    let scale = (w / frame_w).min(h / frame_h);
    let (vw, vh) = (frame_w * scale, frame_h * scale);
    ((w - vw) / 2.0, (h - vh) / 2.0, vw, vh)
}

fn draw(cr: &cairo::Context, width: f64, height: f64, state: &DetectionState) {
    let snapshot = state.snapshot();
    if snapshot.width > 0 && snapshot.height > 0 {
        let (vx, vy, vw, vh) = fit(width, height, snapshot.width as f64, snapshot.height as f64);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size((vh / 26.0).clamp(10.0, 16.0));
        cr.set_line_width(2.0);

        for d in &snapshot.detections {
            let (r, g, b) = PALETTE[d.class % PALETTE.len()];
            let (x, y) = (vx + f64::from(d.x1) * vw, vy + f64::from(d.y1) * vh);
            let (w, h) = (f64::from(d.x2 - d.x1) * vw, f64::from(d.y2 - d.y1) * vh);
            cr.set_source_rgb(r, g, b);
            cr.rectangle(x, y, w, h);
            let _ = cr.stroke();

            let text = format!("{} {:.0}%", classes::pt(d.class), d.score * 100.0);
            label(cr, x, y, &text, (r, g, b));
        }
    }

    // Modelo baixando, carregando ou com erro: o usuário precisa saber por que
    // não aparece caixa nenhuma.
    if let Some(text) = state.status().describe() {
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(12.0);
        label(cr, 8.0, height - 26.0, &text, (0.6, 0.6, 0.6));
    }
}

/// Texto sobre uma tarja da cor da classe, apoiado no canto `(x, y)` — por cima
/// da caixa, ou por dentro dela quando não há espaço acima.
fn label(cr: &cairo::Context, x: f64, y: f64, text: &str, (r, g, b): (f64, f64, f64)) {
    let Ok(extents) = cr.text_extents(text) else {
        return;
    };
    let (pad, tag_h) = (4.0, extents.height() + 8.0);
    let top = if y - tag_h >= 0.0 { y - tag_h } else { y };
    cr.set_source_rgb(r, g, b);
    cr.rectangle(x - 1.0, top, extents.width() + 2.0 * pad, tag_h);
    let _ = cr.fill();
    cr.set_source_rgb(0.05, 0.05, 0.05);
    cr.move_to(
        x + pad - extents.x_bearing(),
        top + 4.0 - extents.y_bearing(),
    );
    let _ = cr.show_text(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_mais_largo_que_o_widget_ganha_faixas_em_cima_e_embaixo() {
        // Widget 400×400, vídeo 16:9: ocupa a largura toda.
        let (x, y, w, h) = fit(400.0, 400.0, 1600.0, 900.0);
        assert_eq!((x, w), (0.0, 400.0));
        assert!((h - 225.0).abs() < 1e-9);
        assert!((y - 87.5).abs() < 1e-9);
    }

    #[test]
    fn video_mais_alto_que_o_widget_ganha_faixas_nas_laterais() {
        let (x, y, w, h) = fit(800.0, 300.0, 400.0, 400.0);
        assert_eq!((y, h), (0.0, 300.0));
        assert_eq!(w, 300.0);
        assert_eq!(x, 250.0);
    }
}
