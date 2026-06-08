use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    // Blueprint sources -> .ui (placed in OUT_DIR, mirroring the resources/ tree).
    let blueprints = ["resources/window.blp"];
    let status = Command::new("blueprint-compiler")
        .arg("batch-compile")
        .arg(&out_dir) // output dir
        .arg("resources") // input root (controls relative paths)
        .args(blueprints)
        .status()
        .expect("failed to run blueprint-compiler (is it installed?)");
    assert!(status.success(), "blueprint-compiler failed");

    for bp in blueprints {
        println!("cargo:rerun-if-changed={bp}");
    }
    println!("cargo:rerun-if-changed=resources/resources.gresource.xml");

    // Compile the gresource bundle. The .ui files live in OUT_DIR (from blueprint),
    // so OUT_DIR is the lookup root for the files referenced by the XML.
    // Two lookup roots: generated .ui files live in OUT_DIR, static assets
    // (CSS, etc.) live in resources/.
    println!("cargo:rerun-if-changed=resources/style.css");
    glib_build_tools::compile_resources(
        &[Path::new(&out_dir), Path::new("resources")],
        "resources/resources.gresource.xml",
        "sqweel.gresource",
    );
}
