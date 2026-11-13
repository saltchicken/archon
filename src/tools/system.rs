use super::{AsyncTool, ToolContext};
use crate::core::config::expand_tilde;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{collections::HashMap, fmt::Write as _, fs, path::Path};

pub struct Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemAction {
    /// CachyOS hardware, kernel, sched-ext (`scx`), CPU governor/EPP, PSI resource pressure, ZRAM/memory, and thermal overview.
    Overview,
    /// Query systemd journal logs (`journalctl --no-pager`) with filtering by unit, priority, boot, time, or regex.
    Journal,
    /// Inspect systemd services, timers, failed units, or boot performance (`systemctl` and `systemd-analyze`).
    Services,
    /// Inspect top resource-consuming processes (`ps`) sorted by CPU or memory, alongside PSI stall metrics.
    Processes,
    /// Analyze Fish shell history (`~/.local/share/fish/fish_history`), frequent command patterns, abbreviations, and functions.
    Fish,
    /// Query Hyprland compositor state (`hyprctl`: monitors, workspaces, clients, binds, devices, configerrors, getoption).
    Hyprland,
    /// Inspect Btrfs filesystems, mount optimization flags (`zstd`, `noatime`, `discard=async`), disk usage, and subvolumes.
    Btrfs,
    /// Query CachyOS / Arch packages via `paru` / `pacman` (stats, orphans, foreign/AUR, explicit, info, modified configs, pending updates).
    Packages,
}

#[derive(Deserialize, JsonSchema)]
pub struct SystemArgs {
    /// The system subsystem or diagnostic action to run.
    pub action: SystemAction,
    /// Sub-query mode depending on `action`:
    /// - `services`: `"failed"` (default), `"status"`, `"timers"`, `"blame"`, `"critical_chain"`
    /// - `processes`: `"cpu"` (default) or `"mem"`
    /// - `fish`: `"recent"` (default), `"frequent"`, `"abbreviations"`
    /// - `hyprland`: `"info"` (default), `"clients"`, `"binds"`, `"configerrors"`, `"devices"`, `"getoption"`
    /// - `btrfs`: `"usage"` (default), `"mounts"`, `"subvolumes"`
    /// - `packages`: `"stats"` (default), `"orphans"`, `"foreign"`, `"explicit"`, `"search"`, `"info"`, `"files"`, `"owns"`, `"modified"`, `"updates"`
    #[serde(default)]
    pub query: String,
    /// Optional target argument (e.g., systemd unit name, package name/file path, Hyprland option name like `"decoration:blur:enabled"`, Btrfs mountpoint like `"/"`, or regex filter for `fish` / `journal`).
    #[serde(default)]
    pub target: String,
    /// When `action` is `journal` or `services`, query the user session (`--user`) instead of system-wide units.
    #[serde(default)]
    pub user: bool,
    /// When `action` is `journal`, filter by syslog priority (e.g. `"err"`, `"warning"`, `"notice"`, `"info"`, or `"0..3"`).
    #[serde(default)]
    pub priority: String,
    /// When `action` is `journal`, boot offset (`0` for current boot, `-1` for previous boot). Defaults to `0`.
    #[serde(default)]
    pub boot: Option<i32>,
    /// When `action` is `journal`, time filter passed to `--since` (e.g. `"1 hour ago"`, `"today"`, `"2025-02-01 12:00:00"`).
    #[serde(default)]
    pub since: String,
    /// When `action` is `journal`, show only kernel (`dmesg` / `-k`) messages.
    #[serde(default)]
    pub kernel: bool,
    /// Maximum number of lines/entries to return (default: 50–100 depending on action, max: 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

const SENSITIVE_CMD_PATTERNS: &[&str] = &[
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "token=",
    "_token",
    "bearer ",
    "private_key",
    "aws_secret",
    "ghp_",
    "sk-",
    "ai_key",
];

fn validate_safe_arg<'a>(raw: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("'{label}' is required for this query");
    }
    if trimmed.starts_with('-') {
        bail!("Security violation: '{label}' cannot start with '-': '{trimmed}'");
    }
    if trimmed.contains(['\0', '\n', '\r']) {
        bail!("Invalid control characters in '{label}'");
    }
    Ok(trimmed)
}

