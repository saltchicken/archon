import os
from archon_tool import resolve_secure_path, run_tool

SCHEMA = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read the exact contents of a file in the workspace.",
        "parameters": {
            "type": "object",
            "properties": {
                "file_path": {"type": "string"}
            },
            "required": ["file_path"]
        }
    }
}


def execute(args, workspace_root):
    path_str = args.get("file_path", "")
    target_path = resolve_secure_path(path_str, workspace_root)

    if not os.path.isfile(target_path):
        raise ValueError(f"Target is not a file (or not found): {path_str}")

    with open(target_path, "r", encoding="utf-8") as f:
        print(f.read())


if __name__ == "__main__":
    run_tool(SCHEMA, execute)
