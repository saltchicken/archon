import sys
import json
import os

SCHEMA = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read the exact contents of a file in the workspace.",
        "parameters": {
            "type": "object",
            "properties": {
                "file_path": { "type": "string" }
            },
            "required": ["file_path"]
        }
    }
}

def resolve_secure_path(rel, workspace_root):
    if not rel or os.path.isabs(rel):
        raise ValueError("Security violation: path must be relative to current working directory")
    cwd = os.path.realpath(workspace_root)
    target = os.path.realpath(os.path.join(cwd, rel))
    if os.path.commonpath([cwd, target]) != cwd:
        raise ValueError("Security violation: path escapes workspace")
    return target

def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--schema":
        print(json.dumps(SCHEMA))
        sys.exit(0)

    raw_input = sys.argv[1] if len(sys.argv) >= 2 else sys.stdin.read()
    if not raw_input.strip():
        print("Error: Missing JSON arguments", file=sys.stderr)
        sys.exit(1)

    payload = json.loads(raw_input)

    # Unpack the JSON envelope
    if isinstance(payload, dict) and "arguments" in payload and "context" in payload:
        args = payload["arguments"]
        context = payload["context"]
    else:
        args = payload
        context = {}

    workspace_root = context.get("workspace_root") or os.getcwd()
    path_str = args.get("file_path", "")

    try:
        target_path = resolve_secure_path(path_str, workspace_root)
        
        if not os.path.exists(target_path):
            print(f"Error: File not found: {path_str}", file=sys.stderr)
            sys.exit(1)
            
        if not os.path.isfile(target_path):
            print(f"Error: Target is not a file: {path_str}", file=sys.stderr)
            sys.exit(1)

        with open(target_path, "r", encoding="utf-8") as f:
            content = f.read()

        # Print directly to stdout; the Rust engine captures this as the tool result
        print(content)
        
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
