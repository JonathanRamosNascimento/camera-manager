//! Grid adaptável de câmeras.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::ui::camera_tile::CameraTile;

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

/// Layout persistido: fração da posição de cada divisória (na ordem de criação).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedLayout {
    /// Formato das frações; muda quando a montagem dos painéis muda.
    #[serde(default)]
    version: u32,
    columns: usize,
    count: usize,
    fractions: Vec<f64>,
    /// `order[posição] = índice da câmera` exibida naquela posição do grid.
    #[serde(default)]
    order: Vec<usize>,
}

/// Versão atual: 2 = linhas incompletas são completadas com espaços vazios.
const LAYOUT_VERSION: u32 = 2;

fn layout_path() -> Option<PathBuf> {
    crate::config::user_config_dir().map(|dir| dir.join("nvr-dashboard").join("layout.toml"))
}

fn load_layout(columns: usize, count: usize) -> SavedLayout {
    let saved = layout_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<SavedLayout>(&text).ok());
    // Mudou o nº de câmeras ou de colunas: as frações antigas não valem.
    let mut layout = match saved {
        Some(layout)
            if layout.version == LAYOUT_VERSION
                && layout.columns == columns
                && layout.count == count =>
        {
            layout
        }
        _ => SavedLayout::default(),
    };
    // A ordem só vale se for uma permutação de 0..count.
    let mut sorted = layout.order.clone();
    sorted.sort_unstable();
    if !sorted.iter().copied().eq(0..count) {
        layout.order = (0..count).collect();
    }
    layout
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

/// Estado compartilhado pelo grid: ordem, frações, e a árvore de painéis atual.
struct Shared {
    columns: usize,
    count: usize,
    /// Um widget por câmera, indexado pelo id da câmera.
    tiles: Vec<gtk::Widget>,
    /// `order[posição] = índice da câmera`.
    order: RefCell<Vec<usize>>,
    fractions: RefCell<Vec<f64>>,
    /// Contêiner fixo onde a árvore de painéis é (re)montada.
    host: gtk::Box,
    paneds: RefCell<Vec<gtk::Paned>>,
    save_generation: Cell<u64>,
}

impl Shared {
    /// Salva 500 ms depois da última mudança, para não gravar a cada pixel.
    fn schedule_save(self: &Rc<Self>) {
        let generation = self.save_generation.get() + 1;
        self.save_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if this.save_generation.get() == generation {
                save_layout(&SavedLayout {
                    version: LAYOUT_VERSION,
                    columns: this.columns,
                    count: this.count,
                    fractions: this.fractions.borrow().clone(),
                    order: this.order.borrow().clone(),
                });
            }
        });
    }

    /// (Re)monta a árvore de painéis na ordem atual.
    ///
    /// Os tiles são soltos dos painéis antigos antes de serem reaproveitados;
    /// as frações ficam presas ao *slot*, então cada slot mantém seu tamanho.
    fn render(self: &Rc<Self>) {
        for paned in self.paneds.borrow_mut().drain(..) {
            paned.set_start_child(gtk::Widget::NONE);
            paned.set_end_child(gtk::Widget::NONE);
        }
        while let Some(child) = self.host.first_child() {
            self.host.remove(&child);
        }

        let ordered: Vec<gtk::Widget> = self
            .order
            .borrow()
            .iter()
            .map(|&id| self.tiles[id].clone())
            .collect();
        let index = Cell::new(0);
        // Divisórias das linhas primeiro, depois a vertical: a ordem precisa ser
        // sempre a mesma para as frações salvas casarem.
        // A última linha é completada com espaços vazios: assim, na primeira
        // abertura, todo card tem o mesmo tamanho (um card sozinho não se
        // estica pela linha inteira). O usuário pode arrastar a divisória
        // para ocupar o espaço.
        let rows: Vec<gtk::Widget> = ordered
            .chunks(self.columns)
            .map(|row| {
                let mut cells = row.to_vec();
                while cells.len() < self.columns {
                    cells.push(
                        gtk::Box::builder()
                            .hexpand(true)
                            .vexpand(true)
                            .build()
                            .upcast(),
                    );
                }
                nest(&cells, gtk::Orientation::Horizontal, self, &index)
            })
            .collect();
        self.host
            .append(&nest(&rows, gtk::Orientation::Vertical, self, &index));
    }

    /// Troca de lugar as câmeras `from` e `to` (ids) e persiste.
    fn swap(self: &Rc<Self>, from: usize, to: usize) {
        if from == to {
            return;
        }
        {
            let mut order = self.order.borrow_mut();
            let (Some(a), Some(b)) = (
                order.iter().position(|&id| id == from),
                order.iter().position(|&id| id == to),
            ) else {
                return;
            };
            order.swap(a, b);
        }
        self.render();
        self.schedule_save();
    }
}

