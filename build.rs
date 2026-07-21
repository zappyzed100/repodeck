use std::path::{Path, PathBuf};

fn main() {
    slint_build::compile("ui/app-window.slint").expect("failed to compile Slint UI");

    // This crate produces two binaries (`repodeck`, `repodeck-hook`), but
    // Cargo only runs one build script per *package*, with no reliable way
    // to tell which binary is currently being linked (`CARGO_BIN_NAME` isn't
    // set for build scripts in this single-build-script setup). So the
    // manifest + icon embedding below unconditionally applies to whichever
    // binary Cargo happens to be linking, including `repodeck-hook.exe` — a
    // harmless side effect for a console tool with no window (a few extra
    // KB, no functional impact). Do not "fix" this with a `CARGO_BIN_NAME`
    // gate; splitting the hook into its own workspace member just to avoid
    // it would be overkill for what it actually costs.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(new_manifest("RepoDeck.App")).expect("failed to embed Windows manifest");

        let icon_path = generate_ico("assets/repodeck-icon.svg");
        winresource::WindowsResource::new()
            .set_icon(
                icon_path
                    .to_str()
                    .expect("OUT_DIR path must be valid UTF-8"),
            )
            .compile()
            .expect("failed to embed the .exe icon resource");
    }

    println!("cargo:rerun-if-changed=ui/app-window.slint");
    println!("cargo:rerun-if-changed=assets/repodeck-icon.svg");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Rasterizes `svg_path` at the standard Windows icon sizes and writes a
/// multi-resolution `.ico` to `OUT_DIR`. Without an embedded `RT_GROUP_ICON`
/// resource, Explorer shows a blank/generic icon for a taskbar-pinned shortcut
/// even though the running window's own (Slint-set) icon looks correct.
fn generate_ico(svg_path: &str) -> PathBuf {
    let svg_data =
        std::fs::read(svg_path).unwrap_or_else(|e| panic!("failed to read {svg_path}: {e}"));
    let tree = resvg::usvg::Tree::from_data(&svg_data, &resvg::usvg::Options::default())
        .unwrap_or_else(|e| panic!("failed to parse {svg_path}: {e}"));
    let source_size = tree.size();

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(size, size).expect("icon size must be nonzero");

        let scale_x = size as f32 / source_size.width();
        let scale_y = size as f32 / source_size.height();
        let transform = resvg::tiny_skia::Transform::from_scale(scale_x, scale_y);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        let rgba: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();

        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        let entry = ico::IconDirEntry::encode(&image).expect("failed to encode icon frame");
        icon_dir.add_entry(entry);
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo during build scripts");
    let ico_path = Path::new(&out_dir).join("repodeck.ico");
    let file = std::fs::File::create(&ico_path).expect("failed to create repodeck.ico in OUT_DIR");
    icon_dir.write(file).expect("failed to write repodeck.ico");

    ico_path
}
