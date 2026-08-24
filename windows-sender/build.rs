//! Embeds the Windows resources: application icon, version information and the manifest.
//!
//! The icon has to live in the binary rather than beside it - the shell, the task bar and the tray
//! all read it out of the executable - and the version block is what makes the entry in "Apps &
//! features" and the properties dialog look like a real application rather than a loose binary.
//!
//! `winresource` shells out to `rc.exe` from the MSVC toolchain, so it is a Windows-host-only
//! build dependency (see Cargo.toml). Type-checking the Win32 paths from a Mac with
//! `cargo check --target x86_64-pc-windows-msvc` therefore lands in the fallback branch and simply
//! skips the resources.

fn main() {
    println!("cargo:rerun-if-changed=resources/app.ico");
    println!("cargo:rerun-if-changed=resources/app.manifest");
    embed();
}

#[cfg(windows)]
fn embed() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("resources/app.ico");
    resource.set_manifest_file("resources/app.manifest");
    resource.set("ProductName", "Remote Input Bridge");
    resource.set("FileDescription", "Remote Input Bridge - Windows sender");
    resource.set("CompanyName", "Lince Studio");
    resource.set("LegalCopyright", "MIT licensed. Copyright (c) 2026 Nikita Maketryuk");
    resource.set("OriginalFilename", "rib-sender.exe");
    resource.set("InternalName", "rib-sender");
    if let Err(e) = resource.compile() {
        // A missing rc.exe must not be fatal: the binary is perfectly usable without an icon, and
        // failing the build here would turn a cosmetic problem into "cannot build at all".
        println!("cargo:warning=could not embed the Windows resources: {e}");
    }
}

#[cfg(not(windows))]
fn embed() {}
