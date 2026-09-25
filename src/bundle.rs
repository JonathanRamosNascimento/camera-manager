//! Autoconfiguração de pacotes "com tudo dentro" (Windows e macOS).
//!
//! Nos instaladores do Windows e no `.app` do macOS, o GStreamer, o GTK e seus
//! dados viajam junto com o executável. Para o app achá-los sem script de
//! lançamento, ele procura a pasta empacotada ao lado do binário e ajusta as
//! variáveis de ambiente **antes** de inicializar o GStreamer/GTK.
//!
//! Layouts reconhecidos (a raiz é onde existe `lib/gstreamer-1.0`):
//!
//! ```text
//! Windows / portátil      macOS (.app)
//! nvr-dashboard.exe       Contents/MacOS/nvr-dashboard
//! lib/gstreamer-1.0/      Contents/Resources/lib/gstreamer-1.0/
//! libexec/gstreamer-1.0/  Contents/Resources/libexec/gstreamer-1.0/
//! share/glib-2.0/schemas  Contents/Resources/share/glib-2.0/schemas
//! ```
//!
//! Instalação normal (Linux, ou o Homebrew/MSYS2 do desenvolvedor) não tem essa
//! pasta, então nada muda: valem os caminhos do sistema.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gtk::glib;

/// Onde procurar a raiz do pacote, a partir da pasta do executável.
fn candidate_roots(exe_dir: &Path) -> Vec<PathBuf> {
    vec![exe_dir.to_path_buf(), exe_dir.join("..").join("Resources")]
}

/// A primeira raiz que tem os plugins do GStreamer empacotados.
pub fn find_root(exe_dir: &Path) -> Option<PathBuf> {
    candidate_roots(exe_dir)
        .into_iter()
        .find(|root| root.join("lib").join("gstreamer-1.0").is_dir())
}

