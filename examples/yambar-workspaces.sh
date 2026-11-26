#!/bin/sh
# Example yambar workspace script for EvaWM.
# Consumes `evawm msg subscribe --json` and emits yambar `script` module tags.
# Usage: yambar-workspaces.sh [OUTPUT_NAME]
# Requires: jq
set -eu

output="${1:-}"
bin="${EVAWM_BIN:-evawm}"

"$bin" msg subscribe --json | jq --unbuffered -r --arg output "$output" '
  .data as $d
  | (if $output != "" then
       ([$d.outputs[] | select(.name == $output)][0])
     else
       null
     end
     // ([$d.outputs[] | select(.active == true)][0])) as $out
  | ($out.name // "") as $name
  | "active_workspace|int|\($out.active_workspace_index // 0)",
    "output|string|\($name)",
    (range(1; 10) as $i
      | ([$d.workspaces[] | select(.output == $name and .index == $i)][0]) as $ws
      | "workspace_\($i)|string|\(if $ws then $i else "" end)",
        "workspace_\($i)_id|int|\($ws.id // 0)",
        "workspace_\($i)_active|bool|\($ws.active // false)",
        "workspace_\($i)_focused|bool|\($ws.focused // false)",
        "workspace_\($i)_windows|int|\($ws.window_count // 0)"),
    ""
'
