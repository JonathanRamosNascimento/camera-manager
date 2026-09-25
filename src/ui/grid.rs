//! Grade dinâmica de câmeras em células.
//!
//! Cada card ocupa `w × h` células e tem tamanho **próprio**: redimensionar um
//! card (arrastando a borda direita, a de baixo ou o canto) nunca mexe no
//! tamanho dos vizinhos. Se o card cresce sobre outro, o invadido é levado para
//! o espaço livre mais próximo; se não houver, ele encolhe o mínimo necessário
//! ou desce uma linha. Arrastar um card sobre outro troca os dois de lugar, e
//! sobre uma célula vazia move o card para lá.
//!
//! A geometria vive em funções puras ([`solve`], [`relocate`]…), testáveis sem
//! GTK; o resto só liga isso a widgets.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib};
use serde::{Deserialize, Serialize};

/// Número de colunas para `count` câmeras.
///
/// Sem configuração explícita usamos `ceil(sqrt(n))`, que dá o layout esperado
/// de um NVR: 1 câmera → 1×1, 2–4 → 2×2, 5–9 → 3×3, 10–16 → 4×4.
pub fn columns_for(count: usize, configured: Option<usize>) -> usize {
    if let Some(columns) = configured {
        return columns.max(1);
    }
    if count <= 1 {
        return 1;
    }
    (1..).find(|c| c * c >= count).unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Geometria (pura)
// ---------------------------------------------------------------------------

/// Retângulo em células.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }

    fn bottom(&self) -> usize {
        self.y + self.h
    }

    fn fits_in(&self, cols: usize) -> bool {
        self.w >= 1 && self.h >= 1 && self.x + self.w <= cols
    }
}

/// Todos 1×1, preenchendo linha a linha — todo card com o mesmo tamanho.
fn default_layout(count: usize, cols: usize) -> Vec<Rect> {
    (0..count)
        .map(|i| Rect {
            x: i % cols,
            y: i / cols,
            w: 1,
            h: 1,
        })
        .collect()
}

/// Linhas da grade: as necessárias para `count` cards ou para a maior base.
fn rows_for(rects: &[Rect], count: usize, cols: usize) -> usize {
    let needed = count.div_ceil(cols.max(1));
    rects
        .iter()
        .map(Rect::bottom)
        .max()
        .unwrap_or(0)
        .max(needed)
        .max(1)
}

fn is_free(candidate: &Rect, placed: &[Rect]) -> bool {
    !placed.iter().any(|other| other.overlaps(candidate))
}

/// Primeira posição livre (linha a linha) para um card `w × h`.
fn first_free(placed: &[Rect], cols: usize, w: usize, h: usize) -> Rect {
    let w = w.min(cols).max(1);
    let limit = placed.iter().map(Rect::bottom).max().unwrap_or(0);
    for y in 0..=limit {
        for x in 0..=cols - w {
            let candidate = Rect { x, y, w, h };
            if is_free(&candidate, placed) {
                return candidate;
            }
        }
    }
    Rect {
        x: 0,
        y: limit,
        w,
        h,
    }
}

/// Acha um lugar para `orig`, que foi invadido por outro card.
///
/// Prefere o tamanho original na posição livre mais próxima (o "lado onde
/// sobrou espaço"); se não cabe em lugar nenhum, encolhe o mínimo possível; só
/// em último caso desce para uma linha nova.
fn relocate(orig: Rect, placed: &[Rect], cols: usize, rows: usize) -> Rect {
    let mut sizes: Vec<(usize, usize)> = (1..=orig.w.min(cols))
        .flat_map(|w| (1..=orig.h).map(move |h| (w, h)))
        .collect();
    // Maior área primeiro; em empate, o que preserva mais largura.
    sizes.sort_by_key(|&(w, h)| (std::cmp::Reverse(w * h), std::cmp::Reverse(w)));

    for (w, h) in sizes {
        let mut best: Option<(usize, Rect)> = None;
        for y in 0..=rows.saturating_sub(h) {
            for x in 0..=cols - w {
                let candidate = Rect { x, y, w, h };
                if !is_free(&candidate, placed) {
                    continue;
                }
                // Mudar de coluna custa menos que de linha: o card "vai para o
                // lado" antes de "descer".
                let cost = x.abs_diff(orig.x) + 2 * y.abs_diff(orig.y);
                if best.is_none_or(|(c, _)| cost < c) {
                    best = Some((cost, candidate));
                }
            }
        }
        if let Some((_, rect)) = best {
            return rect;
        }
    }
    let below = placed.iter().map(Rect::bottom).max().unwrap_or(0).max(rows);
    Rect {
        x: orig.x.min(cols - 1),
        y: below,
        w: orig.w.min(cols).max(1),
        h: orig.h.max(1),
    }
}

