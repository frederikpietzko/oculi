//! Persistence layer: versioned settings storage, output validation and
//! non-atomic filename proposals.

pub mod naming;
pub mod settings;

pub use naming::{
    NamingError, TemplateContext, ensure_writable_directory, expand_template, reserve_filename,
    sanitize_component, validate_template,
};
pub use settings::{
    PersistedSettings, StorageError, default_settings_dir, default_settings_path, load_settings,
    save_settings,
};

pub mod reservation;