fn read_sys_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn run_overview(ctx: &ToolContext) -> Result<String> {
    let mut out = String::from("=== CachyOS System Overview ===\n");

    // 1. OS & Kernel
    if let Ok(os_rel) = fs::read_to_string("/etc/os-release") {
        for line in os_rel.lines() {
            if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                let _ = writeln!(out, "OS: {}", val.trim_matches('"'));
            }
        }
    }
    if let Ok(uname) = ctx.run_command("uname", ["-srm"], "").await {
        let _ = writeln!(out, "Kernel: {}", uname.trim());
    }
    if let Some(load) = read_sys_trimmed("/proc/loadavg") {
        let _ = writeln!(out, "Load Average: {load}");
    }

    // 2. CPU & CachyOS sched-ext (scx) status
    out.push_str("\n--- CPU & Scheduler ---\n");
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = cpuinfo
            .lines()
            .find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')))
    {
        let _ = writeln!(out, "CPU: {}", model.1.trim());
    }
    let scx_state = read_sys_trimmed("/sys/kernel/sched_ext/state")
        .unwrap_or_else(|| "disabled/not loaded".into());
    let scx_ops =
        read_sys_trimmed("/sys/kernel/sched_ext/root/ops").unwrap_or_else(|| "none".into());
    let _ = writeln!(
        out,
        "sched-ext (scx): state={scx_state}, active_scheduler={scx_ops}"
    );

    if let Some(gov) = read_sys_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") {
        let epp =
            read_sys_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference")
                .unwrap_or_else(|| "n/a".into());
        let driver = read_sys_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver")
            .unwrap_or_else(|| "n/a".into());
        let _ = writeln!(out, "CPU Freq: driver={driver}, governor={gov}, epp={epp}");
    }

    // 3. PSI (Pressure Stall Information)
    out.push_str("\n--- Resource Pressure (PSI) ---\n");
    for resource in ["cpu", "memory", "io"] {
        if let Some(psi) = read_sys_trimmed(format!("/proc/pressure/{resource}")) {
            for line in psi.lines() {
                let _ = writeln!(out, "{resource:>6}: {line}");
            }
        }
    }

    // 4. Memory & ZRAM
    out.push_str("\n--- Memory & ZRAM ---\n");
    if let Ok(free_out) = ctx.run_command("free", ["-h"], "").await {
        out.push_str(free_out.trim());
        out.push('\n');
    }
    if let Ok(zram_out) = ctx.run_command("zramctl", Vec::<&str>::new(), "").await
        && !zram_out.trim().is_empty()
    {
        out.push_str("ZRAM:\n");
        out.push_str(zram_out.trim());
        out.push('\n');
    }

    // 5. Thermals
    out.push_str("\n--- Thermals ---\n");
    let mut found_thermals = false;
    if let Ok(entries) = fs::read_dir("/sys/class/thermal") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let (Some(t_type), Some(temp_raw)) = (
                read_sys_trimmed(path.join("type")),
                read_sys_trimmed(path.join("temp")),
            ) && let Ok(millideg) = temp_raw.parse::<f64>()
            {
                let _ = writeln!(out, "  {t_type}: {:.1}°C", millideg / 1000.0);
                found_thermals = true;
            }
        }
    }
    if !found_thermals {
        out.push_str("  (No thermal zones exposed in /sys/class/thermal)\n");
    }

    Ok(out.trim_end().to_string())
}

