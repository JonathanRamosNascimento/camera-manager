//! Identificação de objetos (estilo YOLO) nas câmeras.
//!
//! Cada câmera liga ou desliga a detecção e escolhe quais classes reconhecer
//! ([`DetectionSettings`], gravada no cadastro). O que é comum a todas — modelo,
//! taxa de análise, cooldown — vem do bloco `[detection]` do `cameras.toml`.
//!
//! Fluxo:
//! ```text
//!  pipeline (pad probe, ~2 quadros/s) ──▶ Engine ──▶ workers (tract) ──▶ DetectionState
//!                                                                          │
//!            UI (caixas, selo)  ◀── snapshot() ───────────────────────────┤
//!            supervisor ── take_alerts() ──▶ evento ──▶ notificação ◀──────┘
//! ```
//!
//! A inferência nunca roda na thread do GStreamer nem na do GTK: o probe só
//! entrega uma cópia do quadro à [`Engine`], que tem threads próprias.

pub mod classes;
mod engine;
mod yolo;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use engine::{Engine, Status};

/// Confiança mínima usada quando a câmera não define a dela.
pub const DEFAULT_CONFIDENCE: f32 = 0.45;

// ---------------------------------------------------------------------------
// Configuração por câmera
// ---------------------------------------------------------------------------

/// Ajustes de detecção de uma câmera. Fica no `devices.toml`, junto do canal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Classes a reconhecer, pelo nome COCO em inglês (`person`, `dog`, …).
    /// Nomes desconhecidos são ignorados, para um arquivo editado à mão ou
    /// vindo de outra versão não impedir o app de abrir.
    #[serde(default = "default_classes")]
    pub classes: Vec<String>,
    /// Confiança mínima em %, no lugar do padrão global.
    #[serde(default)]
    pub confidence: Option<u8>,
}

fn default_classes() -> Vec<String> {
    classes::DEFAULT.iter().map(|c| (*c).to_string()).collect()
}

impl Default for DetectionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            classes: default_classes(),
            confidence: None,
        }
    }
}

impl DetectionSettings {
    /// Tudo nos valores de fábrica? Usado para não poluir o `devices.toml`
    /// das câmeras que nunca mexeram nisso.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `true` nas classes escolhidas.
    pub fn class_mask(&self) -> [bool; classes::COUNT] {
        let mut mask = [false; classes::COUNT];
        for id in self.classes.iter().filter_map(|name| classes::id_of(name)) {
            mask[id] = true;
        }
        mask
    }

    /// Confiança mínima efetiva (0.0–1.0).
    pub fn min_confidence(&self, fallback: f32) -> f32 {
        self.confidence
            .map_or(fallback, |pct| f32::from(pct.clamp(1, 99)) / 100.0)
    }
}

// ---------------------------------------------------------------------------
// Resultados
// ---------------------------------------------------------------------------

/// Um objeto encontrado. As caixas são frações (0–1) do quadro original.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub class: usize,
    pub score: f32,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Detection {
    pub fn area(&self) -> f32 {
        (self.x2 - self.x1).max(0.0) * (self.y2 - self.y1).max(0.0)
    }
}

/// Um objeto que acabou de aparecer e merece aviso.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub class: usize,
    /// Quantos da classe estavam no quadro.
    pub count: usize,
}

impl Alert {
    /// `"pessoa"` ou `"pessoa ×2"`.
    pub fn label(&self) -> String {
        label_with_count(self.class, self.count)
    }
}

fn label_with_count(class: usize, count: usize) -> String {
    if count > 1 {
        format!("{} ×{count}", classes::pt(class))
    } else {
        classes::pt(class).to_string()
    }
}

/// O que a UI desenha: as detecções recentes e o tamanho do quadro em que
/// foram feitas (para respeitar a proporção do vídeo).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub detections: Vec<Detection>,
    pub width: usize,
    pub height: usize,
}

// ---------------------------------------------------------------------------
// Estado por câmera
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Active {
    classes: [bool; classes::COUNT],
    min_confidence: f32,
}

#[derive(Debug, Default)]
struct Latest {
    /// O que a UI desenha: o último quadro mais as caixas "seguradas".
    snapshot: Snapshot,
    /// Só o que o último quadro realmente detectou.
    fresh: Vec<Detection>,
    at: Option<Instant>,
}

