use std::path::PathBuf;

/// Removes the file at the held path when dropped.
pub(crate) struct DeleteOnDrop(pub(crate) PathBuf);

impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "Could not remove temporary file {}: {error}",
                self.0.display()
            );
        }
    }
}
