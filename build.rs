fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");

    // Give the Windows executable its icon and version details.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app-icon.ico");
        resource.set("ProductName", "Nexus Music");
        resource.set("FileDescription", "Nexus Music");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=couldn't embed the Windows icon: {error}");
        }
    }
}