async fn run_journal(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let lines = args.limit.unwrap_or(100).clamp(1, 500);
    let mut cmd_args = vec![
        "--no-pager".to_string(),
        "-n".to_string(),
        lines.to_string(),
    ];

    if args.user {
        cmd_args.push("--user".into());
    }
    if args.kernel {
        cmd_args.push("-k".into());
    }

    let boot_offset = args.boot.unwrap_or(0);
    cmd_args.push(format!("-b={boot_offset}"));

    if !args.priority.trim().is_empty() {
        let prio = validate_safe_arg(&args.priority, "priority")?;
        cmd_args.extend(["-p".into(), prio.into()]);
    }
    if !args.since.trim().is_empty() {
        let since = validate_safe_arg(&args.since, "since")?;
        cmd_args.extend(["--since".into(), since.into()]);
    }
    if !args.target.trim().is_empty() {
        let target = validate_safe_arg(&args.target, "target")?;
        if target.starts_with('/') || target.contains('*') {
            cmd_args.extend(["-g".into(), target.trim_matches('/').into()]);
        } else if args.user {
            cmd_args.extend(["--user-unit".into(), target.into()]);
        } else {
            cmd_args.extend(["-u".into(), target.into()]);
        }
    }
    if !args.query.trim().is_empty() {
        let grep = validate_safe_arg(&args.query, "query (grep pattern)")?;
        cmd_args.extend(["-g".into(), grep.into()]);
    }

    ctx.run_command(
        "journalctl",
        &cmd_args,
        "No matching journal entries found.",
    )
    .await
}

async fn run_services(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let query = match args.query.trim() {
        "" => "failed",
        q => q,
    };

    match query {
        "failed" => {
            let sys = ctx
                .run_command(
                    "systemctl",
                    ["--no-pager", "--failed"],
                    "No failed system units.",
                )
                .await?;
            let usr = ctx
                .run_command(
                    "systemctl",
                    ["--no-pager", "--user", "--failed"],
                    "No failed user units.",
                )
                .await
                .unwrap_or_default();
            Ok(format!(
                "=== System Failed Units ===\n{}\n\n=== User Failed Units ===\n{}",
                sys.trim(),
                usr.trim()
            ))
        }
        "status" => {
            let unit = validate_safe_arg(&args.target, "target (unit name)")?;
            let mut cmd_args = vec!["--no-pager", "-n", "40", "status"];
            if args.user {
                cmd_args.insert(1, "--user");
            }
            cmd_args.push(unit);
            ctx.run_command("systemctl", &cmd_args, "No status output.")
                .await
        }
        "timers" => {
            let mut cmd_args = vec!["--no-pager", "list-timers", "--all"];
            if args.user {
                cmd_args.insert(1, "--user");
            }
            ctx.run_command("systemctl", &cmd_args, "No timers found.")
                .await
        }
        "blame" => {
            let limit = args.limit.unwrap_or(30).clamp(5, 100);
            let time_out = ctx
                .run_command("systemd-analyze", ["time"], "")
                .await
                .unwrap_or_default();
            let blame_out = ctx
                .run_command("systemd-analyze", ["--no-pager", "blame"], "")
                .await?;
            let top_blame: String = blame_out.lines().take(limit).collect::<Vec<_>>().join("\n");
            Ok(format!(
                "=== Boot Time ===\n{}\n\n=== Top {limit} Slowest Units (systemd-analyze blame) ===\n{top_blame}",
                time_out.trim()
            ))
        }
        "critical_chain" => {
            let mut cmd_args = vec!["--no-pager", "critical-chain"];
            if !args.target.trim().is_empty() {
                cmd_args.push(validate_safe_arg(&args.target, "target")?);
            }
            ctx.run_command("systemd-analyze", &cmd_args, "No critical-chain output.")
                .await
        }
        other => bail!(
            "Unknown services query '{other}'. Expected 'failed', 'status', 'timers', 'blame', or 'critical_chain'."
        ),
    }
}