/// Aplica `want` ao card `target` e leva embora quem ele invadiu.
///
/// Parte sempre de `base` (o layout no início do arrasto): assim, encolher o
/// card de volta devolve os outros ao lugar de origem. Cards que não foram
/// invadidos nunca se mexem.
fn solve(base: &[Rect], target: usize, want: Rect, cols: usize, rows: usize) -> Vec<Rect> {
    let mut out = base.to_vec();
    out[target] = want;

    let mut displaced: Vec<usize> = (0..base.len())
        .filter(|&i| i != target && base[i].overlaps(&want))
        .collect();
    displaced.sort_by_key(|&i| (base[i].y, base[i].x));

    let mut placed: Vec<Rect> = (0..base.len())
        .filter(|i| !displaced.contains(i))
        .map(|i| out[i])
        .collect();
    for i in displaced {
        let rect = relocate(base[i], &placed, cols, rows);
        out[i] = rect;
        placed.push(rect);
    }
    out
}

// ---------------------------------------------------------------------------
// Persistência
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedCard {
    key: String,
    #[serde(flatten)]
    rect: Rect,
}

/// Layout salvo: onde e com que tamanho cada câmera ficou.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedLayout {
    /// Formato do arquivo; layouts de versões antigas (painéis) são descartados.
    #[serde(default)]
    version: u32,
    columns: usize,
    #[serde(default)]
    cards: Vec<SavedCard>,
}

/// Versão atual: 4 = células com tamanho por card.
const LAYOUT_VERSION: u32 = 4;

fn layout_path() -> Option<PathBuf> {
    crate::config::user_config_dir().map(|dir| dir.join("nvr-dashboard").join("layout.toml"))
}

fn load_layout() -> SavedLayout {
    layout_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<SavedLayout>(&text).ok())
        .filter(|layout| layout.version == LAYOUT_VERSION)
        .unwrap_or_default()
}

