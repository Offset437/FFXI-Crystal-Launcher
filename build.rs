/// Embeds the 32-bit loader (`pol.exe`) so the launcher ships as one exe. The loader is built
/// separately (`tools/build-release.ps1`): a nested cargo from here would contend for locks.
fn embed_loader() {
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    println!("cargo:rerun-if-env-changed=CRYSTAL_LOADER_EXE");
    let explicit = std::env::var_os("CRYSTAL_LOADER_EXE").map(PathBuf::from);
    let source = explicit.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("crystal-loader/target/i686-pc-windows-msvc/release/pol.exe")
    });
    println!("cargo:rerun-if-changed={}", source.display());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(e) if explicit.is_some() || release => panic!(
            "\n\nThe 32-bit loader to embed is missing: {} ({e}).\n\
             Build it first (cd crystal-loader; cargo build --release --target i686-pc-windows-msvc),\n\
             or run tools\\build-release.ps1, or point CRYSTAL_LOADER_EXE at a pol.exe.\n",
            source.display()
        ),
        Err(_) => {
            // Debug builds and unit tests work without it; the launcher then embeds nothing.
            println!(
                "cargo:warning=no loader at {}; this build embeds none (release builds require it)",
                source.display()
            );
            Vec::new()
        }
    };
    let hash = if bytes.is_empty() {
        String::new()
    } else {
        Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
    };
    // Installer builds ship the host as crystal\pol.exe beside the launcher: they keep only its
    // hash (to check that installed file) and carry no second executable inside this one.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_INSTALLED_HOST");
    let carried: &[u8] = if std::env::var_os("CARGO_FEATURE_INSTALLED_HOST").is_some() { &[] } else { &bytes };
    std::fs::write(out.join("embedded-pol.exe"), carried).unwrap();
    std::fs::write(out.join("embedded-pol.sha256"), hash).unwrap();
}

/// crystal.rc (the icon) plus version information naming the product and publisher, written
/// to OUT_DIR with the icon's path made absolute.
fn resource_script() -> std::path::PathBuf {
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = root.join("assets").join("crystal.ico").display().to_string().replace('\\', "\\\\");
    let base = std::fs::read_to_string(root.join("crystal.rc")).unwrap().replace("\"assets/crystal.ico\"", &format!("\"{icon}\""));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).filter_map(|p| p.parse().ok()).take(3).collect();
    parts.resize(4, 0);
    let numbers = format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3]);
    let rc = format!(
        r#"#pragma code_page(65001)
{base}
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Crystal Launcher project"
            VALUE "FileDescription", "Crystal Launcher"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "Crystal-Launcher"
            VALUE "LegalCopyright", "GPL-3.0-only. FINAL FANTASY XI and PlayOnline are trademarks of Square Enix."
            VALUE "OriginalFilename", "Crystal-Launcher.exe"
            VALUE "ProductName", "Crystal Launcher"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("crystal.rc");
    std::fs::write(&out, rc).unwrap();
    out
}

/// The Slint front end's markup (`ui/`), compiled into the exe with its fonts and pictures.
fn compile_slint() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/crystal.slint", config).unwrap();
}

fn main() {
    embed_loader();
    compile_slint();
    println!("cargo:rerun-if-changed=launcher.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rerun-if-changed=assets/crystal.ico");
        println!("cargo:rerun-if-changed=crystal.rc");
        embed_resource::compile(resource_script(), embed_resource::NONE).manifest_required().unwrap();
        let template = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("launcher.manifest");
        // Unit tests and hidden smoke checks must run without elevation. The
        // delivered UI build explicitly enables the administrator manifest.
        let source = std::fs::read_to_string(template).unwrap();
        let source = if std::env::var_os("CARGO_FEATURE_ELEVATED_UI").is_some() {
            source
        } else {
            source.replace("requireAdministrator", "asInvoker")
        };
        let manifest = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("launcher.manifest");
        std::fs::write(&manifest, source).unwrap();
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        if std::env::var_os("CARGO_FEATURE_ELEVATED_UI").is_some() {
            println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
        }
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
