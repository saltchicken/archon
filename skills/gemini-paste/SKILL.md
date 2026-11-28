+++
command = "gemini-paste"
description = "Open Gemini in the browser and paste the workspace snapshot into its chat input box. No screenshot."
name = "gemini-paste"
tools = ["browser"]

[[hooks]]
tool = "browser"

[hooks.args]
action = "goto"
url = "https://gemini.google.com"
wait_secs = 15

[[hooks]]
tool = "browser"

[hooks.args]
action = "paste"
selector = 'div.ql-editor[contenteditable="true"]'
source = "workspace_snapshot"
+++
# Gemini Workspace Paste

On activation, the hooks have already:
1. **Navigated** to `https://gemini.google.com/app` (Gemini redirects `/` → `/app`).
2. **Pasted** the full workspace snapshot into Gemini's chat input (`div.ql-editor[contenteditable="true"]`).

## Your job
1. **Verify** the paste landed — check the `paste` hook result for a character count. If it says the editor accepted the content, you're done.
2. **Report** concisely:
   - Target: Gemini chat input
   - Source: `workspace_snapshot`
   - Size: N characters pasted
   - Status: ready to send (user hits Enter / the send button in the browser)
3. **Do NOT** take a screenshot, click send, or interact further with the page.

## If the paste failed
- The selector may have changed. Use `browser eval` to locate the current contenteditable:
  ```js
  document.querySelector('[contenteditable="true"], textarea, [role="textbox"]')
  ```
- Re-attempt `browser paste` with the discovered selector.
