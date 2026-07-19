??? info "Different ways to identify a session"
    `mur trace show` locates a session three ways:

    **No argument — most recent session:**

    ```bash
    mur trace show
    ```

    Finds the lexicographically largest `ses_*` entry in `workdir/` — always the most recently created session, because session IDs are UUID v7 (time-ordered).

    **Short suffix — type the last few characters:**

    ```bash
    mur trace show 3e4b
    ```

    Matches any session whose ID ends with the given string. Use 4 or more characters to avoid ambiguity. Matching is case-insensitive. If the suffix matches more than one session, `mur` lists the candidates and asks you to be more specific.

    **Full session ID:**

    ```bash
    mur trace show ses_6801f81dd28b4a9daf434e8324c4793e
    ```

    Resolves the session directly without scanning `workdir/`.

    **Legacy file path:**

    ```bash
    mur trace show path/to/trace.jsonl
    ```

    Any argument containing `/` or ending in `.jsonl` is treated as a literal file path. Kept for backward compatibility.

    Use `--workdir <path>` if your session directory is not `./workdir`.
