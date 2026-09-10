//! Filename templating, sanitization, and non-atomic destination proposals.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;

/// Characters that are illegal or awkward in filenames across macOS/HFS+/APFS
/// and are stripped/replaced during sanitization.
const ILLEGAL_CHARS: &[char] = &['/', ':', '\\', '\0'];

/// Replaces characters that are illegal (or simply awkward) in filenames with
/// underscores, trims surrounding whitespace/dots, and falls back to a
/// placeholder if the result would be empty.
pub fn sanitize_component(input: &str) -> String {
    let mut sanitized: String = input
        .chars()
        .map(|c| {
            if ILLEGAL_CHARS.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    sanitized = sanitized.trim().trim_matches('.').to_string();

    if sanitized.is_empty() {
        "recording".to_string()
    } else {
        sanitized
    }
}

/// Inputs used to expand a filename template.
#[derive(Debug, Clone)]
pub struct TemplateContext {
    pub name: String,
    pub timestamp: OffsetDateTime,
    pub counter: u32,
}

/// Validates a filename template with only `{name}`, `{date}`, `{time}`, and
/// `{counter}` placeholders. Braces must be balanced; literal braces and empty
/// placeholders are not supported in templates (but are allowed in names).
/// The stem must not be empty/whitespace/dots. An explicit extension must be
/// `.mp4` (case-insensitive); no extension means `.mp4` will be appended.
pub fn validate_template(template: &str) -> Result<(), String> {
    if template.trim().is_empty() {
        return Err("Filename template must not be empty".to_string());
    }
    render_template(template, |_| String::new())?;
    let stem = match template.rsplit_once('.') {
        Some((stem, extension)) => {
            if !extension.eq_ignore_ascii_case("mp4") {
                return Err("Filename template extension must be .mp4".to_string());
            }
            stem
        }
        None => template,
    };
    if stem
        .trim_matches(|c: char| c.is_whitespace() || c == '.')
        .is_empty()
    {
        return Err("Filename template stem must not be empty".to_string());
    }
    Ok(())
}

fn render_template(
    template: &str,
    mut replacement: impl FnMut(&str) -> String,
) -> Result<String, String> {
    let mut rendered = String::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut token = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => {
                            return Err("Unbalanced filename template braces".to_string());
                        }
                        Some(c) => token.push(c),
                    }
                }
                if !matches!(token.as_str(), "name" | "date" | "time" | "counter") {
                    return Err(format!("Unsupported filename placeholder: {{{token}}}"));
                }
                rendered.push_str(&replacement(&token));
            }
            '}' => return Err("Unbalanced filename template braces".to_string()),
            c => rendered.push(c),
        }
    }
    Ok(rendered)
}

/// Expands a filename template previously checked by `validate_template`.
/// Replacement text is literal, never interpreted as further placeholders.
/// Supported placeholders:
/// - `{name}`: sanitized recording name
/// - `{date}`: `YYYY-MM-DD`
/// - `{time}`: `HH-MM-SS`
/// - `{counter}`: numeric collision-avoidance counter
///
/// The extension is preserved verbatim from the template (defaulting to
/// `.mp4` if the template has none).
pub fn expand_template(template: &str, ctx: &TemplateContext) -> String {
    let date = format_date(ctx.timestamp);
    let time = format_time(ctx.timestamp);
    let name = sanitize_component(&ctx.name);

    let mut expanded = render_template(template, |token| match token {
        "name" => name.clone(),
        "date" => date.clone(),
        "time" => time.clone(),
        "counter" => ctx.counter.to_string(),
        _ => unreachable!("render_template validates placeholders"),
    })
    .unwrap_or_else(|_| template.to_string());

    if !template.to_lowercase().ends_with(".mp4") {
        expanded.push_str(".mp4");
    }

    // Sanitize the whole expanded filename except the final extension, in
    // case placeholders introduced illegal characters (they shouldn't, but
    // user-provided template text around them might).
    resanitize_filename(&expanded)
}