/// Sobreposição (IoU) a partir da qual uma caixa nova e uma antiga da mesma
/// classe são o mesmo objeto.
const SAME_OBJECT_IOU: f32 = 0.3;

#[derive(Debug)]
struct Presence {
    last_seen: [Option<Instant>; classes::COUNT],
    last_alert: [Option<Instant>; classes::COUNT],
}

/// Estado de detecção de uma câmera, compartilhado entre o probe da pipeline,
/// as threads de inferência, o supervisor e a UI.
#[derive(Debug)]
pub struct DetectionState {
    engine: Arc<Engine>,
    active: Mutex<Active>,
    default_confidence: f32,
    /// Tempo mínimo entre dois avisos da mesma classe.
    cooldown: Duration,
    /// Por quanto tempo uma detecção continua valendo depois de feita.
    ttl: Duration,
    latest: Mutex<Latest>,
    /// Sobe a cada resultado publicado; a UI só redesenha quando muda.
    seq: AtomicU64,
    in_flight: AtomicBool,
    alerts: Mutex<Vec<Alert>>,
    presence: Mutex<Presence>,
}

impl DetectionState {
    pub fn new(
        engine: Arc<Engine>,
        settings: &DetectionSettings,
        default_confidence: f32,
        cooldown: Duration,
        ttl: Duration,
    ) -> Self {
        Self {
            engine,
            active: Mutex::new(Active {
                classes: settings.class_mask(),
                min_confidence: settings.min_confidence(default_confidence),
            }),
            default_confidence,
            cooldown,
            ttl,
            latest: Mutex::new(Latest::default()),
            seq: AtomicU64::new(0),
            in_flight: AtomicBool::new(false),
            alerts: Mutex::new(Vec::new()),
            presence: Mutex::new(Presence {
                last_seen: [None; classes::COUNT],
                last_alert: [None; classes::COUNT],
            }),
        }
    }