async fn run_processes(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let limit = args.limit.unwrap_or(25).clamp(5, 100);
    let sort_flag = match args.query.trim() {
        "" | "cpu" => "-%cpu",
        "mem" | "memory" => "-%mem",
        other => bail!("Unknown processes query '{other}'. Expected 'cpu' or 'mem'."),
    };

    let ps_out = ctx
        .run_command(
            "ps",
            [
                "-eo",
                "pid,ppid,user,%cpu,%mem,rss,stat,comm,args",
                &format!("--sort={sort_flag}"),
            ],
            "No process output.",
        )
        .await?;

    let lines: Vec<&str> = ps_out.lines().take(limit + 1).collect();
    Ok(format!(
        "=== Top {limit} Processes (sorted by {sort_flag}) ===\n{}",
        lines.join("\n")
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FishHistoryEntry {
    pub cmd: String,
    pub when: Option<i64>,
}

pub(super) fn is_sensitive_command(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    SENSITIVE_CMD_PATTERNS.iter().any(|pat| lower.contains(pat))
}

pub(super) fn parse_fish_history_str(content: &str) -> Vec<FishHistoryEntry> {
    let mut entries = Vec::new();
    let mut current_cmd: Option<String> = None;
    let mut current_when: Option<i64> = None;

    let flush =
        |entries: &mut Vec<FishHistoryEntry>, cmd: &mut Option<String>, when: &mut Option<i64>| {
            if let Some(c) = cmd.take()
                && !is_sensitive_command(&c)
            {
                entries.push(FishHistoryEntry {
                    cmd: c,
                    when: when.take(),
                });
            }
        };

    for line in content.lines() {
        if let Some(cmd_raw) = line.strip_prefix("- cmd: ") {
            flush(&mut entries, &mut current_cmd, &mut current_when);
            current_cmd = Some(cmd_raw.replace("\\n", "\n").replace("\\\\", "\\"));
            current_when = None;
        } else if let Some(when_raw) = line.strip_prefix("  when: ") {
            current_when = when_raw.trim().parse::<i64>().ok();
        }
    }
    flush(&mut entries, &mut current_cmd, &mut current_when);
    entries
}

fn load_fish_history() -> Result<Vec<FishHistoryEntry>> {
    let path = dirs::data_local_dir()
        .map(|d| d.join("fish/fish_history"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| expand_tilde("~/.local/share/fish/fish_history"));
    if !path.exists() {
        bail!("Fish history file not found at {}", path.display());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(parse_fish_history_str(&content))
}

async fn run_fish(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let query = match args.query.trim() {
        "" => "recent",
        q => q,
    };

    match query {
        "recent" => {
            let entries = load_fish_history()?;
            let limit = args.limit.unwrap_or(50).clamp(1, 300);

            let matcher = (!args.target.trim().is_empty())
                .then(|| {
                    RegexBuilder::new(args.target.trim())
                        .case_insensitive(true)
                        .build()
                        .with_context(|| format!("Invalid regex in target: '{}'", args.target))
                })
                .transpose()?;

            let filtered: Vec<&FishHistoryEntry> = entries
                .iter()
                .rev()
                .filter(|e| matcher.as_ref().is_none_or(|m| m.is_match(&e.cmd)))
                .take(limit)
                .collect();

            if filtered.is_empty() {
                return Ok("No matching Fish history entries found.".into());
            }

            let mut out = format!(
                "=== Fish Command History (showing {} most recent of {} sanitized entries) ===\n",
                filtered.len(),
                entries.len()
            );
            for entry in filtered.into_iter().rev() {
                let ts = entry
                    .when
                    .and_then(|w| Local.timestamp_opt(w, 0).single())
                    .map(|dt: DateTime<Local>| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "unknown-time".into());
                let one_line = entry.cmd.replace('\n', " ⏎ ");
                let _ = writeln!(out, "[{ts}] {one_line}");
            }
            Ok(out.trim_end().to_string())
        }
        "frequent" => {
            let entries = load_fish_history()?;
            let limit = args.limit.unwrap_or(30).clamp(5, 100);

            let mut full_counts: HashMap<&str, usize> = HashMap::new();
            let mut bin_counts: HashMap<&str, usize> = HashMap::new();

            for e in &entries {
                let trimmed = e.cmd.trim();
                if trimmed.is_empty() {
                    continue;
                }
                *full_counts.entry(trimmed).or_insert(0) += 1;
                if let Some(first_word) = trimmed.split_whitespace().next() {
                    *bin_counts.entry(first_word).or_insert(0) += 1;
                }
            }

            let mut top_full: Vec<(&str, usize)> = full_counts
                .into_iter()
                .filter(|&(cmd, count)| count > 1 && cmd.len() >= 4)
                .collect();
            top_full.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

            let mut top_bins: Vec<(&str, usize)> = bin_counts.into_iter().collect();
            top_bins.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

            let mut out = format!(
                "=== Fish History Frequency Analysis ({} total commands) ===\n\n--- Top Executables ---\n",
                entries.len()
            );
            for (bin, count) in top_bins.into_iter().take(15) {
                let _ = writeln!(out, "  {count:>5}x  {bin}");
            }

            let _ = writeln!(
                out,
                "\n--- Top Repeated Full Commands (Abbreviation/Function Candidates) ---"
            );
            for (cmd, count) in top_full.into_iter().take(limit) {
                let one_line = cmd.replace('\n', " ⏎ ");
                let _ = writeln!(out, "  {count:>5}x  {one_line}");
            }
            Ok(out.trim_end().to_string())
        }
        "abbreviations" => {
            let abbrs = ctx
                .run_command(
                    "fish",
                    ["-c", "abbr --show"],
                    "(No Fish abbreviations defined)",
                )
                .await
                .unwrap_or_else(|e| format!("Failed to run fish: {e}"));
            let funcs_dir = dirs::config_dir()
                .unwrap_or_else(|| expand_tilde("~/.config"))
                .join("fish/functions");
            let mut funcs = Vec::new();
            if let Ok(rd) = fs::read_dir(&funcs_dir) {
                for entry in rd.flatten() {
                    funcs.push(entry.file_name().to_string_lossy().into_owned());
                }
                funcs.sort();
            }
            Ok(format!(
                "=== Fish Abbreviations (`abbr --show`) ===\n{}\n\n=== Custom Functions in ~/.config/fish/functions ===\n{}",
                abbrs.trim(),
                if funcs.is_empty() {
                    "(None)".into()
                } else {
                    funcs.join(", ")
                }
            ))
        }
        other => bail!(
            "Unknown fish query '{other}'. Expected 'recent', 'frequent', or 'abbreviations'."
        ),
    }
}

async fn run_hyprland(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let query = match args.query.trim() {
        "" => "info",
        q => q,
    };

    match query {
        "info" => {
            let mut out = String::new();
            for sub in [
                "version",
                "monitors",
                "workspaces",
                "activewindow",
                "configerrors",
            ] {
                let res = ctx.run_command("hyprctl", [sub], "(empty)").await?;
                let _ = writeln!(out, "=== hyprctl {sub} ===\n{}\n", res.trim());
            }
            Ok(out.trim_end().to_string())
        }
        "clients" | "binds" | "configerrors" | "devices" | "layers" | "splash" => {
            ctx.run_command("hyprctl", [query], "No output from hyprctl.")
                .await
        }
        "getoption" => {
            let opt = validate_safe_arg(&args.target, "target (Hyprland option name)")?;
            ctx.run_command("hyprctl", ["getoption", opt], "No output from hyprctl.")
                .await
        }
        other => bail!(
            "Unknown hyprland query '{other}'. Expected 'info', 'clients', 'binds', 'configerrors', 'devices', 'layers', or 'getoption'."
        ),
    }
}

async fn run_btrfs(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let query = match args.query.trim() {
        "" => "usage",
        q => q,
    };
    let mount_target = if args.target.trim().is_empty() {
        "/"
    } else {
        validate_safe_arg(&args.target, "target (mountpoint)")?
    };

    match query {
        "usage" => {
            let mounts = ctx
                .run_command("findmnt", ["-t", "btrfs"], "No Btrfs mounts found.")
                .await
                .unwrap_or_default();
            let df_out = ctx
                .run_command("df", ["-hT", mount_target], "")
                .await
                .unwrap_or_default();
            let btrfs_df = ctx
                .run_command("btrfs", ["filesystem", "df", mount_target], "")
                .await
                .unwrap_or_default();
            let btrfs_usage = ctx
                .run_command("btrfs", ["filesystem", "usage", mount_target], "")
                .await
                .unwrap_or_default();

            Ok(format!(
                "=== Btrfs Mounts (`findmnt -t btrfs`) ===\n{}\n\n=== Disk Usage (`df -hT {mount_target}`) ===\n{}\n\n=== Btrfs Allocation (`btrfs filesystem df {mount_target}`) ===\n{}\n\n=== Btrfs Usage (`btrfs filesystem usage {mount_target}`) ===\n{}",
                mounts.trim(),
                df_out.trim(),
                btrfs_df.trim(),
                btrfs_usage.trim()
            ))
        }
        "mounts" => {
            let findmnt = ctx
                .run_command(
                    "findmnt",
                    ["-t", "btrfs", "-o", "TARGET,SOURCE,FSTYPE,OPTIONS"],
                    "No Btrfs mounts found.",
                )
                .await?;
            let fstab = fs::read_to_string("/etc/fstab").unwrap_or_default();
            Ok(format!(
                "=== Active Btrfs Mount Options ===\n{}\n\n=== /etc/fstab ===\n{}",
                findmnt.trim(),
                fstab.trim()
            ))
        }
        "subvolumes" => {
            let subvols = ctx
                .run_command("btrfs", ["subvolume", "list", mount_target], "")
                .await;
            let findmnt = ctx
                .run_command(
                    "findmnt",
                    ["-t", "btrfs", "-o", "TARGET,SOURCE,OPTIONS"],
                    "",
                )
                .await
                .unwrap_or_default();
            match subvols {
                Ok(s) => Ok(format!(
                    "=== Btrfs Subvolumes ({mount_target}) ===\n{}\n\n=== Mounted Subvolumes ===\n{}",
                    s.trim(),
                    findmnt.trim()
                )),
                Err(e) => Ok(format!(
                    "Note: `btrfs subvolume list` requires elevated privileges ({e}).\n\n=== Mounted Btrfs Subvolumes (`findmnt -t btrfs`) ===\n{}",
                    findmnt.trim()
                )),
            }
        }
        other => {
            bail!("Unknown btrfs query '{other}'. Expected 'usage', 'mounts', or 'subvolumes'.")
        }
    }
}

fn dir_size_bytes(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                files += 1;
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    (files, bytes)
}

async fn run_packages(args: &SystemArgs, ctx: &ToolContext) -> Result<String> {
    let query = match args.query.trim() {
        "" => "stats",
        q => q,
    };

    match query {
        "stats" => {
            let count_pkgs = |flag: &'static str| async move {
                ctx.run_command("pacman", [flag], "")
                    .await
                    .map(|s| {
                        if s.starts_with('[') || s.trim().is_empty() {
                            0
                        } else {
                            s.lines().count()
                        }
                    })
                    .unwrap_or(0)
            };
            let (total, explicit, foreign, orphans) = tokio::join!(
                count_pkgs("-Qq"),
                count_pkgs("-Qeq"),
                count_pkgs("-Qmq"),
                count_pkgs("-Qdtq"),
            );

            let (pkg_files, pkg_bytes) = dir_size_bytes(Path::new("/var/cache/pacman/pkg"));
            let paru_cache = dirs::cache_dir()
                .unwrap_or_else(|| expand_tilde("~/.cache"))
                .join("paru/clone");
            let paru_dirs = fs::read_dir(&paru_cache).map_or(0, Iterator::count);

            Ok(format!(
                "=== CachyOS Package Statistics ===\n\
                 Total installed packages: {total}\n\
                 Explicitly installed:     {explicit}\n\
                 Foreign / AUR packages:   {foreign}\n\
                 Orphaned dependencies:    {orphans}\n\
                 Pacman cache (/var/cache/pacman/pkg): {pkg_files} archive(s) ({:.1} MB)\n\
                 Paru AUR clone cache (~/.cache/paru/clone): {paru_dirs} repo(s)",
                pkg_bytes as f64 / (1024.0 * 1024.0)
            ))
        }
        "orphans" | "foreign" | "explicit" => {
            let (flag, empty_msg) = match query {
                "orphans" => ("-Qdt", "No orphaned packages found."),
                "foreign" => ("-Qm", "No foreign/AUR packages found."),
                _ => ("-Qe", "No explicit packages found."),
            };
            ctx.run_command("pacman", [flag], empty_msg).await
        }
        "search" | "info" | "files" | "owns" => {
            let (flag, label, empty_msg) = match query {
                "search" => (
                    "-Qs",
                    "target (search query)",
                    "No matching installed packages.",
                ),
                "info" => ("-Qi", "target (package name)", "Package not found."),
                "files" => ("-Ql", "target (package name)", "No files found."),
                _ => ("-Qo", "target (file path)", "No owner found."),
            };
            let target = validate_safe_arg(&args.target, label)?;
            ctx.run_command("pacman", [flag, target], empty_msg).await
        }
        "modified" => {
            let out = ctx.run_command("pacman", ["-Qii"], "").await?;
            let modified: Vec<&str> = out
                .lines()
                .filter(|l| l.starts_with("MODIFIED\t") || l.contains("MODIFIED"))
                .collect();
            if modified.is_empty() {
                Ok("No modified package backup configuration files found.".into())
            } else {
                Ok(format!(
                    "=== Modified System Config Files (`pacman -Qii`) ===\n{}",
                    modified.join("\n")
                ))
            }
        }
        "updates" => {
            let repo_updates = match ctx
                .run_command(
                    "checkupdates",
                    Vec::<&str>::new(),
                    "No official repo updates.",
                )
                .await
            {
                Ok(s) => s,
                Err(_) => ctx
                    .run_command("pacman", ["-Qu"], "No cached repo updates.")
                    .await
                    .unwrap_or_default(),
            };
            let aur_updates = ctx
                .run_command("paru", ["-Qua"], "No AUR updates.")
                .await
                .unwrap_or_default();
            Ok(format!(
                "=== Pending Official Repo Updates ===\n{}\n\n=== Pending AUR Updates (`paru -Qua`) ===\n{}",
                repo_updates.trim(),
                aur_updates.trim()
            ))
        }
        other => bail!(
            "Unknown packages query '{other}'. Expected 'stats', 'orphans', 'foreign', 'explicit', 'search', 'info', 'files', 'owns', 'modified', or 'updates'."
        ),
    }
}

impl AsyncTool for Tool {
    const NAME: &'static str = "system";
    const DESCRIPTION: &'static str = "Monitor and diagnose CachyOS system state: hardware/scx/PSI (`overview`), systemd logs (`journal`), units/boot (`services`), top tasks (`processes`), Fish shell history/abbreviations (`fish`), Hyprland compositor (`hyprland`), Btrfs storage (`btrfs`), and `paru`/`pacman` packages (`packages`).";
    type Args = SystemArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            SystemAction::Overview => run_overview(ctx).await,
            SystemAction::Journal => run_journal(&args, ctx).await,
            SystemAction::Services => run_services(&args, ctx).await,
            SystemAction::Processes => run_processes(&args, ctx).await,
            SystemAction::Fish => run_fish(&args, ctx).await,
            SystemAction::Hyprland => run_hyprland(&args, ctx).await,
            SystemAction::Btrfs => run_btrfs(&args, ctx).await,
            SystemAction::Packages => run_packages(&args, ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fish_history_and_redacts_secrets() {
        let sample = "\
- cmd: cargo check --all-targets
  when: 1700000001
- cmd: export OPENAI_API_KEY=sk-1234567890
  when: 1700000002
- cmd: hyprctl monitors\\nhyprctl workspaces
  when: 1700000003
- cmd: curl -H \"Authorization: Bearer my-token\" https://example.com
  when: 1700000004
";
        let entries = parse_fish_history_str(sample);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].cmd, "cargo check --all-targets");
        assert_eq!(entries[0].when, Some(1700000001));
        assert_eq!(entries[1].cmd, "hyprctl monitors\nhyprctl workspaces");
    }

    #[test]
    fn validates_safe_arguments() {
        assert!(validate_safe_arg("hyprland.service", "unit").is_ok());
        assert!(validate_safe_arg("--privileged", "unit").is_err());
        assert!(validate_safe_arg("", "unit").is_err());
    }
}
