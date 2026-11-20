//! Build script: on Windows, embed the application icon and version information in the
//! executables (shown by Explorer, the taskbar and the installer's shortcuts). Nothing to
//! do elsewhere: Linux takes the icon from the desktop entry, macOS from the app bundle.

fn main() {
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
