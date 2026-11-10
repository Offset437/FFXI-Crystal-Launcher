fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "i686-pc-windows-msvc" {
        // xiloader and Ashita's bootloader\pol.exe are both large-address-aware; FFXI's
        // memory use benefits from the 4 GB user space a 32-bit process gets on x64 Windows.
        println!("cargo:rustc-link-arg-bins=/LARGEADDRESSAWARE");
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        version_resource();
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// Version information and the Crystal icon for every binary, so the host is not an
/// anonymous 32-bit executable. Resources only: no manifest, so the process (its UAC
/// virtualization, OS version answers and DPI handling) runs exactly as before.
fn version_resource() {
    use std::path::PathBuf;
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let launcher_toml = dir.join("..").join("Cargo.toml");
    let icon = dir.join("..").join("assets").join("crystal.ico");
    println!("cargo:rerun-if-changed={}", launcher_toml.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    let product = std::fs::read_to_string(&launcher_toml)
        .ok()
        .and_then(|toml| {
            toml.lines()
                .find_map(|line| line.trim().strip_prefix("version = \"").and_then(|v| v.strip_suffix('"')).map(str::to_owned))
        })
        .unwrap_or_else(|| "0.0.0".into());
    let file = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numbers = |v: &str| {
        let mut parts: Vec<u16> = v.split(['.', '-', '+']).filter_map(|p| p.parse().ok()).take(3).collect();
        parts.resize(4, 0);
        format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3])
    };
    let icon_line = if icon.is_file() {
        format!("1 ICON \"{}\"\n", icon.display().to_string().replace('\\', "\\\\"))
    } else {
        String::new()
    };
    let rc = format!(
        r#"#pragma code_page(65001)
{icon_line}1 VERSIONINFO
FILEVERSION {file_n}
PRODUCTVERSION {product_n}
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
            VALUE "FileDescription", "Crystal Launcher game host"
            VALUE "FileVersion", "{file}"
            VALUE "InternalName", "pol"
            VALUE "LegalCopyright", "GPL-3.0-only. FINAL FANTASY XI and PlayOnline are trademarks of Square Enix."
            VALUE "OriginalFilename", "pol.exe"
            VALUE "ProductName", "Crystal Launcher"
            VALUE "ProductVersion", "{product}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        file_n = numbers(&file),
        product_n = numbers(&product),
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("crystal-loader.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
