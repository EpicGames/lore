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

/// One node of [`tree`].
pub struct TreeNode {
    pub name: String,
    pub is_dir: bool,
    pub children: Vec<u32>,
}

/// Adds directories and files to a node arena.
struct TreeBuilder {
    nodes: Vec<TreeNode>,
}

impl TreeBuilder {
    fn new() -> Self {
        Self {
            nodes: vec![TreeNode {
                name: String::new(),
                is_dir: true,
                children: Vec::new(),
            }],
        }
    }

    fn add(&mut self, parent: usize, name: &str, is_dir: bool) -> usize {
        let index = self.nodes.len();
        self.nodes.push(TreeNode {
            name: name.to_owned(),
            is_dir,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(index as u32);
        index
    }

    fn dir(&mut self, parent: usize, name: &str) -> usize {
        self.add(parent, name, true)
    }

    fn file(&mut self, parent: usize, name: &str) {
        self.add(parent, name, false);
    }

    /// A chain of directories, returning the deepest.
    fn chain(&mut self, parent: usize, names: &[&str]) -> usize {
        let mut at = parent;
        for name in names {
            at = self.dir(at, name);
        }
        at
    }
}

/// A tree of the shape [`rules`] describes, as a node arena a walk descends
/// without parsing paths.
///
/// The point is that a walk over it meets every case the rules set up: subtrees
/// pruned at the root, exclusions re-included one level down and re-excluded the
/// level below that, directory-only and bare-name rules biting at depth, and
/// deep paths matching nothing at all. Timing a walk over a tree that only ever
/// matched one rule kind would say nothing about a real one.
///
/// Held as an arena rather than a path list so a walk costs a name push per node
/// and nothing else: the filter is what is being timed, not path arithmetic.
pub fn tree(modules: usize, files_per_dir: usize) -> Vec<TreeNode> {
    let mut b = TreeBuilder::new();
    let root = 0;

    // Subtrees a rooted rule prunes whole. A walk should reach the directory and
    // stop, so these cost one call each however much is under them.
    for pruned in ["Intermediate", "Saved", "Build", "DerivedDataCache"] {
        let at = b.chain(root, &[pruned, "Win64", "Inner"]);
        for file in 0..files_per_dir {
            b.file(at, &format!("Pruned{file:03}.obj"));
        }
    }

    for project in [
        "Engine",
        "Game",
        "Plugins",
        "Programs",
        "Templates",
        "Samples",
    ] {
        let project_node = b.dir(root, project);

        // Source: mostly matching nothing, which is the full line scan.
        let runtime = b.chain(project_node, &["Source", "Runtime"]);
        for module in 0..modules {
            let module_node = b.dir(runtime, &format!("Module{module:04}"));
            for visibility in ["Public", "Private"] {
                let at = b.dir(module_node, visibility);
                for file in 0..files_per_dir {
                    b.file(at, &format!("File{file:03}.cpp"));
                    b.file(at, &format!("File{file:03}.h"));
                }
                // Extension rules biting at depth.
                b.file(at, "scratch.tmp");
                b.file(at, "build.log");
                b.file(at, "Module.o");
            }
            // Bare-name rules biting at depth.
            let modules_dir = b.chain(module_node, &["Web", "node_modules", "react"]);
            for file in 0..files_per_dir {
                b.file(modules_dir, &format!("index{file:03}.js"));
            }
            let target = b.chain(module_node, &["target", "debug"]);
            for file in 0..files_per_dir {
                b.file(target, &format!("artifact{file:03}.rlib"));
            }
        }

        // Content, including the developers tree whose Shared branch is
        // re-included below an exclusion.
        let maps = b.chain(project_node, &["Content", "Maps"]);
        for module in 0..modules {
            let at = b.dir(maps, &format!("Area{module:04}"));
            for file in 0..files_per_dir {
                b.file(at, &format!("Asset{file:03}.uasset"));
            }
        }
        let developers = b.chain(project_node, &["Content", "Developers"]);
        for developer in ["Shared", "alice", "bob", "carol"] {
            let at = b.dir(developers, developer);
            for file in 0..files_per_dir {
                b.file(at, &format!("Work{file:03}.uasset"));
            }
        }

        // Binaries: excluded, with Win64 re-included and its .pdb re-excluded.
        let binaries = b.dir(project_node, "Binaries");
        for platform in ["Win64", "Linux", "Mac"] {
            let at = b.dir(binaries, platform);
            for module in 0..modules {
                b.file(at, &format!("Module{module:04}.dll"));
                b.file(at, &format!("Module{module:04}.pdb"));
                b.file(at, &format!("Module{module:04}.exe"));
            }
        }

        // Intermediate: excluded, with Config re-included for half the projects.
        let intermediate = b.dir(project_node, "Intermediate");
        let build = b.chain(intermediate, &["Build", "Win64"]);
        for module in 0..modules {
            b.file(build, &format!("Module{module:04}.obj"));
        }
        let config = b.dir(intermediate, "Config");
        for file in 0..files_per_dir {
            b.file(config, &format!("Base{file:03}.ini"));
        }

        // Nested plugins, each with the same excluded/re-included shape.
        let plugins = b.dir(project_node, "Plugins");
        for plugin in 0..modules / 4 {
            let plugin_node = b.dir(plugins, &format!("Plugin{plugin:04}"));
            let source = b.chain(plugin_node, &["Source", "Private"]);
            for file in 0..files_per_dir {
                b.file(source, &format!("File{file:03}.cpp"));
            }
            let plugin_binaries = b.chain(plugin_node, &["Binaries", "Win64"]);
            for file in 0..files_per_dir {
                b.file(plugin_binaries, &format!("Plugin{file:03}.dll"));
            }
            let temp = b.chain(plugin_node, &["Content", "Temp"]);
            for file in 0..files_per_dir {
                b.file(temp, &format!("Cooked{file:03}.uasset"));
            }
        }

        let logs = b.chain(project_node, &["Saved", "Logs"]);
        for file in 0..files_per_dir {
            b.file(logs, &format!("Run{file:03}.log"));
        }
    }

    // The sparse-view tail: everything under Restricted excluded, named modules
    // back in. Half the modules here are named, half are not.
    let restricted_source = b.chain(root, &["Restricted", "Source"]);
    let named = [
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
    ];
    for module in named {
        let at = b.chain(restricted_source, &[module, "Public"]);
        for file in 0..files_per_dir {
            b.file(at, &format!("{module}{file:03}.h"));
        }
    }
    for module in 0..modules {
        let at = b.chain(
            restricted_source,
            &[&format!("Private{module:04}"), "Public"],
        );
        for file in 0..files_per_dir {
            b.file(at, &format!("Hidden{file:03}.h"));
        }
    }
    let docs = b.chain(root, &["Restricted", "Docs"]);
    for file in 0..files_per_dir {
        b.file(docs, &format!("README{file:03}.md"));
    }

    // A deep chain, where a whole-path query pays for depth and a walk does not.
    let deep = b.chain(
        root,
        &["Deep", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J"],
    );
    for file in 0..files_per_dir * 4 {
        b.file(deep, &format!("Deep{file:03}.cpp"));
    }

    b.nodes
}
