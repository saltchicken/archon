use serde_json::{json, Value};
use std::{env, process};

fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "context",
            "description": "Retrieves project directory structures and file contents. Use this to explore the codebase.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target_path": { "type": "string", "description": "Optional relative path (defaults to current directory)." }
                }
            }
        }
    })
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "--schema" {
        println!("{}", serde_json::to_string(&schema()).unwrap());
        return;
    }

    if args.len() < 2 {
        eprintln!("Error: Missing JSON arguments");
        process::exit(1);
    }

    let json_args: Value = serde_json::from_str(&args[1]).unwrap_or(json!({}));
    let target = json_args.get("target_path").and_then(|v| v.as_str()).unwrap_or("").trim();

    if target.starts_with('/') || target.contains("..") {
        eprintln!("Error: Path traversal blocked for target '{}'", target);
        process::exit(1);
    }

    let mut cmd = process::Command::new("context");
    if !target.is_empty() {
        cmd.arg(target);
    }

    match cmd.output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.stderr.is_empty() {
                combined.push_str("\n[STDERR]\n");
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            if combined.trim().is_empty() {
                println!("Context executed successfully (no output).");
            } else {
                println!("{}", combined);
            }
        }
        Err(e) => {
            eprintln!("Error executing context: {}", e);
            process::exit(1);
        }
    }
}
