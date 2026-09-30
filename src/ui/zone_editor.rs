//! Editor das áreas monitoradas, sobre o vídeo em tela cheia.
//!
//! [`Model`] é a parte pura (sem GTK): guarda os polígonos em frações do quadro
//! e implementa clicar, arrastar e apagar. [`ZoneEditor`] liga o modelo a uma
//! `DrawingArea` que, diferente do overlay de caixas, recebe cliques.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::cairo;
use gtk::prelude::*;

use crate::detection::Zone;

/// Distância (px) a partir da qual um clique "pega" um vértice.
const GRAB_RADIUS: f64 = 10.0;
/// Deslocamento (px) abaixo do qual um arrasto conta como clique.
const CLICK_SLOP: f64 = 4.0;

type Point = (f32, f32);

/// Retângulo `(x, y, largura, altura)` do vídeo dentro do widget.
pub type Rect = (f64, f64, f64, f64);

fn to_px(p: Point, r: Rect) -> (f64, f64) {
    (r.0 + f64::from(p.0) * r.2, r.1 + f64::from(p.1) * r.3)
}

fn to_frac(x: f64, y: f64, r: Rect) -> Option<Point> {
    if r.2 <= 0.0 || r.3 <= 0.0 {
        return None;
    }
    let (fx, fy) = ((x - r.0) / r.2, (y - r.1) / r.3);
    ((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)).then_some((fx as f32, fy as f32))
}

fn near(a: Point, x: f64, y: f64, r: Rect) -> bool {
    let (px, py) = to_px(a, r);
    (px - x).hypot(py - y) <= GRAB_RADIUS
}

#[derive(Debug, Default, Clone)]
pub struct Model {
    pub zones: Vec<Vec<Point>>,
    /// Polígono em desenho (ainda aberto).
    pub draft: Vec<Point>,
    grabbed: Option<(usize, usize)>,
}

impl Model {
    pub fn from_zones(zones: &[Zone]) -> Self {
        Self {
            zones: zones.iter().map(|z| z.points.clone()).collect(),
            ..Self::default()
        }
    }

    pub fn to_zones(&self) -> Vec<Zone> {
        self.zones
            .iter()
            .cloned()
            .map(|points| Zone { points })
            .filter(Zone::is_valid)
            .collect()
    }

    /// Fecha o polígono em desenho se já tiver 3 pontos. Devolve se fechou.
    pub fn finish_draft(&mut self) -> bool {
        if self.draft.len() < 3 {
            return false;
        }
        self.zones.push(std::mem::take(&mut self.draft));
        true
    }

