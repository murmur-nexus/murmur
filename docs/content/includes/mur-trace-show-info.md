??? info "Different ways to identify a session"
    `mur trace show` with no argument reads the most recent session:

    ```bash
    mur trace show
    ```

    To name another one, pass an ordinal counting back from the newest (`@2`), the last 4 or more
    characters of its ID (`3e4b`), the full ID, or a path to its `trace.jsonl`:

    ```bash
    mur trace show @2
    mur trace show 3e4b
    mur trace show ses_6801f81dd28b4a9daf434e8324c4793e
    mur trace show path/to/trace.jsonl
    ```

    Use `--workdir <path>` if your session directories are not under `./workdir`. Every command
    that names a session takes the same addresses — see
    [Session addresses](../reference/cli.md#session-addresses).
