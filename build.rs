//! Build script: the versions of the main components, for Help → About; and on Windows,
//! the application icon and version information embedded in the executables (shown by
//! Explorer, the taskbar and the installer's shortcuts). Linux takes the icon from the
//! desktop entry, macOS from the app bundle.

/// The crates Help → About lists with their versions (see `ui::dialogs`).
const COMPONENTS: &[&str] = &[
    "egui",
    "vello",
    "wgpu",
    "parley",
    "winit",
    "tree-sitter",
    "ropey",
    "pulldown-cmark",
    "merman",
    "ratex-render",
    "spellbook",
    "resvg",
    "gix",
];

/// The locked version of each of [`COMPONENTS`], as `MARKDONE_COMPONENT_VERSIONS`
/// (`name version` pairs, comma separated). A crate locked at several versions shows the
/// highest.
fn component_versions() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let numbers = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let mut found: Vec<(&str, &str)> = Vec::new();
    let mut name = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = \"") {
            name = n.strip_suffix('"');
        } else if let Some(v) = line.strip_prefix("version = \"")
            && let Some(n) = name.take()
            && let Some(v) = v.strip_suffix('"')
            && COMPONENTS.contains(&n)
        {
            match found.iter_mut().find(|(f, _)| *f == n) {
                Some(entry) if numbers(v) > numbers(entry.1) => entry.1 = v,
                Some(_) => {}
                None => found.push((n, v)),
            }
        }
    }
    let list: Vec<String> = found.iter().map(|(n, v)| format!("{n} {v}")).collect();
    println!(
        "cargo:rustc-env=MARKDONE_COMPONENT_VERSIONS={}",
        list.join(",")
    );
}

fn main() {
    component_versions();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=packaging/windows/markdone.ico");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("packaging/windows/markdone.ico");
        resource.set("ProductName", "Markdone");
        resource.set("FileDescription", "Markdone, a Markdown editor");
        if let Err(e) = resource.compile() {
            panic!("embedding the Windows icon failed: {e}");
        }
    }
}
