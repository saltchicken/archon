import sys
import json
import os

SCHEMA = {
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
}

def resolve_secure_path(rel, must_exist=True):
    if not rel or rel.startswith('/') or '..' in rel:
        raise ValueError("Security violation: path must be relative to current working directory")
    cwd = os.path.abspath(os.getcwd())
    target = os.path.abspath(os.path.join(cwd, rel))
    if must_exist and not os.path.exists(target):
        raise FileNotFoundError(f"File not found: {rel}")
    if not target.startswith(cwd):
        raise ValueError("Security violation: path escapes workspace")
    return target

def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--schema":
        print(json.dumps(SCHEMA))
        sys.exit(0)

    if len(sys.argv) < 2:
        print("Error: Missing JSON arguments", file=sys.stderr)
        sys.exit(1)

    args = json.loads(sys.argv[1])
    path_str = args.get("file_path", "")
    search_str = args.get("search_string", "")
    replace_str = args.get("replace_string", "")

    try:
        target_path = resolve_secure_path(path_str, must_exist=True)
        with open(target_path, "r", encoding="utf-8") as f:
            content = f.read()
        if search_str not in content:
            print(f"Error: Exact search_string not found in {path_str}", file=sys.stderr)
            sys.exit(1)
            
        updated = content.replace(search_str, replace_str, 1)
        with open(target_path, "w", encoding="utf-8") as f:
            f.write(updated)
            
        print(f"Successfully edited {path_str}")
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
