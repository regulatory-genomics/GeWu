//! Plugins shipped with the GeWu distribution.

use std::path::PathBuf;

pub const MARKETPLACE_ID: &str = "gewu-marketplace";
const MARKETPLACE_VERSION: &str = "0.1.0";

const MANIFEST: &str = include_str!("../../resources/builtin-plugins/gewu-marketplace/plugin.json");
const ADAPTER: &str = include_str!("../../resources/builtin-plugins/gewu-marketplace/bin/marketplace");
const INDEX: &str = include_str!("../../resources/builtin-plugins/gewu-marketplace/web/index.html");

pub fn is_protected(id: &str) -> bool {
    id == MARKETPLACE_ID
}

/// A distribution has the plugin beside the executable; a cargo-built binary
/// materializes the embedded copy under the host state directory so neither
/// path depends on the build machine.
pub fn marketplace_root() -> PathBuf {
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join("../resources/builtin-plugins").join(MARKETPLACE_ID);
        if sibling.join("plugin.json").is_file() {
            return sibling;
        }
    }
    if let Some(home) = dirs::home_dir() {
        let directory = home
            .join(".gewu/builtin-plugins")
            .join(MARKETPLACE_ID)
            .join(MARKETPLACE_VERSION);
        if materialize(&directory).is_ok() {
            return directory;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources/builtin-plugins")
        .join(MARKETPLACE_ID)
}

fn materialize(directory: &PathBuf) -> std::io::Result<()> {
    if directory.join("plugin.json").is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(directory.join("bin"))?;
    std::fs::create_dir_all(directory.join("web"))?;
    std::fs::write(directory.join("plugin.json"), MANIFEST)?;
    std::fs::write(directory.join("web/index.html"), INDEX)?;
    let adapter = directory.join("bin/marketplace");
    std::fs::write(&adapter, ADAPTER)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
