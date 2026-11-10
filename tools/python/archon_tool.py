import sys
import json
import os


def resolve_secure_path(rel: str, workspace_root: str) -> str:
    """Resolves a relative path within workspace_root and blocks path traversal."""
    if not rel or os.path.isabs(rel):
        raise ValueError("Security violation: path must be relative to current working directory")
    cwd = os.path.realpath(workspace_root)
    target = os.path.realpath(os.path.join(cwd, rel))
    if os.path.commonpath([cwd, target]) != cwd:
        raise ValueError("Security violation: path escapes workspace")
    return target


def run_tool(schema: dict, execute_fn: callable):
    """Handles Archon's standard CLI/stdin boilerplate for Python tools."""
    if len(sys.argv) > 1 and sys.argv[1] == "--schema":
        print(json.dumps(schema))
        sys.exit(0)

    raw_input = sys.argv[1] if len(sys.argv) >= 2 else sys.stdin.read()
    if not raw_input.strip():
        print("Error: Missing JSON arguments", file=sys.stderr)
        sys.exit(1)

    payload = json.loads(raw_input)
    if isinstance(payload, dict) and "arguments" in payload and "context" in payload:
        args = payload["arguments"]
        context = payload["context"]
    else:
        args = payload
        context = {}

    try:
        workspace_root = context.get("workspace_root") or os.getcwd()
        execute_fn(args, workspace_root)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)
