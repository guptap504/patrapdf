//! Windows: embed the app icon and version information into the .exe.
fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/app.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/windows/app.ico")
            .set("ProductName", "PDF Signer")
            .set("FileDescription", "PDF Signer")
            .set("CompanyName", "Prashant Gupta")
            .set("LegalCopyright", "Copyright © 2026 Prashant Gupta")
            .set("OriginalFilename", "pdf-signer.exe");
        res.compile().expect("failed to compile Windows resources");
    }
}
