import sys
import json
import subprocess

SCHEMA = {
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
}

def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--schema":
        print(json.dumps(SCHEMA))
        sys.exit(0)

    if len(sys.argv) < 2:
        print("Error: Missing JSON arguments", file=sys.stderr)
        sys.exit(1)

    args = json.loads(sys.argv[1])
    target = args.get("target_path", "").strip()

    if target.startswith('/') or '..' in target:
        print(f"Error: Path traversal blocked for target '{target}'", file=sys.stderr)
        sys.exit(1)

    cmd = ["context"]
    if target:
        cmd.append(target)

    try:
        result = subprocess.run(cmd, capture_output=True, text=True)
        combined = result.stdout
        if result.stderr:
            combined += f"\n[STDERR]\n{result.stderr}"
        print("Context executed successfully (no output)." if not combined.strip() else combined)
    except FileNotFoundError:
        print("Error: The 'context' binary is not installed or not in PATH.", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"Error executing context: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
