//! `agproc skills` — the agent-facing guide, specialised for this project.
//!
//! The guide is embedded in the binary (agproc installs as a single file, so it
//! cannot read `skill-data/` from an install directory) and gets a generated
//! section describing this project's services, so the agent sees the real
//! commands instead of a generic tutorial.

use anyhow::Result;
use serde::Serialize;

use crate::cli::Failure;
use crate::config::{Cmd, Config, LoadedConfig};
use crate::exit;
use crate::paths::Project;

const CORE: &str = include_str!("../../skill-data/core/SKILL.md");
const PROJECT_MARKER: &str = "<!-- agproc:project -->";

#[derive(Serialize)]
struct SkillDoc {
    name: String,
    content: String,
}

#[derive(Serialize)]
struct ServiceInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_cmd: Option<String>,
    run_cmd: String,
    probe_kind: String,
    probe_target: String,
    cwd: String,
    log_stdout: String,
    log_stderr: String,
}

#[derive(Serialize)]
struct Report {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<String>,
    services: Vec<ServiceInfo>,
    skill: SkillDoc,
}

pub struct Request {
    pub json: bool,
}

pub fn run(project: Option<&Project>, request: Request) -> Result<i32, Failure> {
    let core = load_core();
    let mut services = Vec::new();
    let mut root = None;
    let mut config_path = None;

    let section = match project {
        Some(project) => {
            root = Some(project.root.display().to_string());
            config_path = Some(project.config_path.display().to_string());
            match crate::config::load(&project.config_path) {
                Ok(loaded) => {
                    services = service_infos(project, &loaded.config);
                    project_section(project, &loaded)
                }
                Err(err) => format!(
                    "## This project\n\n\
                     `{}` could not be read, so agproc cannot describe the services here:\n\n\
                     ```\n{err:#}\n```\n\n\
                     Fix the file (or run `agproc init`) and call `agproc skills` again.\n",
                    project.config_path.display()
                ),
            }
        }
        None => "## This project\n\n\
                 No `agproc.toml` was found in this directory or any parent directory, so this is\n\
                 the generic guide. Run `agproc init` in the project root to create a template, then\n\
                 `agproc start` to bring the services up.\n"
            .to_string(),
    };

    let content = core.replace(PROJECT_MARKER, &section);

    if request.json {
        let report = Report {
            version: env!("CARGO_PKG_VERSION").to_string(),
            root,
            config: config_path,
            services,
            skill: SkillDoc {
                name: "core".to_string(),
                content,
            },
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{content}");
        if !content.ends_with('\n') {
            println!();
        }
    }
    Ok(exit::OK)
}

/// Development override: serve `$AGPROC_SKILLS_DIR/core/SKILL.md` from disk.
fn load_core() -> String {
    if let Ok(dir) = std::env::var("AGPROC_SKILLS_DIR") {
        let path = std::path::Path::new(&dir).join("core/SKILL.md");
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content;
        }
    }
    CORE.to_string()
}

fn service_infos(project: &Project, config: &Config) -> Vec<ServiceInfo> {
    config
        .services
        .iter()
        .map(|service| ServiceInfo {
            name: service.name.clone(),
            build_cmd: service.build_cmd.as_ref().map(Cmd::display),
            run_cmd: service.run_cmd.display(),
            probe_kind: service.target().kind_str().to_string(),
            probe_target: service.target().describe(),
            cwd: service.cwd_path(project).display().to_string(),
            log_stdout: project.log_stdout(&service.name).display().to_string(),
            log_stderr: project.log_stderr(&service.name).display().to_string(),
        })
        .collect()
}

