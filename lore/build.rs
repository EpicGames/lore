// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::error::Error;
use std::fs;
use std::path::MAIN_SEPARATOR;

use glob::glob;
use regex::Regex;

include!("../build-helper.rs");

fn main() -> Result<(), Box<dyn Error>> {
    // Populate environment with build details
    let version = LoreVergen::default();
    vergen::Emitter::default()
        .add_custom_instructions(&version)?
        .emit()?;

    let path_sep = MAIN_SEPARATOR;

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // list all .rs files in `lore` so that we run this script to update the c header
    for entry in glob("src/**/*.rs").expect("glob syntax error") {
        match entry {
            Ok(path) => println!("cargo:rerun-if-changed={}", path.display()),
            Err(err) => println!("Glob error: {err}"),
        }
    }

    // `LoreEvent` is a cbindgen-exported C-API type defined in lore-revision, not in `lore`.
    // Cargo does not re-run this build script when only a dependency crate changes, so watch the
    // event source explicitly to keep the generated header in sync with the event enum.
    println!("cargo:rerun-if-changed=../lore-revision/src/event.rs");

    // list input configuration files so that we run this script to update the c header
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let out_dir = env::var("OUT_DIR").unwrap();
    let header_gen = format!("{out_dir}{path_sep}lore.h");
    let source_gen = format!("{out_dir}{path_sep}lore.c");
    let config = cbindgen::Config::from_file("cbindgen.toml").unwrap();

    // run cbindgen to generate `lore.h`
    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => bindings.write_to_file(&header_gen), // note this only writes if there was a change
        Err(cbindgen::Error::ParseSyntaxError { .. }) => return Ok(()), // ignore in favor of cargo's syntax check
        Err(err) => panic!("{err}"),
    };

    // patch up `lore.h` manually
    let contents = std::fs::read_to_string(&header_gen).expect("Unable to read lore.h");
    // cbindgen uses "_T" for renamed enum variant field names
    // For example: LORE_OPERATING_MODE_T_CLIENT instead of LORE_OPERATING_MODE_CLIENT
    let contents = contents.replace("_T_", "_");
    // cbindgen generates "_t_Tag" for enum unions
    // For example: lore_metadata_t_Tag, lore_event_t_Tag
    let contents = contents.replace("_t_Tag {", "_tag_t {");
    let contents = contents.replace("_t_Tag;", "_tag_t;");
    let contents = contents.replace("_t_Tag tag;", "_tag_t tag;");
    let contents = contents.replace("enum lore_event_tag_t {", "enum lore_event_id_t {");
    // Fill out empty structs with a dummy field
    let contents = contents.replace("{\n\n} lore_", "{\n  int _unused;\n} lore_");

    // Inject the interface version
    let version_string = env!("CARGO_PKG_VERSION", "Need package version").to_string();
    let contents = contents.replace(
        "#include <stdlib.h>",
        format!("#include <stdlib.h>\n\n#define LORE_INTERFACE_VERSION \"{version_string}\"")
            .as_str(),
    );

    let expression = Regex::new(r"(?m)^ {4}struct\s*\{\s*\n {6}(.+?);\s*\n {4}\};")
        .expect("Failed to create regex to clean extra structs");
    let contents = expression
        .replace_all(contents.as_str(), "    ${1};")
        .to_string();

    // Rewrite cbindgen's associated-const macros from `lore_foo_t_BAR` to
    // `LORE_FOO_BAR`. cbindgen emits the type name verbatim as a prefix
    // for Rust `impl T { pub const BAR }` items, producing
    // `lore_store_t_INVALID`; the `_t_` splice reads awkwardly in C.
    let const_re = Regex::new(r"#define (lore_\w+)_t_(\w+)")
        .expect("Failed to create regex for associated-const macros");
    let contents = const_re
        .replace_all(contents.as_str(), |caps: &regex::Captures<'_>| {
            let type_stem = caps.get(1).unwrap().as_str();
            let const_name = caps.get(2).unwrap().as_str();
            format!("#define {}_{}", type_stem.to_uppercase(), const_name)
        })
        .to_string();

    // Verify no Rust-internal names leak into the public C header.
    // Check for any "urc" reference as a prefix/identifier (not as substring of other words like "resource")
    let urc_re = Regex::new(r"(?i)\burc_").expect("Failed to create leak check regex");
    if let Some(line) = contents.lines().find(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with("//")
            && !trimmed.starts_with('*')
            && !trimmed.starts_with("/*")
            && urc_re.is_match(line)
    }) {
        panic!("lore.h header contains 'urc_' reference, update lore/cbindgen.toml: {line}");
    }

    // Check for any CamelCase "Lore" prefix in non-comment lines
    if let Some(line) = contents.lines().find(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with("//")
            && !trimmed.starts_with('*')
            && !trimmed.starts_with("/*")
            && line.contains("Lore")
    }) {
        panic!("lore.h header contains camel cased 'Lore' type, update lore/cbindgen.toml: {line}");
    }
    std::fs::write(&header_gen, &contents).expect("Unable to write patched lore.h");

    // Validate the header by compiling a basic C file including it
    let mut cc_base_builder = cc::Build::new();
    let cc_builder = cc_base_builder
        .cargo_metadata(false)
        .static_crt(true)
        .force_frame_pointer(false)
        .opt_level(3);

    if cc_builder.get_compiler().is_like_msvc() {
        cc_builder.flag("/std:c11");
    }

    std::fs::write(
        source_gen.as_str(),
        "\
        #include \"lore.h\"
        int main(void) {
            const struct lore_global_args_t globals = {0};
            const struct lore_repository_clone_args_t args = {0};
            struct lore_event_callback_config_t callback = {0};
            // A consumer that reads only the original Complete field must still
            // compile and link after error detail was appended to the struct.
            const struct lore_complete_event_data_t complete = {0};
            (void)complete.status;
            return lore_repository_clone(&globals, &args, callback);
        }
        ",
    )
    .expect("Unable to write test source file");

    cc_builder.clone().file(source_gen).compile("headertest");

    // Also validate the header is C++ compatible by compiling a C++ file
    // including it. Skipped for musl targets: the CI musl toolchain
    // (musl-tools) ships only a C compiler (musl-gcc), no musl g++, and this is
    // purely a header-compatibility check — the gnu/macOS/Windows builds still
    // exercise the C++ path, so coverage is unchanged.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("musl") {
        let cpp_source_gen = format!("{out_dir}{path_sep}lore.cpp");
        let mut cxx_base_builder = cc::Build::new();
        let cxx_builder = cxx_base_builder
            .cpp(true)
            .cargo_metadata(false)
            .static_crt(true)
            .force_frame_pointer(false)
            .opt_level(3);

        if cxx_builder.get_compiler().is_like_msvc() {
            cxx_builder.flag("/std:c++14");
        }

        std::fs::write(
            cpp_source_gen.as_str(),
            "\
            extern \"C\" {
            #include \"lore.h\"
            }
            int main(void) {
                const struct lore_global_args_t globals = {};
                const struct lore_repository_clone_args_t args = {};
                struct lore_event_callback_config_t callback = {};
                // A consumer that reads only the original Complete field must still
                // compile and link after error detail was appended to the struct.
                const struct lore_complete_event_data_t complete = {};
                (void)complete.status;
                return lore_repository_clone(&globals, &args, callback);
            }
            ",
        )
        .expect("Unable to write C++ test source file");

        cxx_builder
            .clone()
            .file(cpp_source_gen)
            .compile("headertest_cpp");
    }

    // if the header contents changed, copy it to the /lore-capi directory
    let header_target = format!("{crate_dir}{path_sep}..{path_sep}lore-capi{path_sep}lore.h");
    let contents_old =
        std::fs::read_to_string(&header_target).expect("Unable to read /lore-capi/lore.h");
    let contents_new = contents;
    let hash_old = blake3::hash(contents_old.as_bytes());
    let hash_new = blake3::hash(contents_new.as_bytes());
    if hash_old != hash_new {
        fs::copy(&header_gen, header_target).expect("Unable to write /lore-capi/lore.h");
    }

    let profile_dir = profile_dir();
    let header_target = format!("{profile_dir}{path_sep}lore.h");
    fs::copy(&header_gen, header_target).expect("Unable to write {header_target}");

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos" {
        let dylib_name = "liblore.dylib";
        println!("cargo:rustc-link-arg=-Wl,-install_name,@rpath/{dylib_name}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        // Hack around EXE and DLL having the same file name for PDB file
        println!("cargo:rustc-link-arg-cdylib=/PDB:{profile_dir}\\lore.dll.pdb");
    }

    Ok(())
}
