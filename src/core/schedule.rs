use crate::{
    cli::ScheduleCommand,
    core::config::{Config, ScheduleConfig, workspace_config_dir_for},
};
use anyhow::{Context, Result, bail};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const MANAGED_HEADER_PREFIX: &str = "# Managed by archon schedule sync";
const UNIT_PREFIX: &str = "archon-sched-";

pub fn handle_command(cmd: ScheduleCommand, config: &Config) -> Result<()> {
    match cmd {
        ScheduleCommand::Sync { global } => sync_schedules(config, global),
        ScheduleCommand::List { global } => list_schedules(config, global),
        ScheduleCommand::Run { name, global } => run_schedule(config, &name, global),
        ScheduleCommand::Logs {
            name,
            lines,
            global,
        } => logs_schedule(config, &name, lines, global),
        ScheduleCommand::Clean { global } => clean_schedules(config, global),
    }
}

fn systemd_user_dir() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("Could not determine user config directory")?;
    Ok(base.join("systemd").join("user"))
}

fn sanitize_unit_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

fn resolve_scope(config: &Config, global: bool) -> Result<(String, PathBuf)> {
    if global {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        return Ok(("global".to_string(), home));
    }

    let ws_path = config
        .workspace
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canon = dunce::canonicalize(&ws_path).unwrap_or(ws_path);
    let ws_cfg_dir = workspace_config_dir_for(&canon);
    let raw_name = ws_cfg_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    Ok((sanitize_unit_component(raw_name), canon))
}

fn unit_base_name(scope: &str, name: &str) -> String {
    format!(
        "{UNIT_PREFIX}{}-{}",
        sanitize_unit_component(scope),
        sanitize_unit_component(name)
    )
}

fn scope_unit_prefix(scope: &str) -> String {
    format!("{UNIT_PREFIX}{}-", sanitize_unit_component(scope))
}

fn quote_systemd_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for ch in arg.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn validate_on_calendar(expr: &str) -> Result<()> {
    let output = match Command::new("systemd-analyze")
        .args(["calendar", expr])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("Failed to execute systemd-analyze"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("Invalid OnCalendar expression '{expr}': {detail}");
    }
    Ok(())
}

fn render_service(
    sched: &ScheduleConfig,
    scope: &str,
    working_dir: &Path,
    exe_path: &Path,
) -> String {
    let mut args = vec![quote_systemd_arg(&exe_path.to_string_lossy())];

    if let Some(ref skill) = sched.skill
        && !skill.trim().is_empty()
    {
        args.push("--skill".to_string());
        args.push(quote_systemd_arg(skill.trim()));
    }
    if let Some(ref model) = sched.model
        && !model.trim().is_empty()
    {
        args.push("--model".to_string());
        args.push(quote_systemd_arg(model.trim()));
    }
    if let Some(ref sys) = sched.system_prompt
        && !sys.trim().is_empty()
    {
        args.push("--system".to_string());
        args.push(quote_systemd_arg(sys.trim()));
    }
    if sched.auto_approve {
        args.push("-y".to_string());
    }
    if sched.no_tools {
        args.push("--no-tools".to_string());
    }
    if !sched.save_session {
        args.push("--no-save".to_string());
    }
    args.push("-p".to_string());
    args.push(quote_systemd_arg(sched.prompt.trim()));

    let exec_start = args.join(" ");
    let wd = working_dir.to_string_lossy();

    format!(
        "{MANAGED_HEADER_PREFIX} (scope={scope}, workspace={wd})\n\
         [Unit]\n\
         Description=Archon schedule: {scope}/{name}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         WorkingDirectory={wd}\n\
         ExecStart={exec_start}\n",
        name = sched.name.trim(),
    )
}

fn render_timer(
    sched: &ScheduleConfig,
    scope: &str,
    working_dir: &Path,
    service_unit: &str,
) -> String {
    let wd = working_dir.to_string_lossy();
    let persistent = if sched.persistent { "true" } else { "false" };
    format!(
        "{MANAGED_HEADER_PREFIX} (scope={scope}, workspace={wd})\n\
         [Unit]\n\
         Description=Timer for Archon schedule: {scope}/{name}\n\
         \n\
         [Timer]\n\
         OnCalendar={cal}\n\
         Persistent={persistent}\n\
         Unit={service_unit}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        name = sched.name.trim(),
        cal = sched.on_calendar.trim(),
    )
}

fn is_managed_file(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|content| content.starts_with(MANAGED_HEADER_PREFIX))
        .unwrap_or(false)
}

fn run_systemctl(args: &[&str], allow_failure: bool) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute `systemctl --user {}`", args.join(" ")))?;

    if !output.status.success() && !allow_failure {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "`systemctl --user {}` failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }
    Ok(())
}

fn schedules_for_scope(config: &Config, global: bool) -> &[ScheduleConfig] {
    if global {
        &config.global_schedules
    } else {
        &config.schedules
    }
}

