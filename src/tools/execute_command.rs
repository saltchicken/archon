use serde_json::{json, Value};
use std::{env, fs, process};

fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "execute_command",
            "description": "Executes an allowed shell command. No pipes or redirects.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The exact command to execute." }
                },
                "required": ["command"]
            }
        }
    })
}

fn get_allowed_commands() -> Vec<String> {
    let default = vec!["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
        .into_iter()
        .map(String::from)
        .collect();

    if let Ok(content) = fs::read_to_string("config.toml") {
        if let Ok(parsed) = content.parse::<toml::Value>() {
            // Fix: Use .get() chaining instead of .pointer() for toml::Value
            if let Some(cmds) = parsed.get("app")
                .and_then(|app| app.get("allowed_commands"))
                .and_then(|v| v.as_array()) 
            {
                return cmds.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            }
        }
    }
    default
}

fn main() {
    let args: Vec<String> = env::args().collect();
    
    // Respond to dynamic schema request
    if args.len() > 1 && args[1] == "--schema" {
        println!("{}", serde_json::to_string(&schema()).unwrap());
        return;
    }

    if args.len() < 2 {
        eprintln!("Error: Missing JSON arguments");
        process::exit(1);
    }

    let json_args: Value = match serde_json::from_str(&args[1]) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Error: Invalid JSON arguments");
            process::exit(1);
        }
    };

    let raw_cmd = json_args.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
    if raw_cmd.is_empty() {
        eprintln!("Error: Empty command");
        process::exit(1);
    }

    for bad in [";", "&", "|", "`", "$", "<", ">"] {
        if raw_cmd.contains(bad) {
            eprintln!("Error: Forbidden shell operator '{}'", bad);
            process::exit(1);
        }
    }

    let parts = match shlex::split(raw_cmd) {
        Some(p) => p,
        None => {
            eprintln!("Error: Malformed command string");
            process::exit(1);
        }
    };

    if parts.is_empty() {
        eprintln!("Error: Empty parsed command");
        process::exit(1);
    }

    let base = &parts[0];
    let rest = &parts[1..];

    if !get_allowed_commands().contains(base) {
        eprintln!("Error: Command '{}' is not in allowlist", base);
        process::exit(1);
    }

    for arg in rest {
        if arg.starts_with('/') || arg.contains("..") {
            eprintln!("Error: Path traversal blocked for argument '{}'", arg);
            process::exit(1);
        }
    }

    match process::Command::new(base).args(rest).output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.stderr.is_empty() {
                combined.push_str("\n[STDERR]\n");
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            if combined.trim().is_empty() {
                println!("Command executed successfully (no output).");
            } else {
                println!("{}", combined);
            }
        }
        Err(e) => {
            eprintln!("Error executing command: {}", e);
            process::exit(1);
        }
    }
}
