//! Saved-value edits remain separate from runtime overrides and effective values.
use crate::{GuiArgs, StartupError};
use archive_config::*;

#[derive(Debug, Clone)]
pub struct SettingRow {
    pub key: ConfigKey,
    pub saved: Option<String>,
    pub effective: String,
    pub origin: Origin,
}
pub fn read(args: &GuiArgs, paths: &Paths) -> Result<(bool, Vec<SettingRow>), StartupError> {
    let effective = args
        .prepare(Paths {
            home: paths.home.clone(),
            cwd: paths.cwd.clone(),
        })?
        .settings;
    let saved = archive_config::read_document(paths, args.config.as_deref())?;
    let exists = effective
        .config_path
        .try_exists()
        .map_err(|e| StartupError {
            exit_code: 1,
            message: e.to_string(),
        })?;
    let rows = ConfigKey::ALL
        .into_iter()
        .map(|key| SettingRow {
            key,
            saved: saved
                .iter()
                .find(|v| v.key() == key)
                .map(ConfigValue::input_text),
            effective: effective.setting(key).0,
            origin: effective.setting(key).1,
        })
        .collect();
    Ok((exists, rows))
}
pub fn save(
    args: &GuiArgs,
    paths: &Paths,
    changes: &[(ConfigKey, Option<String>)],
) -> Result<(), StartupError> {
    // Parse and validate the entire batch before publishing any change.
    let changes: Vec<_> = changes
        .iter()
        .map(|(key, value)| match value {
            Some(value) => ConfigValue::parse(*key, value).map(Change::Set),
            None => Ok(Change::Unset(*key)),
        })
        .collect::<Result<_, _>>()?;
    archive_config::apply_changes(paths, args.config.as_deref(), &changes)?;
    Ok(())
}
