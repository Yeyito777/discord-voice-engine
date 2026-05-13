fn main() {
    let mut build = cc::Build::new();
    build.file("src/playback_c.c");
    build.flag_if_supported("-std=c11");
    build.warnings(true);

    for package in ["opus", "libpulse-simple", "libpulse"] {
        let library = pkg_config::Config::new()
            .probe(package)
            .unwrap_or_else(|error| panic!("failed to find {package} with pkg-config: {error}"));
        for include_path in library.include_paths {
            build.include(include_path);
        }
    }

    build.compile("discord_voice_engine_playback_c");
    println!("cargo:rerun-if-changed=src/playback_c.c");
}
