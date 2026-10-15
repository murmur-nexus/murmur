??? info "Different ways to install artifacts"
    `mur install` fetches an artifact from the source of its scope. `murmur/` artifacts come from
    the built-in source, `murmur-nexus/default-artifacts`, with no configuration.

    **Option A — name the source of a scope** in `~/.murmur/config.yaml`:

    ```yaml
    registry:
      sources:
        - repo: <owner>/<repo>
          token: "${GITHUB_TOKEN}"
    ```

    Then install by full name, with or without a version:

    ```bash
    mur install <owner>/<artifact-name>@<version>
    ```

    **Option B — pass a full GitHub reference** and skip configuration entirely:

    ```bash
    mur install github:<owner>/<repo>@<tag>
    ```

    See [Installing artifacts](../reference/installing-artifacts.md) to learn more.
