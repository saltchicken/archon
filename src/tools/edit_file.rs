use serde_json::{json, Value};
use std::{env, fs, path::PathBuf, process};

fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": "Edit a file by replacing an exact search string with a replacement string.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "search_string": { "type": "string" },
                    "replace_string": { "type": "string" }
                },
                "required": ["file_path", "search_string", "replace_string"]
            }
        }
    })
}

fn resolve_secure_path(rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
        return Err("Security violation: path must be relative to current working directory".into());
    }
    let cwd = env::current_dir().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?;
    let target = cwd.join(rel);
    
    if !target.exists() {
        return Err(format!("File not found: {}", rel));
    }
    
    let canon = target.canonicalize().map_err(|e| e.to_string())?;
    if !canon.starts_with(&cwd) {
        return Err("Security violation: path escapes workspace".into());
    }
    Ok(canon)
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
    let search_str = json_args.get("search_string").and_then(|v| v.as_str()).unwrap_or("");
    let replace_str = json_args.get("replace_string").and_then(|v| v.as_str()).unwrap_or("");

    let target_path = match resolve_secure_path(path_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    };

    let content = match fs::read_to_string(&target_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading file: {}", e);
            process::exit(1);
        }
    };

    if !content.contains(search_str) {
        eprintln!("Error: Exact search_string not found in {}", path_str);
        process::exit(1);
    }

    let updated = content.replacen(search_str, replace_str, 1);

    if let Err(e) = fs::write(&target_path, updated) {
        eprintln!("Error writing file: {}", e);
        process::exit(1);
    }

    println!("Successfully edited {}", path_str);
}
