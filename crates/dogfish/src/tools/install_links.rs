//! install-links — Creates BusyBox-style symlinks for all 40 utilities

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use anyhow::Result;
use clap::Parser;
use dogfish_core::DogfishContext;

#[derive(Parser)]
#[command(
    name = "install-links",
    about = "Create symlinks for all Dogfish utilities in TARGET_DIR"
)]
struct InstallLinksArgs {
    /// Directory to create symlinks in (e.g. ./bin or ~/.local/bin)
    target_dir: String,

    /// Optional prefix for symlink names (e.g. `df-` creates `df-ls`, `df-cat`)
    #[arg(long = "prefix", default_value = "")]
    prefix: String,
}

pub fn run_install_links(
    args: Vec<String>,
    tool_names: &[&str],
    ctx: &DogfishContext,
) -> Result<i32> {
    let parsed = InstallLinksArgs::try_parse_from(args)?;
    let target = Path::new(&parsed.target_dir);
    fs::create_dir_all(target)?;
    let exe = std::env::current_exe()?;
    let st = ctx.style();

    let mut count = 0;
    for &tool in tool_names {
        if matches!(
            tool,
            "install-links" | "init" | "bench" | "undo" | "history"
        ) {
            continue;
        }
        let link_name = format!("{}{}", parsed.prefix, tool);
        let link_path = target.join(&link_name);
        if link_path.exists() || link_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&link_path);
        }
        symlink(&exe, &link_path)?;
        count += 1;
    }

    ctx.telemetry.add_items(count);
    println!(
        "{} Installed {} utility symlinks in '{}' -> '{}'",
        st.bold_green("✔"),
        st.bold_cyan(&count.to_string()),
        target.display(),
        exe.display()
    );
    Ok(0)
}
