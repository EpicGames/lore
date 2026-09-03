use lore_base::error::InvalidPath;
use lore_error_set::error_set;

#[error_set]
pub enum ServiceMainError {
    InvalidPath,
}