    pub fn undo_point(&mut self) {
        self.draft.pop();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Começa a arrastar um vértice sob `(x, y)`. Devolve se pegou algum.
    pub fn grab(&mut self, x: f64, y: f64, r: Rect) -> bool {
        if !self.draft.is_empty() {
            return false;
        }
        self.grabbed = self
            .zones
            .iter()
            .enumerate()
            .find_map(|(zi, z)| z.iter().position(|&p| near(p, x, y, r)).map(|vi| (zi, vi)));
        self.grabbed.is_some()
    }

    pub fn drag_to(&mut self, x: f64, y: f64, r: Rect) {
        if let Some((zi, vi)) = self.grabbed {
            // Fora do vídeo: gruda na borda.
            let fx = ((x - r.0) / r.2).clamp(0.0, 1.0) as f32;
            let fy = ((y - r.1) / r.3).clamp(0.0, 1.0) as f32;
            self.zones[zi][vi] = (fx, fy);
        }
    }

    pub fn release(&mut self) {
        self.grabbed = None;
    }

    /// Clique simples: fecha o polígono (clicando no primeiro ponto) ou
    /// acrescenta um vértice.
    pub fn click(&mut self, x: f64, y: f64, r: Rect) {
        let Some(p) = to_frac(x, y, r) else {
            return;
        };
        if self.draft.len() >= 3 && near(self.draft[0], x, y, r) {
            self.finish_draft();
        } else {
            self.draft.push(p);
        }
    }

    /// Botão direito: desfaz o último ponto em desenho; senão apaga o vértice
    /// sob o cursor (a área some se ficar com menos de 3) ou a área sob ele.
    pub fn remove_at(&mut self, x: f64, y: f64, r: Rect) {
        if !self.draft.is_empty() {
            self.draft.pop();
            return;
        }
        for zi in 0..self.zones.len() {
            if let Some(vi) = self.zones[zi].iter().position(|&p| near(p, x, y, r)) {
                self.zones[zi].remove(vi);
                if self.zones[zi].len() < 3 {
                    self.zones.remove(zi);
                }
                return;
            }
        }
        if let Some(p) = to_frac(x, y, r)
            && let Some(zi) = self
                .zones
                .iter()
                .position(|z| Zone { points: z.clone() }.contains(p.0, p.1))
        {
            self.zones.remove(zi);
        }
    }
}

/// Fração do quadro → retângulo do vídeo no widget (`ContentFit::Contain`).
pub fn fit(w: f64, h: f64, frame_w: f64, frame_h: f64) -> Rect {
    let scale = (w / frame_w).min(h / frame_h);
    let (vw, vh) = (frame_w * scale, frame_h * scale);
    ((w - vw) / 2.0, (h - vh) / 2.0, vw, vh)
}

/// Desenha um polígono (fechado ou em desenho) com vértices.
pub fn draw_polygon(cr: &cairo::Context, points: &[Point], r: Rect, closed: bool, editing: bool) {
    if points.is_empty() {
        return;
    }
    let pts: Vec<_> = points.iter().map(|&p| to_px(p, r)).collect();
    cr.move_to(pts[0].0, pts[0].1);
    for &(x, y) in &pts[1..] {
        cr.line_to(x, y);
    }
    if closed {
        cr.close_path();
        cr.set_source_rgba(0.25, 0.65, 1.0, if editing { 0.22 } else { 0.12 });
        let _ = cr.fill_preserve();
    }
    cr.set_source_rgba(0.25, 0.65, 1.0, 0.95);
    cr.set_line_width(2.0);
    cr.set_dash(&[6.0, 4.0], 0.0);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);
    if editing {
        for &(x, y) in &pts {
            cr.arc(x, y, 5.0, 0.0, std::f64::consts::TAU);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            let _ = cr.fill_preserve();
            cr.set_source_rgb(0.25, 0.65, 1.0);
            cr.set_line_width(2.0);
            let _ = cr.stroke();
        }
    }
}

pub struct ZoneEditor {
    area: gtk::DrawingArea,
    model: Rc<RefCell<Model>>,
    /// Proporção do quadro (largura, altura) para mapear cliques.
    aspect: Rc<Cell<(f64, f64)>>,
}

impl ZoneEditor {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .visible(false)
            .focusable(true)
            .build();
        area.set_cursor_from_name(Some("crosshair"));
        let model = Rc::new(RefCell::new(Model::default()));
        let aspect = Rc::new(Cell::new((16.0, 9.0)));

        let rect = {
            let aspect = Rc::clone(&aspect);
            move |area: &gtk::DrawingArea| {
                let (fw, fh) = aspect.get();
                fit(f64::from(area.width()), f64::from(area.height()), fw, fh)
            }
        };

        {
            let model = Rc::clone(&model);
            let rect = rect.clone();
            let weak = area.downgrade();
            area.set_draw_func(move |_, cr, w, h| {
                let Some(area) = weak.upgrade() else {
                    return;
                };
                let _ = (w, h);
                let r = rect(&area);
                let model = model.borrow();
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
                let _ = cr.paint();
                for z in &model.zones {
                    draw_polygon(cr, z, r, true, true);
                }
                draw_polygon(cr, &model.draft, r, false, true);
            });
        }

        let drag = gtk::GestureDrag::builder().button(1).build();
        let grabbed = Rc::new(Cell::new(false));
        {
            let (model, rect, grabbed) = (Rc::clone(&model), rect.clone(), Rc::clone(&grabbed));
            drag.connect_drag_begin(move |g, x, y| {
                if let Some(area) = g
                    .widget()
                    .and_then(|w| w.downcast::<gtk::DrawingArea>().ok())
                {
                    grabbed.set(model.borrow_mut().grab(x, y, rect(&area)));
                }
            });
        }
        {
            let (model, rect, grabbed) = (Rc::clone(&model), rect.clone(), Rc::clone(&grabbed));
            drag.connect_drag_update(move |g, dx, dy| {
                if !grabbed.get() {
                    return;
                }
                if let (Some((sx, sy)), Some(area)) = (
                    g.start_point(),
                    g.widget()
                        .and_then(|w| w.downcast::<gtk::DrawingArea>().ok()),
                ) {
                    model.borrow_mut().drag_to(sx + dx, sy + dy, rect(&area));
                    area.queue_draw();
                }
            });
        }
        {
            let (model, rect, grabbed) = (Rc::clone(&model), rect.clone(), Rc::clone(&grabbed));
            drag.connect_drag_end(move |g, dx, dy| {
                let Some(area) = g
                    .widget()
                    .and_then(|w| w.downcast::<gtk::DrawingArea>().ok())
                else {
                    return;
                };
                if !grabbed.get()
                    && dx.hypot(dy) < CLICK_SLOP
                    && let Some((sx, sy)) = g.start_point()
                {
                    model.borrow_mut().click(sx, sy, rect(&area));
                }
                model.borrow_mut().release();
                grabbed.set(false);
                area.queue_draw();
            });
        }
        area.add_controller(drag);

