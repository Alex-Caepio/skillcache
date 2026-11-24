use crate::error::Result;
use std::path::Path;

pub fn private_directory(path: &Path) -> Result<()> {
    crate::custody::security::secure_private_dir(path)
}

pub fn private_file(path: &Path) -> Result<()> {
    crate::custody::security::secure_private_file(path)
}