fn save_layout(layout: &SavedLayout) {
    let Some(path) = layout_path() else { return };
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, toml::to_string(layout).unwrap_or_default()));
    if let Err(err) = result {
        tracing::warn!(%err, "não consegui salvar o layout do grid");
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// Um card no grid.
struct Entry {
    /// Chave estável entre execuções (`<dispositivo>/<canal>`).
    key: String,
    /// Id da câmera nesta execução.
    id: usize,
    widget: gtk::Widget,
    rect: Rect,
}

struct Shared {
    configured_columns: Option<usize>,
    entries: RefCell<Vec<Entry>>,
    /// Posições salvas (ou da última execução do layout), por chave.
    saved: RefCell<HashMap<String, Rect>>,
    /// Colunas a que `saved` se refere.
    saved_columns: Cell<usize>,
    /// Colunas em uso agora.
    columns: Cell<usize>,
    /// Linhas em uso agora.
    rows: Cell<usize>,
    grid: gtk::Grid,
    /// Ocupa a última célula, só para a grade manter todas as linhas/colunas
    /// (grades vazias no fim seriam descartadas e as células cresceriam).
    spacer: gtk::Box,
    /// Adiar o cálculo enquanto várias câmeras entram de uma vez.
    batching: Cell<bool>,
    save_generation: Cell<u64>,
}

impl Shared {
    /// Salva 500 ms depois da última mudança, para não gravar a cada passo.
    fn schedule_save(self: &Rc<Self>) {
        let generation = self.save_generation.get() + 1;
        self.save_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if this.save_generation.get() == generation {
                save_layout(&SavedLayout {
                    version: LAYOUT_VERSION,
                    columns: this.columns.get(),
                    cards: this
                        .entries
                        .borrow()
                        .iter()
                        .map(|e| SavedCard {
                            key: e.key.clone(),
                            rect: e.rect,
                        })
                        .collect(),
                });
            }
        });
    }

    fn rects(&self) -> Vec<Rect> {
        self.entries.borrow().iter().map(|e| e.rect).collect()
    }

    /// Decide as células de cada card: as salvas quando ainda valem, senão o
    /// padrão (todos do mesmo tamanho); depois joga na tela.
    fn relayout(self: &Rc<Self>) {
        let count = self.entries.borrow().len();
        if count == 0 {
            self.apply();
            return;
        }
        let cols = columns_for(count, self.configured_columns);
        self.columns.set(cols);

        let defaults = default_layout(count, cols);
        let keep_layout = self.saved_columns.get() == cols;
        let mut placed: Vec<Rect> = Vec::with_capacity(count);
        let mut chosen: Vec<Option<Rect>> = vec![None; count];

        if keep_layout {
            let saved = self.saved.borrow();
            for (i, entry) in self.entries.borrow().iter().enumerate() {
                if let Some(rect) = saved.get(&entry.key).copied()
                    && rect.fits_in(cols)
                    && is_free(&rect, &placed)
                {
                    chosen[i] = Some(rect);
                    placed.push(rect);
                }
            }
        }
        for i in 0..count {
            if chosen[i].is_some() {
                continue;
            }
            // Câmera nova (ou layout descartado): entra 1×1 na posição natural
            // se estiver livre, senão na primeira célula livre.
            let rect = if is_free(&defaults[i], &placed) {
                defaults[i]
            } else {
                first_free(&placed, cols, 1, 1)
            };
            chosen[i] = Some(rect);
            placed.push(rect);
        }
        for (entry, rect) in self.entries.borrow_mut().iter_mut().zip(chosen) {
            entry.rect = rect.expect("toda entrada recebeu uma célula");
        }
        self.remember();
        self.apply();
    }

    /// Guarda o layout atual como referência para os próximos `relayout`.
    ///
    /// Mescla em vez de substituir: a posição de uma câmera retirada por
    /// instantes (edição, reconexão do cadastro) sobrevive até ela voltar. Com
    /// número de colunas novo, o que havia deixa de valer.
    fn remember(&self) {
        let mut saved = self.saved.borrow_mut();
        if self.saved_columns.get() != self.columns.get() {
            saved.clear();
        }
        saved.extend(
            self.entries
                .borrow()
                .iter()
                .map(|e| (e.key.clone(), e.rect)),
        );
        self.saved_columns.set(self.columns.get());
    }

    /// Reflete as células de cada entrada na `gtk::Grid`, sem reparentar (um
    /// arrasto de redimensionamento em curso vive dentro do widget).
    fn apply(&self) {
        let entries = self.entries.borrow();
        let cols = self.columns.get().max(1);
        let rows = rows_for(&self.rects(), entries.len(), cols);
        self.rows.set(rows);

        for entry in entries.iter() {
            let Rect { x, y, w, h } = entry.rect;
            if entry.widget.parent().is_none() {
                self.grid
                    .attach(&entry.widget, x as i32, y as i32, w as i32, h as i32);
            } else if let Some(child) = self
                .grid
                .layout_manager()
                .and_then(|manager| manager.downcast::<gtk::GridLayout>().ok())
                .and_then(|layout| {
                    layout
                        .layout_child(&entry.widget)
                        .downcast::<gtk::GridLayoutChild>()
                        .ok()
                })
            {
                child.set_column(x as i32);
                child.set_row(y as i32);
                child.set_column_span(w as i32);
                child.set_row_span(h as i32);
            }
        }

        if self.spacer.parent().is_some() {
            self.grid.remove(&self.spacer);
        }
        if !entries.is_empty() {
            self.grid
                .attach(&self.spacer, (cols - 1) as i32, (rows - 1) as i32, 1, 1);
        }
    }

    /// Índice da entrada com este id.
    fn index_of(&self, id: usize) -> Option<usize> {
        self.entries.borrow().iter().position(|e| e.id == id)
    }

    fn set_rects(self: &Rc<Self>, rects: &[Rect]) {
        for (entry, rect) in self.entries.borrow_mut().iter_mut().zip(rects) {
            entry.rect = *rect;
        }
        self.remember();
        self.apply();
    }

    /// Troca de lugar (e de tamanho) as câmeras `from` e `to` e persiste.
    fn swap(self: &Rc<Self>, from: usize, to: usize) {
        let (Some(a), Some(b)) = (self.index_of(from), self.index_of(to)) else {
            return;
        };
        if a == b {
            return;
        }
        let mut rects = self.rects();
        rects.swap(a, b);
        self.set_rects(&rects);
        self.schedule_save();
    }

    /// Leva a câmera `id` para a célula (`cx`, `cy`), empurrando quem estiver lá.
    fn move_to_cell(self: &Rc<Self>, id: usize, cx: usize, cy: usize) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let base = self.rects();
        let cols = self.columns.get();
        let rows = self.rows.get();
        let current = base[index];
        let want = Rect {
            x: cx.min(cols - current.w.min(cols)),
            y: cy.min(rows.saturating_sub(current.h)),
            ..current
        };
        if want == current {
            return;
        }
        let solved = solve(&base, index, want, cols, rows);
        self.set_rects(&solved);
        self.schedule_save();
    }

    /// Tamanho de uma célula em pixels, para converter o ponteiro em células.
    fn cell_size(&self) -> (f64, f64) {
        (
            f64::from(self.grid.width()) / self.columns.get().max(1) as f64,
            f64::from(self.grid.height()) / self.rows.get().max(1) as f64,
        )
    }
}

