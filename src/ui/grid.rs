//! Grid dinâmico de câmeras: cards que entram e saem em tempo de execução,
//! com divisórias arrastáveis entre eles e ordem alterável por arrastar e soltar.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib};
use gtk::prelude::*;
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

/// Layout persistido: fração da posição de cada divisória (na ordem de criação)
/// e a ordem das câmeras.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedLayout {
    /// Formato do arquivo; muda quando a montagem dos painéis ou a ordem mudam.
    #[serde(default)]
    version: u32,
    columns: usize,
    count: usize,
    fractions: Vec<f64>,
    /// Chaves das câmeras (`<dispositivo>/<canal>`) na ordem de exibição.
    #[serde(default)]
    order: Vec<String>,
}

/// Versão atual: 3 = ordem por chave de câmera (antes era por índice).
const LAYOUT_VERSION: u32 = 3;

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

/// Um card no grid.
struct Entry {
    /// Chave estável entre execuções (`<dispositivo>/<canal>`).
    key: String,
    /// Id da câmera nesta execução.
    id: usize,
    widget: gtk::Widget,
}

/// Estado compartilhado: entradas, frações, e a árvore de painéis atual.
struct Shared {
    configured_columns: Option<usize>,
    /// Entradas na ordem de exibição.
    entries: RefCell<Vec<Entry>>,
    /// Ordem salva; cameras novas (fora da lista) entram no fim.
    order: RefCell<Vec<String>>,
    /// Frações das divisórias, alinhadas por slot.
    fractions: RefCell<Vec<f64>>,
    /// `(colunas, câmeras)` a que `fractions` se refere.
    shape: Cell<(usize, usize)>,
    /// Contêiner fixo onde a árvore de painéis é (re)montada.
    host: gtk::Box,
    paneds: RefCell<Vec<gtk::Paned>>,
    save_generation: Cell<u64>,
}

impl Shared {
    fn columns(&self) -> usize {
        columns_for(self.entries.borrow().len(), self.configured_columns)
    }

    /// Salva 500 ms depois da última mudança, para não gravar a cada pixel.
    fn schedule_save(self: &Rc<Self>) {
        let generation = self.save_generation.get() + 1;
        self.save_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if this.save_generation.get() == generation {
                let (columns, count) = this.shape.get();
                save_layout(&SavedLayout {
                    version: LAYOUT_VERSION,
                    columns,
                    count,
                    fractions: this.fractions.borrow().clone(),
                    order: this.entries.borrow().iter().map(|e| e.key.clone()).collect(),
                });
            }
        });
    }

    /// Ordena as entradas conforme a ordem salva; as desconhecidas ficam no fim,
    /// na ordem em que chegaram.
    fn sort_entries(&self) {
        let order = self.order.borrow();
        self.entries.borrow_mut().sort_by_key(|entry| {
            order
                .iter()
                .position(|key| *key == entry.key)
                .unwrap_or(usize::MAX)
        });
    }

    /// (Re)monta a árvore de painéis na ordem atual.
    ///
    /// Os cards são soltos dos painéis antigos antes de serem reaproveitados;
    /// as frações ficam presas ao *slot*, então cada slot mantém seu tamanho.
    fn render(self: &Rc<Self>) {
        for paned in self.paneds.borrow_mut().drain(..) {
            paned.set_start_child(gtk::Widget::NONE);
            paned.set_end_child(gtk::Widget::NONE);
        }
        while let Some(child) = self.host.first_child() {
            self.host.remove(&child);
        }

        let count = self.entries.borrow().len();
        if count == 0 {
            return;
        }
        let columns = self.columns();
        // Mudou o nº de câmeras ou de colunas: as frações antigas não valem e
        // todo card volta ao mesmo tamanho.
        if self.shape.get() != (columns, count) {
            self.shape.set((columns, count));
            self.fractions.borrow_mut().clear();
        }

        let ordered: Vec<gtk::Widget> = self
            .entries
            .borrow()
            .iter()
            .map(|entry| entry.widget.clone())
            .collect();
        let index = Cell::new(0);
        // A última linha é completada com espaços vazios: assim, na primeira
        // abertura, todo card tem o mesmo tamanho (um card sozinho não se
        // estica pela linha inteira). O usuário pode arrastar a divisória
        // para ocupar o espaço.
        let rows: Vec<gtk::Widget> = ordered
            .chunks(columns)
            .map(|row| {
                let mut cells = row.to_vec();
                while cells.len() < columns {
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
            let mut entries = self.entries.borrow_mut();
            let (Some(a), Some(b)) = (
                entries.iter().position(|e| e.id == from),
                entries.iter().position(|e| e.id == to),
            ) else {
                return;
            };
            entries.swap(a, b);
            *self.order.borrow_mut() = entries.iter().map(|e| e.key.clone()).collect();
        }
        self.render();
        self.schedule_save();
    }
}

/// Permite arrastar o card da câmera `id` e soltar sobre outro para trocar os dois.
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
            // Adiado: montar a árvore de novo dentro do próprio evento de drop
            // desparentaria o widget que ainda está tratando o evento.
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
                let fraction = shared.fractions.borrow().get(slot).copied().unwrap_or(0.5);
                paned.set_position((size as f64 * fraction).round() as i32);
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
            if let Some(saved) = shared.fractions.borrow_mut().get_mut(slot) {
                *saved = fraction;
            }
            shared.schedule_save();
        });
    }

    paned.upcast()
}

/// Grade de câmeras com divisórias arrastáveis entre os cards.
///
/// Linhas e colunas seguem [`columns_for`]. Arrastar um card sobre outro troca
/// os dois de lugar. Proporções e ordem escolhidas pelo usuário são salvas em
/// `<config>/nvr-dashboard/layout.toml`.
pub struct GridView {
    shared: Rc<Shared>,
}

impl GridView {
    pub fn new(configured_columns: Option<usize>) -> Self {
        let saved = load_layout();
        let host = gtk::Box::builder()
            .css_classes(["camera-grid"])
            .hexpand(true)
            .vexpand(true)
            .build();
        let shape = (saved.columns, saved.count);
        Self {
            shared: Rc::new(Shared {
                configured_columns,
                entries: RefCell::new(Vec::new()),
                order: RefCell::new(saved.order),
                fractions: RefCell::new(saved.fractions),
                shape: Cell::new(shape),
                host,
                paneds: RefCell::new(Vec::new()),
                save_generation: Cell::new(0),
            }),
        }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.shared.host.clone().upcast()
    }

    /// Acrescenta um card. `key` identifica a câmera entre execuções.
    pub fn add(&self, key: String, id: usize, widget: gtk::Widget) {
        enable_reordering(&self.shared, id, &widget);
        self.shared.entries.borrow_mut().push(Entry { key, id, widget });
        self.shared.sort_entries();
        self.shared.render();
    }

    /// Tira o card da câmera `id`. As demais mantêm ordem e posição relativa.
    pub fn remove(&self, id: usize) {
        self.shared.entries.borrow_mut().retain(|e| e.id != id);
        self.shared.render();
        self.shared.schedule_save();
    }

    /// Ids das câmeras na ordem de exibição (esquerda→direita, cima→baixo).
    pub fn ids(&self) -> Vec<usize> {
        self.shared.entries.borrow().iter().map(|e| e.id).collect()
    }
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