/// Permite arrastar `tile` (pelo id) e soltar sobre outro para trocar os dois.
fn enable_reordering(shared: &Rc<Shared>, id: usize) {
    let tile = &shared.tiles[id];

    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .content(&gdk::ContentProvider::for_value(&(id as u32).to_value()))
        .build();
    source.connect_drag_begin(|source, drag| {
        if let Some(widget) = source.widget() {
            let icon = gtk::WidgetPaintable::new(Some(&widget));
            gtk::DragIcon::for_drag(drag)
                .downcast::<gtk::DragIcon>()
                .ok()
                .map(|icon_widget| {
                    icon_widget.set_child(Some(&gtk::Picture::for_paintable(&icon)));
                });
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
        let shared = Rc::clone(shared);
        let tile = tile.clone();
        target.connect_drop(move |_, value, _, _| {
            tile.remove_css_class("drop-target");
            let Ok(from) = value.get::<u32>() else {
                return false;
            };
            // Adiado: montar a árvore de novo dentro do próprio evento de drop
            // desparentaria o widget que ainda está tratando o evento.
            let shared = Rc::clone(&shared);
            glib::idle_add_local_once(move || shared.swap(from as usize, id));
            true
        });
    }
    tile.add_controller(target);
}

/// Encadeia `items` em `gtk::Paned` aninhados: o 1º item ocupa a fração `f` do
/// espaço e o resto vai para outro `Paned`. Cada divisória arrastável redimensiona
/// só os vizinhos dela.
fn nest(
    items: &[gtk::Widget],
    orientation: gtk::Orientation,
    shared: &Rc<Shared>,
    index: &Cell<usize>,
) -> gtk::Widget {
    let [first, rest @ ..] = items else {
        unreachable!("nest chamado sem itens");
    };
    if rest.is_empty() {
        return first.clone();
    }

    let slot = index.get();
    index.set(slot + 1);
    {
        let mut fractions = shared.fractions.borrow_mut();
        if fractions.len() <= slot {
            fractions.resize(slot + 1, 0.0);
        }
        // Valor ausente/inválido → divisão igual entre os itens que restam.
        if !(0.05..=0.95).contains(&fractions[slot]) {
            fractions[slot] = 1.0 / items.len() as f64;
        }
    }

    let paned = gtk::Paned::builder()
        .orientation(orientation)
        .wide_handle(true)
        .hexpand(true)
        .vexpand(true)
        .resize_start_child(true)
        .resize_end_child(true)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .start_child(first)
        .end_child(&nest(rest, orientation, shared, index))
        .build();
    shared.paneds.borrow_mut().push(paned.clone());

    let horizontal = orientation == gtk::Orientation::Horizontal;
    let size_of = move |paned: &gtk::Paned| if horizontal { paned.width() } else { paned.height() };
    let applying = Rc::new(Cell::new(false));
    let last_size = Rc::new(Cell::new(0));

    // A cada mudança de tamanho da janela, reaplica a fração guardada: o
    // `Paned` sozinho não mantém a proporção.
    {
        let shared = Rc::clone(shared);
        let applying = Rc::clone(&applying);
        let last_size = Rc::clone(&last_size);
        paned.add_tick_callback(move |paned, _| {
            let size = size_of(paned);
            if size > 0 && size != last_size.get() {
                last_size.set(size);
                applying.set(true);
                paned.set_position((size as f64 * shared.fractions.borrow()[slot]).round() as i32);
                applying.set(false);
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let shared = Rc::clone(shared);
        paned.connect_position_notify(move |paned| {
            let size = size_of(paned);
            // Só vale o que o usuário arrastou: ignora o layout inicial do GTK
            // (antes de aplicarmos a fração) e ajustes durante redimensionamento
            // da janela (o tamanho ainda não foi reaplicado pelo tick).
            if applying.get() || size <= 0 || last_size.get() != size {
                return;
            }
            let fraction = (paned.position() as f64 / size as f64).clamp(0.05, 0.95);
            shared.fractions.borrow_mut()[slot] = fraction;
            shared.schedule_save();
        });
    }

    paned.upcast()
}

/// Monta a grade de câmeras com divisórias arrastáveis entre os tiles.
///
/// Linhas e colunas seguem o mesmo cálculo de [`columns_for`]. Arrastar um tile
/// sobre outro troca os dois de lugar. Proporções e ordem escolhidas pelo
/// usuário são salvas em `<config>/nvr-dashboard/layout.toml`.
pub fn build(tiles: &[CameraTile], configured_columns: Option<usize>) -> gtk::Widget {
    let columns = columns_for(tiles.len(), configured_columns);
    let saved = load_layout(columns, tiles.len());

    let host = gtk::Box::builder()
        .css_classes(["camera-grid"])
        .hexpand(true)
        .vexpand(true)
        .build();
    let shared = Rc::new(Shared {
        columns,
        count: tiles.len(),
        tiles: tiles.iter().map(|t| t.widget().clone().upcast()).collect(),
        order: RefCell::new(saved.order),
        fractions: RefCell::new(saved.fractions),
        host: host.clone(),
        paneds: RefCell::new(Vec::new()),
        save_generation: Cell::new(0),
    });

    for id in 0..tiles.len() {
        enable_reordering(&shared, id);
    }
    shared.render();
    host.upcast()
}

#[cfg(test)]
mod tests {
    use super::columns_for;

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
}
