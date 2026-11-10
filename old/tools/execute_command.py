import sys
import json
import shlex
import subprocess
import os

SCHEMA = {
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
}

def get_allowed_commands():
    allowed = ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
    if os.path.exists("config.toml"):
        try:
            import tomllib
            with open("config.toml", "rb") as f:
                data = tomllib.load(f)
                if "app" in data and "allowed_commands" in data["app"]:
                    allowed = data["app"]["allowed_commands"]
        except ImportError:
            with open("config.toml", "r") as f:
                for line in f:
                    if line.strip().startswith("allowed_commands"):
                        content = line.split("=", 1)[1].strip().strip("[]")
                        allowed = [c.strip().strip('",\'') for c in content.split(",") if c.strip()]
    return allowed

def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--schema":
        print(json.dumps(SCHEMA))
        sys.exit(0)

    if len(sys.argv) < 2:
        print("Error: Missing JSON arguments", file=sys.stderr)
        sys.exit(1)

    args = json.loads(sys.argv[1])
    raw_cmd = args.get("command", "").strip()

    if not raw_cmd:
        print("Error: Empty command", file=sys.stderr)
        sys.exit(1)

    for bad in [';', '&', '|', '`', '$', '<', '>']:
        if bad in raw_cmd:
            print(f"Error: Forbidden shell operator '{bad}'", file=sys.stderr)
            sys.exit(1)

    try:
        parts = shlex.split(raw_cmd)
    except ValueError as e:
        print(f"Error: Malformed command string - {e}", file=sys.stderr)
        sys.exit(1)

    if not parts:
        print("Error: Empty parsed command", file=sys.stderr)
        sys.exit(1)

    base = parts[0]
    rest = parts[1:]
    
    if base not in get_allowed_commands():
        print(f"Error: Command '{base}' is not in allowlist", file=sys.stderr)
        sys.exit(1)

    for arg in rest:
        if arg.startswith('/') or '..' in arg:
            print(f"Error: Path traversal blocked for argument '{arg}'", file=sys.stderr)
            sys.exit(1)

    try:
        result = subprocess.run([base] + rest, capture_output=True, text=True)
        combined = result.stdout
        if result.stderr:
            combined += f"\n[STDERR]\n{result.stderr}"
        print("Command executed successfully (no output)." if not combined.strip() else combined)
    except Exception as e:
        print(f"Error executing command: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
