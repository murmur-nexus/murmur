??? info "Different ways to install artifacts"
    `mur install` needs to know where to fetch artifacts from. You have two options:

    **Option A — configure a registry source** in `~/.murmur/config.yaml`:

    ```yaml
    registry:
      default: official
      sources:
        - name: official
          type: github
          repo: <owner>/<repo>
          token: "${GITHUB_TOKEN}"
    ```

    Then install by artifact name and version:

    ```bash
    mur install <artifact-name@version>
    ```

    **Option B — pass a full GitHub reference** and skip configuration entirely:

    ```bash
    mur install github:<username>/<repo>@<tag>
    ```

    See [Installing artifacts](../reference/installing-artifacts.md) to learn more.