/// Estado de um arrasto de redimensionamento em curso.
struct ResizeDrag {
    /// Layout no início do arrasto: toda atualização parte dele.
    base: Vec<Rect>,
    /// Tamanho de uma célula em pixels no início.
    cell: (f64, f64),
    /// Índice do card sendo redimensionado.
    index: usize,
    edge: Edge,
}

/// Qual borda a alça controla.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Right,
    Bottom,
    Corner,
}

impl Edge {
    fn tag(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Corner => "corner",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            "corner" => Some(Self::Corner),
            _ => None,
        }
    }
}

const HANDLE_PREFIX: &str = "resize-handle:";

/// Adiciona ao card as três alças de redimensionamento (borda direita, borda de
/// baixo e canto). Elas só se identificam (pelo nome do widget) e mostram o
/// cursor; quem trata o arrasto é [`install_resize_gesture`].
fn install_resize_handles(id: usize, tile: &gtk::Widget) {
    let Some(overlay) = tile.downcast_ref::<gtk::Overlay>() else {
        return;
    };
    for (edge, class, cursor, halign, valign) in [
        (
            Edge::Right,
            "resize-right",
            "e-resize",
            gtk::Align::End,
            gtk::Align::Fill,
        ),
        (
            Edge::Bottom,
            "resize-bottom",
            "s-resize",
            gtk::Align::Fill,
            gtk::Align::End,
        ),
        (
            Edge::Corner,
            "resize-corner",
            "se-resize",
            gtk::Align::End,
            gtk::Align::End,
        ),
    ] {
        let handle = gtk::Box::builder()
            .name(format!("{HANDLE_PREFIX}{id}:{}", edge.tag()))
            .css_classes(["resize-handle", class])
            .halign(halign)
            .valign(valign)
            .build();
        handle.set_cursor_from_name(Some(cursor));
        overlay.add_overlay(&handle);
        overlay.set_measure_overlay(&handle, false);
    }
}