fn resanitize_filename(filename: &str) -> String {
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (filename, None),
    };
    let clean_stem: String = stem
        .chars()
        .map(|c| {
            if ILLEGAL_CHARS.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    match ext {
        Some(e) => format!("{clean_stem}.{e}"),
        None => clean_stem,
    }
}

fn format_date(ts: OffsetDateTime) -> String {
    format!("{:04}-{:02}-{:02}", ts.year(), ts.month() as u8, ts.day())
}

fn format_time(ts: OffsetDateTime) -> String {
    format!("{:02}-{:02}-{:02}", ts.hour(), ts.minute(), ts.second())
}

/// Errors returned while proposing a destination or checking writability.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NamingError {
    #[error("destination directory is not writable: {0}")]
    DirectoryNotWritable(String),
    #[error("could not find a non-colliding filename after {0} attempts")]
    ExhaustedAttempts(u32),
}

const MAX_COUNTER_ATTEMPTS: u32 = 10_000;

/// Proposes an unused filename in `directory` for a validated template/context.
/// Also considers `already_reserved`, a caller-maintained set of paths proposed
/// earlier in the same group start. This performs existence checks only: it does
/// not create a file or atomically reserve a path. The future writer must protect
/// against races with other writers before creating the destination.
/// Without `{counter}`, an existing destination exhausts attempts unchanged.
///
/// Returns the full destination path plus the counter value actually used.
pub fn reserve_filename(
    directory: &Path,
    template: &str,
    base_ctx: &TemplateContext,
    already_reserved: &HashSet<PathBuf>,
) -> Result<(PathBuf, u32), NamingError> {
    for counter in base_ctx.counter..base_ctx.counter.saturating_add(MAX_COUNTER_ATTEMPTS) {
        let ctx = TemplateContext {
            name: base_ctx.name.clone(),
            timestamp: base_ctx.timestamp,
            counter,
        };
        let filename = expand_template(template, &ctx);
        let candidate = directory.join(&filename);
        if !candidate.exists() && !already_reserved.contains(&candidate) {
            return Ok((candidate, counter));
        }
    }
    Err(NamingError::ExhaustedAttempts(MAX_COUNTER_ATTEMPTS))
}

