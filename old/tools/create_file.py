import sys
import json
import os

SCHEMA = {
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
}

def resolve_secure_path(rel):
    if not rel or rel.startswith('/') or '..' in rel:
        raise ValueError("Security violation: path must be relative to current working directory")
    cwd = os.path.abspath(os.getcwd())
    target = os.path.abspath(os.path.join(cwd, rel))
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
    content = args.get("content", "")

    try:
        target_path = resolve_secure_path(path_str)
        if os.path.exists(target_path):
            print(f"Error: File already exists. Use edit_file instead.", file=sys.stderr)
            sys.exit(1)
            
        parent = os.path.dirname(target_path)
        if parent:
            os.makedirs(parent, exist_ok=True)
            
        with open(target_path, "w", encoding="utf-8") as f:
            f.write(content)
            
        print(f"Successfully created {path_str}")
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
