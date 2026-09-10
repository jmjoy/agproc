//! `agproc init` — create an `agproc.toml` template.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::paths::CONFIG_FILE;

pub struct InitArgs {
    pub root: PathBuf,
    pub force: bool,
}

pub fn run(args: InitArgs) -> Result<i32> {
    let root = crate::paths::absolute(&args.root)?;
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    let config_path = root.join(CONFIG_FILE);

    if config_path.exists() && !args.force {
        anyhow::bail!(
            "{} already exists (use --force to overwrite)",
            config_path.display()
        );
    }

    let body = render(&root);
    std::fs::write(&config_path, body)
        .with_context(|| format!("cannot write {}", config_path.display()))?;

    let gitignore = ensure_gitignore(&root)?;
    println!("===== WROTE {} =====", config_path.display());
    if let Some(note) = gitignore {
        println!("===== {note} =====");
    }
    println!("Next: review the file, then run `agproc start` (agents: run `agproc skills`).");
    Ok(crate::exit::OK)
}

/// Append `.agproc/` to `.gitignore` when it is not ignored yet.
fn ensure_gitignore(root: &Path) -> Result<Option<String>> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let already = existing
        .lines()
        .map(str::trim)
        .any(|line| line == ".agproc/" || line == ".agproc");
    if already {
        return Ok(None);
    }
    let mut updated = existing.clone();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".agproc/\n");
    std::fs::write(&path, updated).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(format!("UPDATED {}", path.display())))
}

fn render(root: &Path) -> String {
    let mut out = String::new();
    out.push_str(
        "# agproc.toml — dev-time process manager for AI agents and humans.\n\
         # Agents: run `agproc skills` for the full, project-aware guide.\n\
         #\n\
         # start   : build (if needed) + run + wait for the probe; a no-op when already running\n\
         # restart : stop, then build + run + wait for the probe\n\
         # stop/ps/logs: manage and inspect what agproc started\n\n\
         [settings]\n\
         # shell = \"sh\"                      # shell for the string form of build-cmd / run-cmd\n\
         # stop-timeout-seconds = 10         # SIGTERM -> SIGKILL grace period\n\
         # log-max-bytes = 33554432          # rotate .agproc/logs/*.log past 32 MiB (0 disables)\n\
         # port-check = true                 # warn about foreign listeners on the probe port\n\n",
    );

    let rust = cargo_target(root);
    let node = node_scripts(root);

    match &rust {
        Some(name) => out.push_str(&format!(
            "[[service]]\n\
             name = \"backend\"\n\
             build-cmd = \"cargo build\"\n\
             run-cmd = \"./target/debug/{name}\"\n\
             probe = {{\n\
             \x20 http-get = {{ scheme = \"http\", host = \"127.0.0.1\", port = 3000, path = \"/healthz\" }},\n\
             \x20 initial-delay-seconds = 1,\n\
             \x20 period-seconds = 1,\n\
             \x20 timeout-seconds = 2,\n\
             \x20 failure-threshold = 3,\n\
             }}\n\n"
        )),
        None => out.push_str(
            "# [[service]]\n\
             # name = \"backend\"\n\
             # build-cmd = \"cargo build\"\n\
             # run-cmd = \"./target/debug/my-backend\"\n\
             # probe = { http-get = { port = 3000, path = \"/healthz\" },\n\
             #                     initial-delay-seconds = 1, period-seconds = 1,\n\
             #                     timeout-seconds = 2, failure-threshold = 3 }\n\n",
        ),
    }

    let mut frontend = String::from("# [[service]]\n# name = \"frontend\"\n");
    if let Some(scripts) = &node {
        if scripts.iter().any(|s| s == "build") {
            frontend.push_str("# build-cmd = \"pnpm build\"\n");
        }
        frontend.push_str(
            "# # run-cmd should NOT watch files: agproc restarts it explicitly, so a\n\
             # # non-watch server (e.g. `vite preview`) keeps restarts meaningful.\n",
        );
        if scripts.iter().any(|s| s == "preview") {
            frontend.push_str("# run-cmd = \"pnpm preview --port 5173\"\n");
        } else if scripts.iter().any(|s| s == "dev") {
            frontend.push_str("# run-cmd = \"pnpm dev --port 5173\"   # only if it does not watch files\n");
        } else {
            frontend.push_str("# run-cmd = \"pnpm start\"\n");
        }
    } else {
        frontend.push_str(
            "# build-cmd = \"pnpm build\"\n\
             # run-cmd = \"pnpm preview --port 5173\"\n",
        );
    }
    frontend.push_str(
        "# probe = { tcp-connect = { host = \"127.0.0.1\", port = 5173 },\n\
         #                     initial-delay-seconds = 1, period-seconds = 1,\n\
         #                     timeout-seconds = 2, failure-threshold = 3 }\n",
    );
    out.push_str(&frontend);
    out
}

/// Binary name from `Cargo.toml`, when this looks like a Rust project.
fn cargo_target(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let value: toml::Value = toml::from_str(&raw).ok()?;
    if let Some(name) = value
        .get("bin")
        .and_then(|b| b.as_array())
        .and_then(|bins| bins.first())
        .and_then(|bin| bin.get("name"))
        .and_then(|name| name.as_str())
    {
        return Some(name.to_string());
    }
    value
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// Script names from `package.json`, when this looks like a Node project.
fn node_scripts(root: &Path) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(root.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let scripts = value.get("scripts")?.as_object()?;
    Some(scripts.keys().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_config_and_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("package.json"), r#"{"scripts":{"build":"x","preview":"y"}}"#)
            .unwrap();

        let args = InitArgs {
            root: root.clone(),
            force: false,
        };
        assert_eq!(run(args).unwrap(), crate::exit::OK);

        let config = std::fs::read_to_string(root.join("agproc.toml")).unwrap();
        assert!(config.contains("run-cmd = \"./target/debug/demo-app\""), "{config}");
        assert!(config.contains("pnpm preview --port 5173"), "{config}");
        assert!(config.contains("tcp-connect"), "{config}");

        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains(".agproc/"), "{gitignore}");

        // Second run without --force must refuse, and the file must stay intact.
        let err = run(InitArgs {
            root: root.clone(),
            force: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        // --force rewrites; .gitignore must not get a duplicate entry.
        run(InitArgs {
            root: root.clone(),
            force: true,
        })
        .unwrap();
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(gitignore.matches(".agproc/").count(), 1, "{gitignore}");
    }

    #[test]
    fn init_without_project_files_emits_comments_only() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            root: dir.path().to_path_buf(),
            force: false,
        })
        .unwrap();
        let config = std::fs::read_to_string(dir.path().join("agproc.toml")).unwrap();
        assert!(config.contains("# [[service]]"), "{config}");
        assert!(!config.contains("\n[[service]]"), "{config}");
    }
}