/// Verifies that `directory` exists (creating it if necessary) and is
/// writable, by creating and removing a temporary probe file.
pub fn ensure_writable_directory(directory: &Path) -> Result<(), NamingError> {
    if !directory.exists() {
        std::fs::create_dir_all(directory)
            .map_err(|e| NamingError::DirectoryNotWritable(e.to_string()))?;
    }
    let probe = directory.join(format!(".oculi-write-probe-{}", Uuid::new_v4()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| NamingError::DirectoryNotWritable(e.to_string()))?;
    drop(file);
    // Only remove a probe that this call successfully created.
    std::fs::remove_file(&probe).map_err(|e| NamingError::DirectoryNotWritable(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn ctx(name: &str, counter: u32) -> TemplateContext {
        TemplateContext {
            name: name.to_string(),
            timestamp: datetime!(2026-09-10 17:25:30 UTC),
            counter,
        }
    }

    #[test]
    fn write_probe_preserves_preexisting_predictable_path() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir
            .path()
            .join(format!(".oculi-write-probe-{}", std::process::id()));
        std::fs::write(&probe, b"keep this data").unwrap();
        ensure_writable_directory(dir.path()).unwrap();
        assert_eq!(std::fs::read(&probe).unwrap(), b"keep this data");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn expand_template_does_not_expand_placeholders_in_names() {
        let expanded = expand_template(
            "{name}_{date}_{counter}.mp4",
            &ctx("{name} {date} {time} {counter} {literal}", 3),
        );
        assert_eq!(
            expanded,
            "{name} {date} {time} {counter} {literal}_2026-09-10_3.mp4"
        );
    }

    #[test]
    fn output_rejects_invalid_templates() {
        for template in [
            "",
            "   ",
            "{}.mp4",
            "{unknown}.mp4",
            "{name.mp4",
            "name}.mp4",
            "{{name}}.mp4",
            ".mp4",
            "...mp4",
            " .mp4",
            "{name}.mov",
            "clip.",
        ] {
            let output = crate::model::OutputSettings {
                filename_template: template.into(),
                ..Default::default()
            };
            assert!(output.validate_format().is_err(), "accepted {template:?}");
        }
    }

    #[test]
    fn output_accepts_supported_templates() {
        for template in [
            "{name}_{date}_{time}_{counter}.mp4",
            "clip.MP4",
            "{name}",
            "clip",
        ] {
            let output = crate::model::OutputSettings {
                filename_template: template.into(),
                ..Default::default()
            };
            assert!(output.validate_format().is_ok(), "rejected {template:?}");
        }
    }

    #[test]
    fn sanitize_replaces_illegal_characters() {
        assert_eq!(sanitize_component("a/b:c\\d"), "a_b_c_d");
    }

    #[test]
    fn sanitize_empty_falls_back_to_placeholder() {
        assert_eq!(sanitize_component("   ..  "), "recording");
    }

    #[test]
    fn expand_template_fills_placeholders() {
        let expanded = expand_template("{name}_{date}_{time}_{counter}.mp4", &ctx("My Cam", 3));
        assert_eq!(expanded, "My Cam_2026-09-10_17-25-30_3.mp4");
    }

    #[test]
    fn expand_template_defaults_extension_when_missing() {
        let expanded = expand_template("{name}", &ctx("Cam", 0));
        assert!(expanded.ends_with(".mp4"));
    }

    #[test]
    fn expand_template_sanitizes_illegal_name_characters() {
        let expanded = expand_template("{name}.mp4", &ctx("A/B:C", 0));
        assert_eq!(expanded, "A_B_C.mp4");
    }

    #[test]
    fn reserve_filename_picks_first_free_counter() {
        let dir = tempfile::tempdir().unwrap();
        let template = "{name}_{counter}.mp4";
        let context = ctx("Cam", 0);

        // Pre-create counter 0 so the reservation must skip to 1.
        std::fs::write(dir.path().join("Cam_0.mp4"), b"").unwrap();

        let (path, counter) =
            reserve_filename(dir.path(), template, &context, &HashSet::new()).unwrap();
        assert_eq!(counter, 1);
        assert_eq!(path, dir.path().join("Cam_1.mp4"));
    }

    #[test]
    fn reserve_filename_respects_already_reserved_set() {
        let dir = tempfile::tempdir().unwrap();
        let template = "{name}_{counter}.mp4";
        let context = ctx("Cam", 0);

        let mut reserved = HashSet::new();
        reserved.insert(dir.path().join("Cam_0.mp4"));

        let (path, counter) = reserve_filename(dir.path(), template, &context, &reserved).unwrap();
        assert_eq!(counter, 1);
        assert_eq!(path, dir.path().join("Cam_1.mp4"));
    }

    #[test]
    fn reserve_filename_never_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let template = "fixed.mp4";
        let context = ctx("Cam", 0);
        std::fs::write(dir.path().join("fixed.mp4"), b"existing").unwrap();

        // With a fixed template (no counter placeholder) reservation should
        // still fail to find a free slot rather than overwrite.
        let result = reserve_filename(dir.path(), template, &context, &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn ensure_writable_directory_creates_missing_dirs() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("a").join("b");
        assert!(!nested.exists());
        ensure_writable_directory(&nested).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn ensure_writable_directory_rejects_file_as_directory() {
        let base = tempfile::tempdir().unwrap();
        let file_path = base.path().join("not_a_dir");
        std::fs::write(&file_path, b"x").unwrap();
        let result = ensure_writable_directory(&file_path);
        assert!(result.is_err());
    }
}