fn project_section(project: &Project, loaded: &LoadedConfig) -> String {
    let mut out = String::new();
    out.push_str("## This project\n\n");
    out.push_str(&format!("- project root: `{}`\n", project.root.display()));
    out.push_str(&format!(
        "- config: `{}`\n",
        project.config_path.display()
    ));
    out.push_str(&format!("- agproc: `{}`\n\n", env!("CARGO_PKG_VERSION")));

    out.push_str("| service | build-cmd | run-cmd | probe | logs |\n");
    out.push_str("|---|---|---|---|---|\n");
    for service in &loaded.config.services {
        let build = service
            .build_cmd
            .as_ref()
            .map(|cmd| format!("`{}`", cmd.display()))
            .unwrap_or_else(|| "(none)".to_string());
        let target = service.target();
        let probe = match target {
            crate::config::ProbeTarget::None => "process stays alive".to_string(),
            _ => format!("`{}`", target.describe()),
        };
        out.push_str(&format!(
            "| `{}` | {} | `{}` | {} | `.agproc/logs/{}.stdout.log` |\n",
            service.name,
            build,
            service.run_cmd.display(),
            probe,
            service.name,
        ));
    }

    out.push('\n');
    out.push_str("Workflow for this project:\n\n");
    let names: Vec<&str> = loaded
        .config
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    if names.len() == 1 {
        out.push_str(
            "1. `agproc start` (safe to repeat: it reports `ALREADY RUNNING` instead of restarting).\n\
             2. after editing code: `agproc restart`.\n\
             3. if the exit code is not 0: `agproc logs --tail 80`.\n\
             4. `agproc stop` when you are done.\n",
        );
    } else {
        out.push_str(&format!(
            "1. `agproc start` starts all of {} in parallel; it is safe to repeat.\n\
             2. after editing code, restart only what changed: {}.\n\
             3. if the exit code is not 0: `agproc ps` then `agproc logs <service> --tail 80`.\n\
             4. `agproc stop` (or `agproc stop <service>`) when you are done.\n",
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", "),
            names
                .iter()
                .map(|name| format!("`agproc restart {name}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    let mut settings = String::new();
    if loaded.config.settings.log_max_bytes > 0 {
        settings.push_str(&format!(
            "- log files rotate into `<name>.1` past {} MiB\n",
            loaded.config.settings.log_max_bytes / (1024 * 1024)
        ));
    }
    if !loaded.config.settings.port_check {
        settings.push_str("- port ownership checks are disabled in `[settings] port-check = false`\n");
    }
    if !settings.is_empty() {
        out.push_str("\nNotes:\n");
        out.push_str(&settings);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(dir: &std::path::Path, config: &str) -> Project {
        let path = dir.join("agproc.toml");
        std::fs::write(&path, config).unwrap();
        Project {
            root: dir.to_path_buf(),
            config_path: path,
        }
    }

    #[test]
    fn project_section_lists_real_services() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "backend"
build-cmd = "cargo build"
run-cmd = "./target/debug/api"
probe = { http-get = { port = 3000, path = "/healthz" } }

[[service]]
name = "frontend"
run-cmd = "pnpm preview"
probe = { tcp-connect = { port = 5173 } }
"#,
        );
        let loaded = crate::config::load(&project.config_path).unwrap();
        let section = project_section(&project, &loaded);
        assert!(section.contains("`cargo build`"), "{section}");
        assert!(section.contains("`./target/debug/api`"), "{section}");
        assert!(section.contains("http://127.0.0.1:3000/healthz"), "{section}");
        assert!(section.contains("`agproc restart frontend`"), "{section}");
        assert!(section.contains(".agproc/logs/backend.stdout.log"), "{section}");
    }

    #[test]
    fn core_guide_has_the_marker_and_key_contract() {
        assert!(CORE.contains(PROJECT_MARKER), "marker missing from the guide");
        for needle in [
            "===== ALREADY RUNNING",
            "agproc restart",
            "--timeout-seconds",
            "Exit codes",
        ] {
            assert!(CORE.contains(needle), "guide is missing {needle}");
        }
    }

    #[test]
    fn json_report_carries_services_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            "[[service]]\nname = \"api\"\nrun-cmd = \"sleep 1\"\n",
        );
        let loaded = crate::config::load(&project.config_path).unwrap();
        let infos = service_infos(&project, &loaded.config);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "api");
        assert_eq!(infos[0].probe_kind, "none");
        assert!(infos[0].log_stdout.ends_with("api.stdout.log"));
    }
}
