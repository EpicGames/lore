// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The filter workload the benchmarks share.
//!
//! Lives under `tests/support/` so cargo does not build it as a test target of
//! its own; each benchmark pulls it in with `#[path]`.

#![allow(dead_code)]

/// A hundred-line filter of the shape a real view or ignore file has: blanket
/// and rooted exclusions, extension and bare-name rules, directory-only rules,
/// wildcards in every position, and re-inclusions carving paths back out at
/// several depths.
pub fn rules() -> Vec<String> {
    // Blanket and top-level exclusions.
    let mut out = vec![
        "/Intermediate".to_string(),
        "/Saved".to_string(),
        "/DerivedDataCache".to_string(),
        "/Build".to_string(),
    ];

    // Bare-name and extension rules, applying at any depth.
    for name in [
        "node_modules",
        "__pycache__",
        ".DS_Store",
        "Thumbs.db",
        "target",
        "obj",
        "bin",
    ] {
        out.push(name.to_string());
    }
    for ext in [
        "*.tmp", "*.log", "*.o", "*.a", "*.pdb", "*.obj", "*.swp", "*.bak", "*.orig", "*.rej",
        "*.dll", "*.exe", "*.so", "*.dylib", "*.lib", "*.exp", "*.ilk", "*.idb",
    ] {
        out.push(ext.to_string());
    }

    // Directory-only rules.
    for dir in [
        "Binaries/",
        "Intermediate/",
        "Saved/",
        "Cache/",
        "Logs/",
        "Temp/",
        "Backup/",
    ] {
        out.push(dir.to_string());
    }

    // Wildcards in leading, interior and trailing position.
    out.push("*Editor/Intermediate".to_string());
    out.push("Engine/*/Intermediate".to_string());
    out.push("Engine/Plugins/*/Binaries".to_string());
    out.push("Content/**/Temp".to_string());
    out.push("Engine/Content/*.uasset.bak".to_string());
    out.push("*/Saved/Logs".to_string());
    out.push("*/Intermediate/Build".to_string());
    out.push("Engine/**/Binaries".to_string());
    out.push("**/Saved/Config".to_string());
    out.push("Game/*/*.tmp".to_string());
    out.push("*/*/DerivedDataCache".to_string());

    // Excluded trees with re-inclusions carved back out, at several depths.
    for (index, project) in [
        "Engine",
        "Game",
        "Plugins",
        "Programs",
        "Templates",
        "Samples",
    ]
    .iter()
    .enumerate()
    {
        out.push(format!("/{project}/Intermediate"));
        out.push(format!("/{project}/Binaries"));
        out.push(format!("!/{project}/Binaries/Win64"));
        out.push(format!("/{project}/Binaries/Win64/*.pdb"));
        out.push(format!("/{project}/Content/Developers"));
        out.push(format!("!/{project}/Content/Developers/Shared"));
        if index % 2 == 0 {
            out.push(format!("!/{project}/Intermediate/Config"));
        }
    }

    // A sparse-view tail: exclude everything, then name paths back in.
    out.push("Restricted/**".to_string());
    for module in [
        "Core",
        "CoreUObject",
        "Engine",
        "RenderCore",
        "RHI",
        "Slate",
        "SlateCore",
        "InputCore",
        "Json",
        "Sockets",
        "Networking",
        "AudioMixer",
        "Chaos",
    ] {
        out.push(format!("!Restricted/Source/{module}"));
    }

    out.truncate(100);
    assert_eq!(out.len(), 100, "the filter should have 100 lines");
    out
}

/// Paths spanning the outcomes a walk actually meets: plainly included, excluded
/// by each rule kind, re-included below an exclusion, and deep paths that match
/// nothing and so cost a full scan of the line list.
pub fn probe_paths() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (path, is_dir) in [
        // Included, matching nothing -- the worst case for a scan.
        ("Engine/Source/Runtime/Core/Private/Misc/Paths.cpp", false),
        ("Game/Content/Maps/Arena/Arena_Persistent.umap", false),
        ("Plugins/Editor/Foo/Source/Private/Bar.cpp", false),
        ("Programs/UnrealBuildTool/Configuration/Target.cs", false),
        // Excluded by a rooted rule, and paths below it.
        ("Intermediate", true),
        ("Intermediate/Build/Win64/Foo.obj", false),
        ("Engine/Intermediate/Build/Module.h", false),
        // Excluded by a bare-name rule at depth.
        ("Game/Source/Web/node_modules/react/index.js", false),
        ("Engine/Source/target/debug/build.rs", false),
        // Excluded by an extension rule at depth.
        ("Engine/Source/Runtime/Core/Private/scratch.tmp", false),
        ("Game/Saved/Logs/Game.log", false),
        // Directory-only rules.
        ("Engine/Binaries", true),
        ("Engine/Binaries/Win64/UnrealEditor.exe", false),
        // Wildcards in each position.
        ("FooEditor/Intermediate/Build/x.h", false),
        ("Engine/Plugins/Niagara/Binaries/Win64/x.dll", false),
        ("Content/Characters/Hero/Temp/x.uasset", false),
        // Re-included below an exclusion, and its excluded siblings.
        ("Engine/Binaries/Win64/UnrealEditor.exe", false),
        ("Engine/Binaries/Win64/UnrealEditor.pdb", false),
        ("Engine/Binaries/Linux/UnrealEditor", false),
        ("Game/Content/Developers/Shared/Common.uasset", false),
        ("Game/Content/Developers/alice/Test.uasset", false),
        ("Engine/Intermediate/Config/Base.ini", false),
        // Sparse-view tail.
        ("Restricted/Source/Core/Public/Core.h", false),
        ("Restricted/Source/Physics/Public/Physics.h", false),
        ("Restricted/Docs/README.md", false),
        // Deep paths, to show the cost of walking many components.
        ("Engine/Plugins/Runtime/A/B/C/D/E/F/Deep.cpp", false),
        ("Game/Content/A/B/C/D/E/F/G/H/Asset.uasset", false),
    ] {
        out.push((path.to_string(), is_dir));
    }
    out
}

/// A targets-file workload: many files spread over far fewer directories, which
/// is what `lore stage --targets` is handed.
pub fn target_list(dirs: usize, files_per_dir: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(dirs * files_per_dir);
    for dir in 0..dirs {
        // A spread of roots so the batch crosses excluded, re-included and
        // untouched trees rather than sitting in one of them.
        let root = match dir % 5 {
            0 => "Engine/Source",
            1 => "Game/Content",
            2 => "Engine/Binaries/Win64",
            3 => "Restricted/Source",
            _ => "Plugins/Runtime",
        };
        for file in 0..files_per_dir {
            out.push(format!("{root}/Mod{dir:04}/Sub/File{file:03}.cpp"));
        }
    }
    out.sort();
    out
}
