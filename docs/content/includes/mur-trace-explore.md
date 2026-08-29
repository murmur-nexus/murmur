??? info "Other trace exploration commands"
    `mur trace` has four subcommands for exploring session output:

    **`mur trace show`** — print the full trace for a session to the terminal.

    **`mur trace steps`** — show a turn-by-turn summary of what the agent did in a session. Pass `--verbose` to include a truncated summary of each tool's input.

    **`mur trace diff`** — compare the traces of two sessions side by side, or with no arguments the two most recent. Useful for spotting behavioural regressions between runs.

    **`mur trace report`** — generate a structured summary report from a session's trace. Covers token usage, tool calls, latency, and other session-level metrics.
