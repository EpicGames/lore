#[allow(dead_code)]
fn profile_dir() -> String {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let path = std::path::PathBuf::from(out_dir);
    let mut path = path.as_path();
    while let Some(name) = path.file_name() {
        if name == "build" {
            return path
                .parent()
                .expect("No parent of build")
                .display()
                .to_string();
        }
        path = path.parent().expect("Reached root of filesystem");
    }
    panic!("OUT_DIR did not contain a build directory");
}

#[allow(dead_code)]
fn profile_name() -> String {
    let profile_dir = profile_dir();
    std::path::PathBuf::from(profile_dir)
        .file_name()
        .expect("Failed to get profile name")
        .display()
        .to_string()
}
