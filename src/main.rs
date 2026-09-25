//! Dashboard de visualização das câmeras de um NVR iCSee/XMEye via RTSP.

mod audio;
mod camera;
mod config;
mod discovery;
mod motion;
mod notify;
mod pipeline;
mod reconnect;
mod recording;
mod store;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::store::Store;

const HELP: &str = concat!(
    "nvr-dashboard ",
    env!("CARGO_PKG_VERSION"),
    r#" — grid de câmeras RTSP (NVR iCSee/XMEye e câmeras IP)

USO:
    nvr-dashboard [OPÇÕES]

OPÇÕES:
    -c, --config <ARQUIVO>   Caminho do TOML de configuração
        --check              Valida a configuração e testa o alcance dos dispositivos, sem abrir a GUI
    -h, --help               Mostra esta ajuda
    -V, --version            Mostra a versão

AJUSTES (cameras.toml, opcional) — procurado nesta ordem:
    --config <ARQUIVO>
    $NVR_DASHBOARD_CONFIG
    ./config/cameras.toml
    $XDG_CONFIG_HOME/nvr-dashboard/cameras.toml

CÂMERAS: cadastradas pela própria janela (manualmente ou escaneando a rede) e
guardadas em $XDG_CONFIG_HOME/nvr-dashboard/devices.toml (ou em
$NVR_DASHBOARD_DEVICES, se definido).

ATALHOS NA JANELA:
    Clique / Enter   Abre a câmera em foco em tela cheia
    1 … 9            Abre a câmera daquela posição em tela cheia
    Esc              Volta ao grid
    Ctrl+S           Captura um PNG da câmera em foco
    Ctrl+R           Inicia/para a gravação da câmera em foco
    Ctrl+M           Liga/desliga o áudio da câmera em foco (uma por vez)
    F11              Alterna tela cheia da janela
    Ctrl+Q           Sai

VARIÁVEIS DE AMBIENTE:
    RUST_LOG    Filtro de log (padrão: nvr_dashboard=info,warn)
    GST_DEBUG   Verbosidade do GStreamer (ex.: rtspsrc:5)
"#
);

/// Sobrescreve o caminho do cadastro de câmeras (útil em testes).
const STORE_ENV_VAR: &str = "NVR_DASHBOARD_DEVICES";

/// Quanto esperar pelos supervisores no encerramento. Precisa ser maior que o
/// `recording::STOP_TIMEOUT`, que é o pior caso de fechar um arquivo.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    check: bool,
}

/// Parsing manual: são quatro flags, não vale puxar um parser de CLI inteiro.
fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    let mut raw = std::env::args().skip(1);

    while let Some(flag) = raw.next() {
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("nvr-dashboard {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--check" => args.check = true,
            "-c" | "--config" => {
                let value = raw
                    .next()
                    .with_context(|| format!("`{flag}` exige um caminho de arquivo"))?;
                args.config = Some(PathBuf::from(value));
            }
            other => bail!("argumento desconhecido: {other}\n\nUse --help."),
        }
    }
    Ok(Some(args))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("nvr_dashboard=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn main() -> Result<()> {
    init_tracing();

    let Some(args) = parse_args()? else {
        return Ok(());
    };

    let (config, path) = Config::discover(args.config)?;
    match &path {
        Some(path) => tracing::info!(arquivo = %path.display(), "configuração carregada"),
        None => tracing::info!("sem cameras.toml; usando ajustes padrão"),
    }
    if config.ignored_legacy_blocks() > 0 {
        tracing::warn!(
            blocos = config.ignored_legacy_blocks(),
            "os blocos [nvr]/[[cameras]] do cameras.toml são ignorados: cadastre as câmeras \
             pela janela do app"
        );
    }

    let store_path = std::env::var_os(STORE_ENV_VAR)
        .map(PathBuf::from)
        .or_else(Store::default_path)
        .context("não consegui descobrir onde guardar o cadastro de câmeras")?;
    let store = Store::load(store_path);
    tracing::info!(
        dispositivos = store.devices.len(),
        "cadastro de câmeras carregado"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        // Os supervisores passam quase todo o tempo bloqueados em I/O; duas
        // threads dão folga de sobra e mantêm o processo leve.
        .worker_threads(2)
        .thread_name("nvr-supervisor")
        .enable_all()
        .build()
        .context("falha ao criar o runtime do tokio")?;

    if args.check {
        return runtime.block_on(check(&config, &store));
    }

    gst::init().context("falha ao inicializar o GStreamer")?;
    pipeline::configure_decoders(config.app.hardware_decoding);

    let session = ui::run(config, store, runtime.handle().clone())?;

    // A janela já fechou, mas um supervisor pode estar fechando um arquivo de
    // gravação. Encerrar o runtime agora cancelaria essa task no meio e
    // deixaria o vídeo truncado, então esperamos o canal fechar.
    runtime.block_on(async {
        if tokio::time::timeout(SHUTDOWN_GRACE, session.supervisors_done.recv())
            .await
            .is_err()
        {
            tracing::warn!(
                segundos = SHUTDOWN_GRACE.as_secs(),
                "supervisores não encerraram a tempo; saindo assim mesmo"
            );
        }
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    tracing::info!("finalizado");

    // `process::exit` não roda destrutores — é exatamente o que queremos:
    // `session._pipelines` segura os `gtk4paintablesink` até aqui, e eles nunca
    // são liberados fora da thread do GTK.
    std::process::exit(i32::from(session.exit_code.get()));
}

/// Modo `--check`: valida a configuração e o alcance dos dispositivos sem abrir a GUI.
async fn check(config: &Config, store: &Store) -> Result<()> {
    println!("Configuração válida.\n");

    println!("Dispositivos ({}):", store.devices.len());
    let mut unreachable = Vec::new();
    for device in &store.devices {
        let reachable =
            reconnect::probe_tcp(&device.host, device.port, Duration::from_secs(3)).await;
        println!(
            "  [{}] {}:{} — usuário {} (senha: {}) — {}",
            device.id,
            device.host,
            device.port,
            device.username,
            config::MASK,
            if reachable {
                "acessível"
            } else {
                "SEM RESPOSTA"
            }
        );
        for camera in camera::build_device(0, device, &config.app) {
            println!("      canal {} — {}", camera.channel, camera.name);
            println!("        {}", camera.masked_url_for(camera.grid_stream));
        }
        if !reachable {
            unreachable.push(device.id.clone());
        }
    }
    if store.devices.is_empty() {
        println!("  (nenhum — abra o app e adicione câmeras pela janela)");
    }

    println!("\nSaídas:");
    println!("  Capturas:  {}", config.snapshot_dir().display());
    println!("  Gravações: {}", config.recording_dir().display());
    println!(
        "  Movimento: {}",
        if config.motion.enabled {
            "ligado"
        } else {
            "desligado"
        }
    );

    if !unreachable.is_empty() {
        bail!(
            "sem resposta na porta RTSP de: {}. Verifique rede/IP/porta",
            unreachable.join(", ")
        );
    }
    Ok(())
}
