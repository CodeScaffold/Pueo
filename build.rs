fn main() {
    // Bundle the app icon and extra symbolic icons, so they show even when running from target/.
    glib_build_tools::compile_resources(
        &["data", "data/icons/symbolic"],
        "data/resources.gresource.xml",
        "pueo.gresource",
    );
}