/// Variáveis de ambiente para usar o pacote em `root`. Função pura (só olha o
/// disco), para poder testar sem mexer no ambiente do processo.
pub fn env_for(
    root: &Path,
    registry: &Path,
    existing_data_dirs: Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    let plugins = root.join("lib").join("gstreamer-1.0");
    let mut vars: Vec<(&'static str, OsString)> = vec![
        // Só os plugins do pacote: misturar com os do sistema causa conflito de versões.
        ("GST_PLUGIN_SYSTEM_PATH_1_0", plugins.clone().into()),
        ("GST_PLUGIN_PATH_1_0", plugins.into()),
        ("GST_REGISTRY_1_0", registry.into()),
    ];

    let scanner_name = if cfg!(windows) {
        "gst-plugin-scanner.exe"
    } else {
        "gst-plugin-scanner"
    };
    let scanner = root
        .join("libexec")
        .join("gstreamer-1.0")
        .join(scanner_name);
    if scanner.is_file() {
        vars.push(("GST_PLUGIN_SCANNER_1_0", scanner.into()));
    }

    let schemas = root.join("share").join("glib-2.0").join("schemas");
    if schemas.is_dir() {
        vars.push(("GSETTINGS_SCHEMA_DIR", schemas.into()));
    }

    let pixbuf = root
        .join("lib")
        .join("gdk-pixbuf-2.0")
        .join("2.10.0")
        .join("loaders.cache");
    if pixbuf.is_file() {
        vars.push(("GDK_PIXBUF_MODULE_FILE", pixbuf.into()));
    }

    let share = root.join("share");
    if share.is_dir() {
        let mut dirs = vec![share];
        if let Some(existing) = existing_data_dirs {
            dirs.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(dirs) {
            vars.push(("XDG_DATA_DIRS", joined));
        }
    }
    vars
}

/// Se o app está empacotado, aponta o ambiente para o pacote. Deve ser chamada
/// no início do `main`, antes de criar threads e de inicializar GStreamer/GTK.
pub fn configure() {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let Some(root) = find_root(&exe_dir) else {
        return;
    };

    // O registro de plugins vai para a pasta de cache do usuário, com a versão
    // no nome: um cache velho de outra versão faria o GStreamer não achar nada.
    let registry = glib::user_cache_dir()
        .join("nvr-dashboard")
        .join(format!("gst-registry-{}.bin", env!("CARGO_PKG_VERSION")));
    if let Some(dir) = registry.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    for (key, value) in env_for(&root, &registry, std::env::var_os("XDG_DATA_DIRS")) {
        // SAFETY: chamado no início do `main`, antes de qualquer thread (tokio,
        // GTK ou GStreamer) existir; ninguém lê o ambiente concorrentemente.
        unsafe { std::env::set_var(key, value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nvr-bundle-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sem_pasta_empacotada_nada_muda() {
        let dir = tempdir("vazio");
        assert!(find_root(&dir).is_none());
    }

    #[test]
    fn acha_a_raiz_ao_lado_do_executavel() {
        let dir = tempdir("lado");
        fs::create_dir_all(dir.join("lib/gstreamer-1.0")).unwrap();
        assert_eq!(find_root(&dir), Some(dir.clone()));
    }

    #[test]
    fn acha_a_raiz_no_resources_do_app_do_mac() {
        let dir = tempdir("mac");
        let macos = dir.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::create_dir_all(dir.join("Contents/Resources/lib/gstreamer-1.0")).unwrap();
        let root = find_root(&macos).expect("raiz em Resources");
        assert!(root.ends_with("Resources"), "{root:?}");
    }

    #[test]
    fn variaveis_apontam_so_para_o_que_existe() {
        let dir = tempdir("env");
        fs::create_dir_all(dir.join("lib/gstreamer-1.0")).unwrap();
        fs::create_dir_all(dir.join("share/glib-2.0/schemas")).unwrap();
        let registry = dir.join("reg.bin");

        let vars = env_for(&dir, &registry, Some("/usr/share".into()));
        let get = |key: &str| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());

        assert_eq!(
            get("GST_PLUGIN_PATH_1_0"),
            Some(dir.join("lib/gstreamer-1.0").into())
        );
        assert_eq!(
            get("GST_PLUGIN_SYSTEM_PATH_1_0"),
            get("GST_PLUGIN_PATH_1_0")
        );
        assert_eq!(get("GST_REGISTRY_1_0"), Some(registry.into()));
        assert_eq!(
            get("GSETTINGS_SCHEMA_DIR"),
            Some(dir.join("share/glib-2.0/schemas").into())
        );
        assert!(
            get("GST_PLUGIN_SCANNER_1_0").is_none(),
            "scanner ausente não entra"
        );
        assert!(get("GDK_PIXBUF_MODULE_FILE").is_none());

        // O share do pacote vem primeiro; os dirs que já existiam continuam.
        let data_dirs = get("XDG_DATA_DIRS").unwrap();
        let parts: Vec<PathBuf> = std::env::split_paths(&data_dirs).collect();
        assert_eq!(parts[0], dir.join("share"));
        assert!(parts.contains(&PathBuf::from("/usr/share")));
    }

    #[test]
    fn scanner_e_pixbuf_entram_quando_existem() {
        let dir = tempdir("scanner");
        fs::create_dir_all(dir.join("lib/gstreamer-1.0")).unwrap();
        fs::create_dir_all(dir.join("libexec/gstreamer-1.0")).unwrap();
        let scanner = if cfg!(windows) {
            "gst-plugin-scanner.exe"
        } else {
            "gst-plugin-scanner"
        };
        fs::write(dir.join("libexec/gstreamer-1.0").join(scanner), b"").unwrap();
        fs::create_dir_all(dir.join("lib/gdk-pixbuf-2.0/2.10.0")).unwrap();
        fs::write(dir.join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"), b"").unwrap();

        let vars = env_for(&dir, &dir.join("r.bin"), None);
        assert!(vars.iter().any(|(k, _)| *k == "GST_PLUGIN_SCANNER_1_0"));
        assert!(vars.iter().any(|(k, _)| *k == "GDK_PIXBUF_MODULE_FILE"));
    }
}
