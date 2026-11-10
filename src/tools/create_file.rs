use serde_json::{json, Value};
use std::{env, fs, path::PathBuf, process};

fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "create_file",
            "description": "Create a new file in the workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["file_path", "content"]
            }
        }
    })
}

fn resolve_secure_path(rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
        return Err("Security violation: path must be relative to current working directory".into());
    }
    // We cannot canonicalize a file that does not exist yet to verify bounds,
    // so we rely heavily on the textual verification blocking `/` and `..` above.
    let cwd = env::current_dir().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?;
    Ok(cwd.join(rel))
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
    let path_str = json_args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    let content = json_args.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let target_path = match resolve_secure_path(path_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    };

    if target_path.exists() {
        eprintln!("Error: File already exists. Use edit_file instead.");
        process::exit(1);
    }

    if let Some(parent) = target_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("Error creating directories: {}", e);
            process::exit(1);
        }
    }

    if let Err(e) = fs::write(&target_path, content) {
        eprintln!("Error writing file: {}", e);
        process::exit(1);
    }

    println!("Successfully created {}", path_str);
}