/// Um único gesto de arrasto na própria grade trata todas as alças.
///
/// Fica na grade (que não se mexe) e não em cada alça: um card que cresce
/// desloca o widget sob o ponteiro, e as coordenadas relativas a ele deixariam
/// de refletir o movimento real do mouse.
fn install_resize_gesture(shared: &Rc<Shared>) {
    let drag = gtk::GestureDrag::builder()
        .button(1)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let state: Rc<RefCell<Option<ResizeDrag>>> = Rc::default();

    {
        let (shared, state) = (Rc::downgrade(shared), Rc::clone(&state));
        drag.connect_drag_begin(move |gesture, x, y| {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let hit = shared
                .grid
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .and_then(|widget| {
                    let name = widget.widget_name();
                    let rest = name.strip_prefix(HANDLE_PREFIX)?.to_string();
                    let (id, edge) = rest.split_once(':')?;
                    Some((id.parse::<usize>().ok()?, Edge::from_tag(edge)?))
                });
            let Some((id, edge)) = hit else {
                // Não foi numa alça: deixa o clique/arrastar normais seguirem.
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            };
            let Some(index) = shared.index_of(id) else {
                return;
            };
            // Reivindica a sequência: impede o clique (abrir em tela cheia) e o
            // arrastar-para-reordenar de dispararem junto.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            *state.borrow_mut() = Some(ResizeDrag {
                base: shared.rects(),
                cell: shared.cell_size(),
                index,
                edge,
            });
        });
    }
    {
        let (shared, state) = (Rc::downgrade(shared), Rc::clone(&state));
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let solved = {
                let guard = state.borrow();
                let Some(drag) = guard.as_ref() else { return };
                let (cell_w, cell_h) = drag.cell;
                if cell_w < 1.0 || cell_h < 1.0 {
                    return;
                }
                let dw = (offset_x / cell_w).round() as isize;
                let dh = (offset_y / cell_h).round() as isize;

                let base = &drag.base;
                let cols = shared.columns.get();
                let rows = rows_for(base, base.len(), cols);
                let from = base[drag.index];
                let grow = |value: usize, delta: isize, max: usize| {
                    (value as isize + delta).clamp(1, max.max(1) as isize) as usize
                };
                let want = Rect {
                    w: if drag.edge == Edge::Bottom {
                        from.w
                    } else {
                        grow(from.w, dw, cols - from.x)
                    },
                    h: if drag.edge == Edge::Right {
                        from.h
                    } else {
                        grow(from.h, dh, rows - from.y)
                    },
                    ..from
                };
                solve(base, drag.index, want, cols, rows)
            };
            // Só mexe na tela quando o encaixe em células muda de fato.
            if solved != shared.rects() {
                shared.set_rects(&solved);
            }
        });
    }
    {
        let (shared, state) = (Rc::downgrade(shared), state);
        drag.connect_drag_end(move |_, _, _| {
            if state.borrow_mut().take().is_some()
                && let Some(shared) = shared.upgrade()
            {
                shared.schedule_save();
            }
        });
    }
    shared.grid.add_controller(drag);
}

/// Permite arrastar o card da câmera `id` e soltar sobre outro (troca os dois).
fn enable_reordering(shared: &Rc<Shared>, id: usize, tile: &gtk::Widget) {
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .content(&gdk::ContentProvider::for_value(&(id as u32).to_value()))
        .build();
    source.connect_drag_begin(|source, drag| {
        if let Some(widget) = source.widget() {
            let icon = gtk::WidgetPaintable::new(Some(&widget));
            if let Ok(icon_widget) = gtk::DragIcon::for_drag(drag).downcast::<gtk::DragIcon>() {
                icon_widget.set_child(Some(&gtk::Picture::for_paintable(&icon)));
            }
        }
    });
    tile.add_controller(source);

    let target = gtk::DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);
    {
        let tile = tile.clone();
        target.connect_enter(move |_, _, _| {
            tile.add_css_class("drop-target");
            gdk::DragAction::MOVE
        });
    }
    {
        let tile = tile.clone();
        target.connect_leave(move |_| tile.remove_css_class("drop-target"));
    }
    {
        // Referência fraca: o controlador vive dentro do card, que o `Shared`
        // segura — uma forte formaria um ciclo que impediria a liberação.
        let shared = Rc::downgrade(shared);
        let tile = tile.clone();
        target.connect_drop(move |_, value, _, _| {
            tile.remove_css_class("drop-target");
            let Ok(from) = value.get::<u32>() else {
                return false;
            };
            // Adiado: mexer na grade dentro do próprio evento de drop mexeria
            // no widget que ainda está tratando o evento.
            let shared = shared.clone();
            glib::idle_add_local_once(move || {
                if let Some(shared) = shared.upgrade() {
                    shared.swap(from as usize, id);
                }
            });
            true
        });
    }
    tile.add_controller(target);
}

/// Grade de câmeras.
pub struct GridView {
    shared: Rc<Shared>,
}

impl GridView {
    pub fn new(configured_columns: Option<usize>) -> Self {
        let saved = load_layout();
        let grid = gtk::Grid::builder()
            .css_classes(["camera-grid"])
            .row_homogeneous(true)
            .column_homogeneous(true)
            .row_spacing(8)
            .column_spacing(8)
            .hexpand(true)
            .vexpand(true)
            .build();
        let spacer = gtk::Box::builder().can_target(false).build();
        let shared = Rc::new(Shared {
            configured_columns,
            entries: RefCell::new(Vec::new()),
            saved: RefCell::new(
                saved
                    .cards
                    .into_iter()
                    .map(|card| (card.key, card.rect))
                    .collect(),
            ),
            saved_columns: Cell::new(saved.columns),
            columns: Cell::new(1),
            rows: Cell::new(1),
            grid: grid.clone(),
            spacer,
            batching: Cell::new(false),
            save_generation: Cell::new(0),
        });

        install_resize_gesture(&shared);

        // Soltar um card numa célula vazia move o card para lá.
        let target = gtk::DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);
        {
            let shared = Rc::downgrade(&shared);
            target.connect_drop(move |_, value, x, y| {
                let (Some(shared), Ok(id)) = (shared.upgrade(), value.get::<u32>()) else {
                    return false;
                };
                let (cell_w, cell_h) = shared.cell_size();
                if cell_w < 1.0 || cell_h < 1.0 {
                    return false;
                }
                let (cx, cy) = ((x / cell_w) as usize, (y / cell_h) as usize);
                glib::idle_add_local_once(move || shared.move_to_cell(id as usize, cx, cy));
                true
            });
        }
        grid.add_controller(target);