        let right = gtk::GestureClick::builder().button(3).build();
        {
            let (model, rect) = (Rc::clone(&model), rect.clone());
            right.connect_pressed(move |g, _, x, y| {
                if let Some(area) = g
                    .widget()
                    .and_then(|w| w.downcast::<gtk::DrawingArea>().ok())
                {
                    model.borrow_mut().remove_at(x, y, rect(&area));
                    area.queue_draw();
                }
            });
        }
        area.add_controller(right);

        Self {
            area,
            model,
            aspect,
        }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn set_aspect(&self, w: f64, h: f64) {
        if w > 0.0 && h > 0.0 {
            self.aspect.set((w, h));
        }
    }

    /// Abre o editor com as áreas atuais.
    pub fn begin(&self, zones: &[Zone]) {
        *self.model.borrow_mut() = Model::from_zones(zones);
        self.area.set_visible(true);
        self.area.queue_draw();
    }

    pub fn end(&self) {
        self.area.set_visible(false);
    }

    pub fn is_active(&self) -> bool {
        self.area.is_visible()
    }

    /// Fecha o polígono em desenho (se der) e devolve as áreas resultantes.
    pub fn take_zones(&self) -> Vec<Zone> {
        let mut model = self.model.borrow_mut();
        model.finish_draft();
        model.to_zones()
    }

    pub fn finish_draft(&self) {
        self.model.borrow_mut().finish_draft();
        self.area.queue_draw();
    }

    pub fn undo(&self) {
        self.model.borrow_mut().undo_point();
        self.area.queue_draw();
    }

    pub fn clear(&self) {
        self.model.borrow_mut().clear();
        self.area.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = (0.0, 0.0, 100.0, 100.0);

    #[test]
    fn clique_acrescenta_e_fecha_no_primeiro_ponto() {
        let mut m = Model::default();
        m.click(10.0, 10.0, R);
        m.click(90.0, 10.0, R);
        m.click(50.0, 90.0, R);
        assert_eq!(m.draft.len(), 3);
        m.click(12.0, 11.0, R); // perto do primeiro
        assert!(m.draft.is_empty());
        assert_eq!(m.zones.len(), 1);
        assert_eq!(m.to_zones().len(), 1);
    }

    #[test]
    fn clique_fora_do_video_e_ignorado() {
        let mut m = Model::default();
        m.click(150.0, 10.0, (0.0, 0.0, 100.0, 100.0));
        assert!(m.draft.is_empty());
    }

    #[test]
    fn arrasta_vertice_e_limita_na_borda() {
        let mut m = Model::default();
        for (x, y) in [(10.0, 10.0), (90.0, 10.0), (50.0, 90.0)] {
            m.click(x, y, R);
        }
        m.finish_draft();
        assert!(m.grab(90.0, 11.0, R));
        m.drag_to(500.0, -20.0, R);
        m.release();
        assert_eq!(m.zones[0][1], (1.0, 0.0));
    }

    #[test]
    fn direito_remove_vertice_area_e_ponto_em_desenho() {
        let mut m = Model {
            zones: vec![vec![(0.1, 0.1), (0.9, 0.1), (0.5, 0.9), (0.5, 0.5)]],
            ..Model::default()
        };
        m.remove_at(50.0, 50.0, R); // vértice
        assert_eq!(m.zones[0].len(), 3);
        m.remove_at(50.0, 30.0, R); // dentro da área
        assert!(m.zones.is_empty());
        m.click(10.0, 10.0, R);
        m.remove_at(0.0, 0.0, R);
        assert!(m.draft.is_empty());
    }

    #[test]
    fn nao_fecha_com_menos_de_tres_pontos() {
        let mut m = Model::default();
        m.click(10.0, 10.0, R);
        m.click(20.0, 20.0, R);
        assert!(!m.finish_draft());
        assert!(m.to_zones().is_empty());
    }
}