fn sync_schedules(config: &Config, global: bool) -> Result<()> {
    let (scope, working_dir) = resolve_scope(config, global)?;
    let schedules = schedules_for_scope(config, global);
    let unit_dir = systemd_user_dir()?;
    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("Failed to create {}", unit_dir.display()))?;

    let exe_path = env::current_exe().context("Failed to resolve current archon executable path")?;
    let exe_path = dunce::canonicalize(&exe_path).unwrap_or(exe_path);

    let mut desired_bases = HashSet::new();
    for sched in schedules {
        if sched.name.trim().is_empty() {
            bail!("Found a [[schedules]] entry with an empty `name`.");
        }
        if sched.on_calendar.trim().is_empty() {
            bail!(
                "Schedule '{}' is missing `on_calendar`.",
                sched.name.trim()
            );
        }
        if sched.prompt.trim().is_empty() {
            bail!("Schedule '{}' is missing `prompt`.", sched.name.trim());
        }
        validate_on_calendar(sched.on_calendar.trim())?;

        let base = unit_base_name(&scope, &sched.name);
        if !desired_bases.insert(base.clone()) {
            bail!(
                "Duplicate schedule name '{}' (collides on unit '{base}').",
                sched.name.trim()
            );
        }
    }

    // Prune stale managed units for this scope
    let prefix = scope_unit_prefix(&scope);
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&unit_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !file_name.starts_with(&prefix) || !is_managed_file(&path) {
                continue;
            }
            let base = file_name
                .strip_suffix(".timer")
                .or_else(|| file_name.strip_suffix(".service"));
            let Some(base) = base else {
                continue;
            };
            if !desired_bases.contains(base) {
                if file_name.ends_with(".timer") {
                    let _ = run_systemctl(&["disable", "--now", file_name], true);
                }
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }

    // Write desired service & timer units
    for sched in schedules {
        let base = unit_base_name(&scope, &sched.name);
        let service_name = format!("{base}.service");
        let timer_name = format!("{base}.timer");

        let service_content = render_service(sched, &scope, &working_dir, &exe_path);
        let timer_content = render_timer(sched, &scope, &working_dir, &service_name);

        fs::write(unit_dir.join(&service_name), service_content)
            .with_context(|| format!("Failed to write {service_name}"))?;
        fs::write(unit_dir.join(&timer_name), timer_content)
            .with_context(|| format!("Failed to write {timer_name}"))?;
    }

    run_systemctl(&["daemon-reload"], false)?;

    let mut enabled_count = 0usize;
    let mut disabled_count = 0usize;
    for sched in schedules {
        let base = unit_base_name(&scope, &sched.name);
        let timer_name = format!("{base}.timer");
        if sched.enabled {
            run_systemctl(&["enable", "--now", &timer_name], false)?;
            enabled_count += 1;
        } else {
            run_systemctl(&["disable", "--now", &timer_name], true)?;
            disabled_count += 1;
        }
    }

    println!(
        "Synced schedules for scope '{scope}': {enabled_count} enabled, {disabled_count} disabled, {removed} stale unit file(s) removed."
    );
    Ok(())
}

fn list_schedules(config: &Config, global: bool) -> Result<()> {
    let (scope, _) = resolve_scope(config, global)?;
    let schedules = schedules_for_scope(config, global);

    if schedules.is_empty() {
        println!("No schedules configured for scope '{scope}'.");
    } else {
        println!("Configured schedules (scope: {scope}):");
        for sched in schedules {
            let state = if sched.enabled { "enabled" } else { "disabled" };
            let skill = sched.skill.as_deref().unwrap_or("-");
            let save = if sched.save_session { "save" } else { "no-save" };
            println!(
                "  - {} [{}] | on_calendar: {} | skill: {} | {}",
                sched.name, state, sched.on_calendar, skill, save
            );
        }
    }

    println!("\nActive systemd user timers:");
    let pattern = format!("{}*", scope_unit_prefix(&scope));
    let status = Command::new("systemctl")
        .args(["--user", "list-timers", "--all", &pattern, "--no-pager"])
        .status()
        .context("Failed to execute `systemctl --user list-timers`")?;
    if !status.success() {
        bail!("`systemctl --user list-timers` exited with status {status}");
    }
    Ok(())
}

fn run_schedule(config: &Config, name: &str, global: bool) -> Result<()> {
    let (scope, _) = resolve_scope(config, global)?;
    let service_name = format!("{}.service", unit_base_name(&scope, name));
    println!("Starting {service_name}...");
    run_systemctl(&["start", &service_name], false)?;
    println!("Completed {service_name}.");
    Ok(())
}

fn logs_schedule(config: &Config, name: &str, lines: usize, global: bool) -> Result<()> {
    let (scope, _) = resolve_scope(config, global)?;
    let service_name = format!("{}.service", unit_base_name(&scope, name));
    let lines_str = lines.to_string();
    let status = Command::new("journalctl")
        .args(["--user", "-u", &service_name, "-n", &lines_str, "--no-pager"])
        .status()
        .context("Failed to execute `journalctl --user`")?;
    if !status.success() {
        bail!("`journalctl --user` exited with status {status}");
    }
    Ok(())
}

fn clean_schedules(config: &Config, global: bool) -> Result<()> {
    let (scope, _) = resolve_scope(config, global)?;
    let unit_dir = systemd_user_dir()?;
    if !unit_dir.exists() {
        println!("No systemd user unit directory found; nothing to clean.");
        return Ok(());
    }

    let prefix = scope_unit_prefix(&scope);
    let mut removed = 0usize;
    for entry in fs::read_dir(&unit_dir)?.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !file_name.starts_with(&prefix) || !is_managed_file(&path) {
            continue;
        }
        if file_name.ends_with(".timer") {
            let _ = run_systemctl(&["disable", "--now", file_name], true);
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }

    if removed > 0 {
        run_systemctl(&["daemon-reload"], false)?;
    }
    println!("Cleaned {removed} managed unit file(s) for scope '{scope}'.");
    Ok(())
}