        Self { shared }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.shared.grid.clone().upcast()
    }

    /// Enquanto ativo, `add` não recalcula o layout: use ao inserir várias
    /// câmeras de uma vez e chame `set_batch(false)` no fim.
    pub fn set_batch(&self, on: bool) {
        self.shared.batching.set(on);
        if !on {
            self.shared.relayout();
        }
    }

    /// Acrescenta um card. `key` identifica a câmera entre execuções.
    pub fn add(&self, key: String, id: usize, widget: gtk::Widget) {
        enable_reordering(&self.shared, id, &widget);
        install_resize_handles(id, &widget);
        self.shared.entries.borrow_mut().push(Entry {
            key,
            id,
            widget,
            rect: Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
        });
        if !self.shared.batching.get() {
            self.shared.relayout();
            self.shared.schedule_save();
        }
    }

    /// Tira o card da câmera `id`. Os demais ficam onde estão.
    pub fn remove(&self, id: usize) {
        let removed = {
            let mut entries = self.shared.entries.borrow_mut();
            entries
                .iter()
                .position(|e| e.id == id)
                .map(|index| entries.remove(index))
        };
        if let Some(entry) = removed {
            self.shared.grid.remove(&entry.widget);
            // Em lote (troca de vários cards), o layout é refeito uma vez no fim.
            if !self.shared.batching.get() {
                self.shared.relayout();
                self.shared.schedule_save();
            }
        }
    }

    /// Faz a câmera de chave `new` herdar a posição/tamanho de `old` (a chave
    /// muda quando o endereço do dispositivo é editado).
    pub fn alias_key(&self, old: &str, new: &str) {
        let rect = self.shared.saved.borrow().get(old).copied();
        if let Some(rect) = rect {
            self.shared.saved.borrow_mut().insert(new.to_string(), rect);
        }
    }

    /// Ids das câmeras na ordem de leitura (cima→baixo, esquerda→direita).
    pub fn ids(&self) -> Vec<usize> {
        let mut entries: Vec<(usize, usize, usize)> = self
            .shared
            .entries
            .borrow()
            .iter()
            .map(|e| (e.rect.y, e.rect.x, e.id))
            .collect();
        entries.sort_unstable();
        entries.into_iter().map(|(_, _, id)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    fn no_overlap(rects: &[Rect]) -> bool {
        rects
            .iter()
            .enumerate()
            .all(|(i, a)| rects.iter().skip(i + 1).all(|b| !a.overlaps(b)))
    }

    #[test]
    fn layout_automatico_segue_ceil_sqrt() {
        assert_eq!(columns_for(0, None), 1);
        assert_eq!(columns_for(1, None), 1);
        assert_eq!(columns_for(2, None), 2);
        assert_eq!(columns_for(3, None), 2, "3 câmeras cabem num 2×2");
        assert_eq!(columns_for(4, None), 2);
        assert_eq!(columns_for(5, None), 3);
        assert_eq!(columns_for(9, None), 3);
        assert_eq!(columns_for(10, None), 4);
        assert_eq!(columns_for(16, None), 4);
    }

    #[test]
    fn configuracao_explicita_tem_precedencia() {
        assert_eq!(columns_for(3, Some(3)), 3);
        assert_eq!(columns_for(9, Some(1)), 1);
        assert_eq!(columns_for(9, Some(0)), 1, "0 é normalizado para 1");
    }

    #[test]
    fn layout_padrao_tem_todos_do_mesmo_tamanho() {
        let layout = default_layout(3, 2);
        assert_eq!(layout, vec![r(0, 0, 1, 1), r(1, 0, 1, 1), r(0, 1, 1, 1)]);
        assert!(layout.iter().all(|c| c.w == 1 && c.h == 1));
    }

    #[test]
    fn crescer_um_card_nao_mexe_no_vizinho_da_mesma_linha() {
        // O caso do relatório: o card 0 vira 1×2; o card 1 (ao lado) NÃO muda.
        let base = default_layout(3, 2);
        let solved = solve(&base, 0, r(0, 0, 1, 2), 2, 2);
        assert_eq!(solved[0], r(0, 0, 1, 2));
        assert_eq!(solved[1], base[1], "vizinho intacto");
    }

    #[test]
    fn card_invadido_vai_para_o_lado_onde_sobrou_espaco() {
        // 0 cresce até o fundo e cobre o card 2, que estava embaixo dele. O
        // espaço livre está do outro lado, na coluna 1.
        let base = default_layout(3, 2);
        let solved = solve(&base, 0, r(0, 0, 1, 2), 2, 2);
        assert_eq!(solved[2], r(1, 1, 1, 1));
        assert!(no_overlap(&solved));
    }

    #[test]
    fn encolher_de_volta_devolve_o_card_deslocado() {
        let base = default_layout(3, 2);
        let grown = solve(&base, 0, r(0, 0, 1, 2), 2, 2);
        assert_ne!(grown[2], base[2]);
        // O arrasto sempre recalcula a partir da base.
        let back = solve(&base, 0, r(0, 0, 1, 1), 2, 2);
        assert_eq!(back, base);
    }

    #[test]
    fn sem_lugar_o_card_encolhe_antes_de_descer() {
        // 2 colunas × 1 linha, dois cards. O 0 ocupa tudo: o 1 não tem onde
        // ficar e desce para uma linha nova.
        let base = default_layout(2, 2);
        let solved = solve(&base, 0, r(0, 0, 2, 1), 2, 1);
        assert_eq!(solved[0], r(0, 0, 2, 1));
        assert!(no_overlap(&solved));
        assert_eq!(solved[1].y, 1, "desceu");
    }

    #[test]
    fn card_maior_invadido_encolhe_para_caber() {
        // 3 colunas, 2 linhas. O card 1 é 2×1; o 0 cresce e o invade, mas o
        // resto da linha 0 não comporta 2×1 — só há 1×1 livre em (2,0)...
        let base = vec![r(0, 0, 1, 1), r(1, 0, 2, 1), r(0, 1, 3, 1)];
        let solved = solve(&base, 0, r(0, 0, 2, 1), 3, 2);
        assert!(no_overlap(&solved));
        assert_eq!(solved[0], r(0, 0, 2, 1));
        assert!(solved.iter().all(|c| c.fits_in(3)));
    }

    #[test]
    fn nunca_ha_sobreposicao_em_nenhuma_combinacao() {
        // Varre todos os tamanhos possíveis de cada card, em vários layouts.
        for count in 1..=6 {
            let cols = columns_for(count, None);
            let base = default_layout(count, cols);
            let rows = rows_for(&base, count, cols);
            for target in 0..count {
                for w in 1..=(cols - base[target].x) {
                    for h in 1..=(rows - base[target].y) {
                        let want = Rect {
                            w,
                            h,
                            ..base[target]
                        };
                        let solved = solve(&base, target, want, cols, rows);
                        assert!(
                            no_overlap(&solved),
                            "{count} cards, alvo {target}, {w}×{h}: {solved:?}"
                        );
                        assert_eq!(
                            solved[target], want,
                            "o card redimensionado fica como pedido"
                        );
                        assert!(solved.iter().all(|c| c.fits_in(cols)));
                    }
                }
            }
        }
    }

    #[test]
    fn primeira_celula_livre_e_a_de_menor_linha() {
        let placed = vec![r(0, 0, 1, 1), r(1, 0, 1, 1)];
        assert_eq!(first_free(&placed, 2, 1, 1), r(0, 1, 1, 1));
        assert_eq!(first_free(&[], 2, 1, 1), r(0, 0, 1, 1));
    }

    #[test]
    fn linhas_cobrem_o_maior_card_e_o_minimo_para_a_contagem() {
        assert_eq!(rows_for(&default_layout(3, 2), 3, 2), 2);
        assert_eq!(rows_for(&[r(0, 0, 1, 3)], 1, 1), 3);
        assert_eq!(rows_for(&[], 0, 1), 1);
    }
}