    /// Aplica novas classes/confiança sem recriar a pipeline.
    pub fn update(&self, settings: &DetectionSettings) {
        *self.active.lock().unwrap() = Active {
            classes: settings.class_mask(),
            min_confidence: settings.min_confidence(self.default_confidence),
        };
        // Some da tela o que deixou de ser pedido, sem esperar o próximo quadro.
        let mask = settings.class_mask();
        let mut latest = self.latest.lock().unwrap();
        latest.snapshot.detections.retain(|d| mask[d.class]);
        latest.fresh.retain(|d| mask[d.class]);
        drop(latest);
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    pub fn status(&self) -> Status {
        self.engine.status()
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Filtro atual, para a thread de inferência.
    fn active(&self) -> ([bool; classes::COUNT], f32) {
        let active = self.active.lock().unwrap();
        (active.classes, active.min_confidence)
    }

    /// Reserva a vaga de análise desta câmera: só um quadro por vez fica na
    /// fila, o resto é descartado (melhor pular um quadro que acumular atraso).
    pub fn try_begin(&self) -> bool {
        !self.in_flight.swap(true, Ordering::AcqRel)
    }

    /// Libera a vaga sem publicar resultado (erro ou modelo ainda carregando).
    pub fn finish(&self) {
        self.in_flight.store(false, Ordering::Release);
    }

    /// Guarda o resultado de um quadro e levanta avisos para os objetos novos.
    pub fn publish(&self, detections: Vec<Detection>, width: usize, height: usize) {
        self.publish_at(detections, width, height, Instant::now());
        self.finish();
    }

    fn publish_at(&self, detections: Vec<Detection>, width: usize, height: usize, now: Instant) {
        let mut counts = [0usize; classes::COUNT];
        for d in &detections {
            counts[d.class] += 1;
        }

        {
            // Um objeto "aparece" quando não era visto há mais que o `ttl`:
            // uma pessoa parada na frente da câmera avisa uma vez, não a
            // cada cooldown.
            let mut presence = self.presence.lock().unwrap();
            let mut alerts = self.alerts.lock().unwrap();
            for (class, &count) in counts.iter().enumerate().filter(|&(_, &n)| n > 0) {
                let appeared = presence.last_seen[class]
                    .is_none_or(|seen| now.saturating_duration_since(seen) > self.ttl);
                let cooled = presence.last_alert[class]
                    .is_none_or(|sent| now.saturating_duration_since(sent) >= self.cooldown);
                presence.last_seen[class] = Some(now);
                if appeared && cooled {
                    presence.last_alert[class] = Some(now);
                    alerts.push(Alert { class, count });
                }
            }
        }

        let mut latest = self.latest.lock().unwrap();
        // Um objeto limítrofe some e volta de um quadro para o outro; sem isto
        // a caixa pisca. Quem sumiu só agora fica mais um quadro na tela — e só
        // um: as seguradas não são seguradas de novo.
        let mut shown = detections.clone();
        if latest
            .at
            .is_some_and(|at| now.saturating_duration_since(at) <= self.ttl)
        {
            shown.extend(latest.fresh.iter().filter(|old| {
                !detections
                    .iter()
                    .any(|new| new.class == old.class && yolo::iou(new, old) > SAME_OBJECT_IOU)
            }));
        }
        *latest = Latest {
            snapshot: Snapshot {
                detections: shown,
                width,
                height,
            },
            fresh: detections,
            at: Some(now),
        };
        drop(latest);
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    /// Detecções ainda válidas (vazio se já passou do `ttl`).
    pub fn snapshot(&self) -> Snapshot {
        let latest = self.latest.lock().unwrap();
        match latest.at {
            Some(at) if at.elapsed() <= self.ttl => latest.snapshot.clone(),
            _ => Snapshot::default(),
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// Avisos pendentes; quem chama fica com eles.
    pub fn take_alerts(&self) -> Vec<Alert> {
        std::mem::take(&mut *self.alerts.lock().unwrap())
    }

    /// `"pessoa ×2 · cachorro"` para o selo do card, ou `None` sem detecções.
    pub fn summary(&self) -> Option<String> {
        let snapshot = self.snapshot();
        let mut counts = [0usize; classes::COUNT];
        for d in &snapshot.detections {
            counts[d.class] += 1;
        }
        let mut present: Vec<(usize, usize)> = counts
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, n)| n > 0)
            .collect();
        if present.is_empty() {
            return None;
        }
        present.sort_by_key(|&(class, n)| (std::cmp::Reverse(n), class));
        Some(
            present
                .into_iter()
                .map(|(class, n)| label_with_count(class, n))
                .collect::<Vec<_>>()
                .join(" · "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Detection as EngineConfig;

    fn state(cooldown_ms: u64, ttl_ms: u64) -> DetectionState {
        DetectionState::new(
            Arc::new(Engine::new(EngineConfig::default())),
            &DetectionSettings::default(),
            DEFAULT_CONFIDENCE,
            Duration::from_millis(cooldown_ms),
            Duration::from_millis(ttl_ms),
        )
    }

    fn person() -> Detection {
        Detection {
            class: 0,
            score: 0.9,
            x1: 0.1,
            y1: 0.1,
            x2: 0.4,
            y2: 0.9,
        }
    }

    #[test]
    fn padrao_marca_as_classes_comuns() {
        let settings = DetectionSettings::default();
        assert!(!settings.enabled);
        let mask = settings.class_mask();
        assert!(mask[classes::id_of("person").unwrap()]);
        assert!(mask[classes::id_of("dog").unwrap()]);
        assert!(!mask[classes::id_of("book").unwrap()]);
        assert!(settings.is_default());
    }

    #[test]
    fn classe_desconhecida_e_ignorada() {
        let settings = DetectionSettings {
            classes: vec!["cat".into(), "unicorn".into()],
            ..Default::default()
        };
        assert_eq!(settings.class_mask().iter().filter(|&&on| on).count(), 1);
    }

    #[test]
    fn confianca_propria_vale_sobre_a_global() {
        let mut settings = DetectionSettings::default();
        assert_eq!(settings.min_confidence(0.45), 0.45);
        settings.confidence = Some(70);
        assert_eq!(settings.min_confidence(0.45), 0.7);
        settings.confidence = Some(0);
        assert_eq!(settings.min_confidence(0.45), 0.01, "nunca zero");
    }

    #[test]
    fn toml_sem_campos_usa_os_padroes() {
        let settings: DetectionSettings = toml::from_str("enabled = true").unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.classes, default_classes());
        let custom: DetectionSettings =
            toml::from_str("enabled = true\nclasses = [\"bird\"]\nconfidence = 60").unwrap();
        assert_eq!(custom.classes, ["bird"]);
        assert_eq!(custom.confidence, Some(60));
    }

    #[test]
    fn aviso_so_quando_o_objeto_aparece() {
        let state = state(0, 1_000);
        let start = Instant::now();

        state.publish_at(vec![person()], 640, 360, start);
        assert_eq!(state.take_alerts(), [Alert { class: 0, count: 1 }]);

        // Continua na cena: sem novo aviso.
        state.publish_at(
            vec![person(), person()],
            640,
            360,
            start + Duration::from_millis(500),
        );
        assert!(state.take_alerts().is_empty());

        // Some por mais que o ttl e volta: aviso de novo.
        state.publish_at(vec![], 640, 360, start + Duration::from_secs(3));
        state.publish_at(vec![person()], 640, 360, start + Duration::from_secs(4));
        assert_eq!(state.take_alerts().len(), 1);
    }

    #[test]
    fn cooldown_segura_aviso_repetido() {
        let state = state(10_000, 100);
        let start = Instant::now();
        state.publish_at(vec![person()], 640, 360, start);
        assert_eq!(state.take_alerts().len(), 1);

        // Saiu e voltou, mas dentro do cooldown.
        state.publish_at(vec![person()], 640, 360, start + Duration::from_secs(2));
        assert!(state.take_alerts().is_empty());

        state.publish_at(vec![person()], 640, 360, start + Duration::from_secs(12));
        assert_eq!(state.take_alerts().len(), 1);
    }

    #[test]
    fn resumo_ordena_por_quantidade() {
        let state = state(0, 60_000);
        let dog = Detection {
            class: 16,
            ..person()
        };
        state.publish(vec![dog, person(), person()], 640, 360);
        assert_eq!(state.summary().as_deref(), Some("pessoa ×2 · cachorro"));
    }

    #[test]
    fn caixa_que_some_por_um_quadro_nao_pisca() {
        let state = state(0, 10_000);
        let dog = Detection {
            class: 16,
            ..person()
        };
        let start = Instant::now();
        let ms = Duration::from_millis;

        state.publish_at(vec![person(), dog], 640, 360, start);
        // O cachorro some por um quadro: a caixa dele ainda fica…
        state.publish_at(vec![person()], 640, 360, start + ms(500));
        assert_eq!(state.snapshot().detections.len(), 2);
        // …mas só por um: no seguinte já sumiu de vez.
        state.publish_at(vec![person()], 640, 360, start + ms(1_000));
        assert_eq!(state.snapshot().detections.len(), 1);
    }

    #[test]
    fn caixa_redetectada_nao_e_duplicada_pela_seguranca() {
        let state = state(0, 10_000);
        let start = Instant::now();
        state.publish_at(vec![person()], 640, 360, start);
        // Mesmo objeto, caixa um pouco deslocada.
        let moved = Detection {
            x1: 0.12,
            x2: 0.42,
            ..person()
        };
        state.publish_at(vec![moved], 640, 360, start + Duration::from_millis(500));
        assert_eq!(state.snapshot().detections, [moved]);
    }

    #[test]
    fn detecao_expira_depois_do_ttl() {
        let state = state(0, 20);
        state.publish(vec![person()], 640, 360);
        assert_eq!(state.snapshot().detections.len(), 1);
        std::thread::sleep(Duration::from_millis(40));
        assert!(state.snapshot().detections.is_empty());
        assert_eq!(state.summary(), None);
    }

    #[test]
    fn so_uma_vaga_por_vez() {
        let state = state(0, 1_000);
        assert!(state.try_begin());
        assert!(!state.try_begin());
        state.finish();
        assert!(state.try_begin());
    }

    #[test]
    fn update_tira_da_tela_a_classe_desmarcada() {
        let state = state(0, 60_000);
        state.publish(
            vec![
                person(),
                Detection {
                    class: 16,
                    ..person()
                },
            ],
            640,
            360,
        );
        let seq = state.seq();
        state.update(&DetectionSettings {
            classes: vec!["dog".into()],
            ..Default::default()
        });
        assert_eq!(state.snapshot().detections.len(), 1);
        assert_eq!(state.snapshot().detections[0].class, 16);
        assert!(state.seq() > seq, "a UI precisa redesenhar");
    }
}
