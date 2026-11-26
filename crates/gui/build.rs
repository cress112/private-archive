fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_edition((1, 19))
        .with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("compile the GUI");
}
