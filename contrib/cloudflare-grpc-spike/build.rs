fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../lore-proto/proto";
    let files = [
        "../../lore-proto/proto/lore/environment/v1/environment.proto",
        "../../lore-proto/proto/lore/storage/v1/storage.proto",
    ];
    for file in files {
        println!("cargo:rerun-if-changed={file}");
    }
    println!("cargo:rerun-if-changed={root}/lore/model/v1/model.proto");
    prost_build::compile_protos(&files, &[root])?;
    Ok(())
}
