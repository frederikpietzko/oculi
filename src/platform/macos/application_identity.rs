//! Process-backed application identity; no native objects leave the calling worker.

use crate::model::ApplicationProcessId;
use objc2::rc::Retained;
use objc2_app_kit::NSRunningApplication;
use objc2_screen_capture_kit::{SCRunningApplication, SCShareableContent};
use std::collections::BTreeMap;
use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

static SESSION: OnceLock<Uuid> = OnceLock::new();

pub(super) struct ProcessMetadata {
    pub id: ApplicationProcessId,
    pub name: String,
}

pub(super) fn process_metadata(pid: i32) -> Option<ProcessMetadata> {
    if pid <= 0 {
        return None;
    }
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: libproc writes into an aligned, correctly sized BSD info buffer.
    // Read it only when the kernel reports the complete structure was written.
    let info = unsafe {
        let bytes = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        );
        if bytes != size_of::<libc::proc_bsdinfo>() as i32 {
            return None;
        }
        info.assume_init()
    };
    if info.pbi_pid != pid as u32 || info.pbi_start_tvsec == 0 {
        return None;
    }
    let name = |bytes: &[libc::c_char]| {
        String::from_utf8_lossy(
            &bytes
                .iter()
                .take_while(|b| **b != 0)
                .map(|b| *b as u8)
                .collect::<Vec<_>>(),
        )
        .into_owned()
    };
    let mut label = executable_label(pid).unwrap_or_else(|| name(&info.pbi_name));
    if label.trim().is_empty() {
        label = name(&info.pbi_comm);
    }
    Some(ProcessMetadata {
        id: ApplicationProcessId {
            pid,
            start_time: [info.pbi_start_tvsec, info.pbi_start_tvusec],
            session: *SESSION.get_or_init(Uuid::new_v4),
        },
        name: process_label(&label, pid),
    })
}

pub(super) fn is_current(process: &ApplicationProcessId) -> bool {
    process_metadata(process.pid).is_some_and(|metadata| metadata.id == *process)
}

fn process_label(label: &str, pid: i32) -> String {
    if label.trim().is_empty() {
        format!("Process {pid}")
    } else {
        label.to_owned()
    }
}

fn executable_label(pid: i32) -> Option<String> {
    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: The output is a bounded byte buffer. Its path is used only as a
    // human-readable label, never to merge processes or infer a bundle identity.
    let bytes = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if bytes <= 0 {
        return None;
    }
    let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
    let path = String::from_utf8_lossy(&path[..end]);
    label_from_executable(Path::new(path.as_ref()))
}

fn label_from_executable(path: &Path) -> Option<String> {
    let executable = path.file_name()?.to_string_lossy();
    for parent in path.ancestors().skip(1) {
        if parent
            .extension()
            .is_some_and(|extension| extension == "app")
        {
            return Some(format!(
                "{} ({executable})",
                parent.file_stem()?.to_string_lossy()
            ));
        }
    }
    Some(executable.into_owned())
}

pub(super) fn resolve_metadata(
    pid: i32,
    mut bundle: String,
    mut label: String,
) -> (String, String) {
    if (bundle.is_empty() || label.trim().is_empty())
        && let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        && !app.isTerminated()
    {
        if bundle.is_empty() {
            bundle = app
                .bundleIdentifier()
                .map(|id| id.to_string())
                .unwrap_or_default();
        }
        if label.trim().is_empty() {
            label = app
                .localizedName()
                .map(|name| name.to_string())
                .unwrap_or_default();
        }
    }
    if label.trim().is_empty() {
        label = process_metadata(pid)
            .map(|metadata| metadata.name)
            .unwrap_or_default();
    }
    (bundle, process_label(&label, pid))
}

/// Include owners reported only through windows, without grouping by title/name.
pub(super) unsafe fn shareable_applications(
    content: &SCShareableContent,
) -> Vec<Retained<SCRunningApplication>> {
    // SAFETY: Immutable snapshot and retained children stay on the calling worker.
    unsafe {
        let mut apps = BTreeMap::new();
        for app in content.applications().iter().chain(
            content
                .windows()
                .iter()
                .filter_map(|window| window.owningApplication()),
        ) {
            let pid = app.processID();
            if pid > 0 {
                apps.entry(pid).or_insert(app);
            }
        }
        apps.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_identity_rejects_recycled_pids_and_restored_sessions() {
        let process = process_metadata(std::process::id() as i32).unwrap();
        assert!(!process.name.trim().is_empty());
        assert!(is_current(&process.id));
        let mut recycled = process.id;
        recycled.start_time[1] += 1;
        assert!(!is_current(&recycled));
        let mut restored = process.id;
        restored.session = Uuid::nil();
        assert!(!is_current(&restored));
        assert!(process_metadata(0).is_none());
        assert!(process_metadata(-1).is_none());
        assert!(process_metadata(i32::MAX).is_none());
    }

    #[test]
    fn blank_names_have_a_diagnostic_process_label() {
        assert_eq!(process_label("", 42), "Process 42");
        assert_eq!(process_label("  ", 42), "Process 42");
        assert_eq!(process_label("java", 42), "java");
    }

    #[test]
    fn executable_path_improves_labels_without_becoming_identity() {
        assert_eq!(
            label_from_executable(Path::new(
                "/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/bin/java"
            )),
            Some("IntelliJ IDEA (java)".into()),
        );
        assert_eq!(
            label_from_executable(Path::new("/opt/jdk/bin/java")),
            Some("java".into())
        );
        assert_eq!(label_from_executable(Path::new("/")), None);
    }
}
