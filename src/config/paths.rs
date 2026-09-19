use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Entry-point config file, `$XDG_CONFIG_HOME/desktop-rs/config.nbcl`,
/// falling back to `$HOME/.config/desktop-rs/config.nbcl`.
pub fn entry_point() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.nbcl"))
}

fn config_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("desktop-rs"));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".config").join("desktop-rs"))
}

/// Config and examples embedded into the binary. First run never uses network.
pub const DEFAULT_CONFIG: &str = include_str!("../../assets/config.nbcl");

const BUNDLE: &[(&str, &str)] = &[
    ("config.nbcl", DEFAULT_CONFIG),
    (
        "themes/tokyo-night.nbcl",
        include_str!("../../assets/themes/tokyo-night.nbcl"),
    ),
    (
        "themes/gruvbox.nbcl",
        include_str!("../../assets/themes/gruvbox.nbcl"),
    ),
    (
        "themes/dracula.nbcl",
        include_str!("../../assets/themes/dracula.nbcl"),
    ),
    (
        "themes/catppuccin-latte.nbcl",
        include_str!("../../assets/themes/catppuccin-latte.nbcl"),
    ),
    (
        "themes/catppuccin-frappe.nbcl",
        include_str!("../../assets/themes/catppuccin-frappe.nbcl"),
    ),
    (
        "themes/catppuccin-macchiato.nbcl",
        include_str!("../../assets/themes/catppuccin-macchiato.nbcl"),
    ),
    (
        "themes/catppuccin-mocha.nbcl",
        include_str!("../../assets/themes/catppuccin-mocha.nbcl"),
    ),
    (
        "examples/minimal.nbcl",
        include_str!("../../assets/examples/minimal.nbcl"),
    ),
    (
        "examples/full.nbcl",
        include_str!("../../assets/examples/full.nbcl"),
    ),
    (
        "examples/launcher.nbcl",
        include_str!("../../assets/examples/launcher.nbcl"),
    ),
];

/// Writes the complete starter bundle if `config.nbcl` does not exist.
/// Existing installations are never modified, including missing example files.
pub fn write_default_if_missing(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let root = path
        .parent()
        .context("config path has no parent directory")?;
    for (relative, content) in BUNDLE {
        let target = root.join(relative);
        let parent = target
            .parent()
            .context("bundled config path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config directory `{}`", parent.display()))?;
        std::fs::write(&target, content)
            .with_context(|| format!("write default config `{}`", target.display()))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("desktop-rs-config-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn first_run_should_write_config_themes_and_examples() -> Result<()> {
        let root = temp_dir();
        let entry = root.join("config.nbcl");

        assert!(write_default_if_missing(&entry)?);
        assert!(entry.exists());
        assert!(root.join("themes/catppuccin-mocha.nbcl").exists());
        assert!(root.join("examples/full.nbcl").exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn existing_config_should_never_be_overwritten() -> Result<()> {
        let root = temp_dir();
        std::fs::create_dir_all(&root)?;
        let entry = root.join("config.nbcl");
        std::fs::write(&entry, "custom")?;

        assert!(!write_default_if_missing(&entry)?);
        assert_eq!(std::fs::read_to_string(&entry)?, "custom");
        assert!(!root.join("themes").exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
