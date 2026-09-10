//! Cooperative filename reservations and atomic, no-replacement publication.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use super::naming::{
    TemplateContext, ensure_writable_directory, expand_template, validate_template,
};

pub struct OutputReservation {
    destination: PathBuf,
    marker: PathBuf,
    marker_file: File,
    staging_directory: PathBuf,
    staging: PathBuf,
}

impl OutputReservation {
    pub fn reserve(
        directory: &Path,
        template: &str,
        context: &TemplateContext,
    ) -> Result<Self, String> {
        validate_template(template)?;
        ensure_writable_directory(directory).map_err(|e| e.to_string())?;
        for attempt in 0..10_000_u32 {
            let counter = context
                .counter
                .checked_add(attempt)
                .ok_or("Filename counter overflow")?;
            let mut filename = expand_template(
                template,
                &TemplateContext {
                    counter,
                    ..context.clone()
                },
            );
            if attempt > 0 && !template.contains("{counter}") {
                filename.insert_str(filename.len() - 4, &format!("_{counter}"));
            }
            let destination = directory.join(&filename);
            if occupied(&destination)? {
                continue;
            }
            let marker = directory.join(format!(".{filename}.oculi-reservation"));
            let marker_file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Reserve {}: {e}", destination.display())),
            };
            let staging_directory =
                directory.join(format!(".oculi-incomplete-{}", uuid::Uuid::new_v4()));
            if let Err(e) = std::fs::create_dir(&staging_directory) {
                remove_owned_marker(&marker, &marker_file);
                return Err(format!("Create staging directory: {e}"));
            }
            let reservation = Self {
                staging: staging_directory.join("incomplete.mp4"),
                destination,
                marker,
                marker_file,
                staging_directory,
            };
            if occupied(&reservation.destination)? {
                continue;
            }
            return Ok(reservation);
        }
        Err("No free recording filename after 10000 attempts".into())
    }

    pub fn staging_path(&self) -> &Path {
        &self.staging
    }
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Call only after successful writer completion. Hard-link creation is atomic
    /// and refuses existing files (including dangling symlinks). Staging is on the
    /// same filesystem; unsupported hard links fail safely, retaining the artifact.
    pub fn promote(self) -> Result<PathBuf, String> {
        let file =
            File::open(&self.staging).map_err(|e| format!("Open completed staging file: {e}"))?;
        if file.metadata().map_err(|e| e.to_string())?.len() == 0 {
            return Err("Writer produced an empty recording; incomplete artifact retained".into());
        }
        file.sync_all()
            .map_err(|e| format!("Sync recording: {e}"))?;
        std::fs::hard_link(&self.staging, &self.destination).map_err(|e| {
            format!(
                "Save {} without replacement: {e}; incomplete file retained at {}",
                self.destination.display(),
                self.staging.display()
            )
        })?;
        if let Err(e) = std::fs::remove_file(&self.staging) {
            log::warn!(
                "Saved {}, but staging cleanup failed: {e}",
                self.destination.display()
            );
        }
        Ok(self.destination.clone())
    }

    /// Preparation rollback, only after any writer has been cancelled. Ordinary
    /// drop never deletes partial media, including after errors or unwinding.
    pub fn abort(self) -> Result<(), String> {
        match std::fs::remove_file(&self.staging) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Remove cancelled staging output: {e}")),
        }
    }
}

impl Drop for OutputReservation {
    fn drop(&mut self) {
        remove_owned_marker(&self.marker, &self.marker_file);
        // remove_dir removes only an empty directory; failed media is preserved.
        let _ = std::fs::remove_dir(&self.staging_directory);
    }
}

fn occupied(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Inspect {}: {e}", path.display())),
    }
}

fn remove_owned_marker(path: &Path, file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(owned), Ok(current)) = (file.metadata(), std::fs::symlink_metadata(path))
            && owned.dev() == current.dev()
            && owned.ino() == current.ino()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> TemplateContext {
        TemplateContext {
            name: "My / recording".into(),
            timestamp: time::macros::datetime!(2026-09-10 19:33 UTC),
            counter: 1,
        }
    }

    #[test]
    fn concurrent_reservations_and_publication_do_not_collide() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = directory.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let r = OutputReservation::reserve(
                        &path,
                        "{name}_{date}_{time}_{counter}.mp4",
                        &context(),
                    )
                    .unwrap();
                    assert!(!r.staging_path().exists());
                    barrier.wait();
                    std::fs::write(r.staging_path(), b"completed").unwrap();
                    r.promote().unwrap()
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(paths.len(), 8);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 8);
    }

    #[test]
    fn external_collision_is_not_overwritten_and_partial_is_retained() {
        let directory = tempfile::tempdir().unwrap();
        let r = OutputReservation::reserve(directory.path(), "fixed.mp4", &context()).unwrap();
        let staging = r.staging_path().to_owned();
        let destination = r.destination().to_owned();
        std::fs::write(&staging, b"media").unwrap();
        std::fs::write(&destination, b"external").unwrap();
        assert!(
            r.promote()
                .unwrap_err()
                .contains("incomplete file retained")
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"external");
        assert_eq!(std::fs::read(staging).unwrap(), b"media");
        let next = OutputReservation::reserve(directory.path(), "fixed.mp4", &context()).unwrap();
        assert_eq!(next.destination().file_name().unwrap(), "fixed_2.mp4");
        next.abort().unwrap();
    }

    #[test]
    fn rollback_cleans_owned_files_but_drop_keeps_failed_media() {
        let directory = tempfile::tempdir().unwrap();
        let r = OutputReservation::reserve(directory.path(), "{counter}.mp4", &context()).unwrap();
        std::fs::write(r.staging_path(), b"cancelled").unwrap();
        r.abort().unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let r = OutputReservation::reserve(directory.path(), "{counter}.mp4", &context()).unwrap();
        let staging = r.staging_path().to_owned();
        std::fs::write(&staging, b"failed").unwrap();
        drop(r);
        assert_eq!(std::fs::read(staging).unwrap(), b"failed");
    }

    #[test]
    fn rejects_invalid_destination_template_and_empty_output() {
        let directory = tempfile::tempdir().unwrap();
        assert!(OutputReservation::reserve(directory.path(), "{unknown}.mp4", &context()).is_err());
        let file = directory.path().join("file");
        std::fs::write(&file, b"untouched").unwrap();
        assert!(OutputReservation::reserve(&file, "ok.mp4", &context()).is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"untouched");
        let r = OutputReservation::reserve(directory.path(), "ok.mp4", &context()).unwrap();
        std::fs::write(r.staging_path(), b"").unwrap();
        assert!(r.promote().is_err());
        assert!(!directory.path().join("ok.mp4").exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_occupied_and_replaced_markers_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("missing", directory.path().join("fixed.mp4")).unwrap();
        let r = OutputReservation::reserve(directory.path(), "fixed.mp4", &context()).unwrap();
        assert_eq!(r.destination().file_name().unwrap(), "fixed_2.mp4");
        let marker = r.marker.clone();
        std::fs::remove_file(&marker).unwrap();
        std::fs::write(&marker, b"replacement").unwrap();
        drop(r);
        assert_eq!(std::fs::read(marker).unwrap(), b"replacement");
    }
}
