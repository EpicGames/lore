use std::path::Path;
use std::path::PathBuf;

use lore_error_set::ForwardStrict;
use lore_error_set::error_set;

use crate::global::get_global_data_dir;
use crate::instance::InstanceId;
use crate::repository::DOT_LORE;

#[error_set]
pub enum ExternalDirError {}

const EXTERNAL: &str = "external";
const WRITE: &str = "write";

pub fn external_dir_for_instance(instance_id: InstanceId) -> Result<PathBuf, ExternalDirError> {
    let global =
        get_global_data_dir().forward::<ExternalDirError>("Missing global data directory")?;
    Ok(global
        .join(EXTERNAL)
        .join(Path::new(instance_id.text_encoding().as_str())))
}

pub fn external_lore_dir(instance_id: InstanceId) -> Result<PathBuf, ExternalDirError> {
    Ok(external_dir_for_instance(instance_id)?.join(DOT_LORE))
}

pub fn external_write_dir(instance_id: InstanceId) -> Result<PathBuf, ExternalDirError> {
    Ok(external_dir_for_instance(instance_id)?.join(WRITE))
}

pub fn check_for_external_lore_dir(
    instance_id: InstanceId,
) -> Result<Option<PathBuf>, ExternalDirError> {
    let path = external_lore_dir(instance_id)?;
    Ok(if path.exists() { Some(path) } else { None })
}
