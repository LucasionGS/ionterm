//! Embeds the application icon in `ionterm.exe`.
//!
//! Windows resource compilers only exist on Windows hosts, so a Windows
//! build made elsewhere (like `cargo check --target x86_64-pc-windows-msvc`
//! on Linux) skips it; release builds happen on Windows CI.

fn main() {
    println!("cargo:rerun-if-changed=../../data/icons/ionterm.rc");
    println!("cargo:rerun-if-changed=../../data/icons/ionterm.ico");
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if target_windows && cfg!(windows) {
        embed_resource::compile("../../data/icons/ionterm.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling the Windows icon resource");
    }
}
