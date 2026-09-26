// Embute o ícone (icone.ico, na raiz do repo) no .exe do Windows. Nos outros
// sistemas o ícone da janela/app vem do hicolor (veja src/ui/mod.rs, APP_ID), então
// este script não faz nada fora do Windows.
fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=icone.ico");
        winresource::WindowsResource::new()
            .set_icon("icone.ico")
            .compile()
            .expect("falha ao embutir icone.ico no .exe (falta windres do mingw-w64-toolchain?)");
    }
}
