# How to: Setup your own PyPI server

## Setting up environment variables

1. Setting the environment variable `MURMUR_INDEX_URL` will use the private registry adapter.

```bash
export MURMUR_INDEX_URL=http://localhost:8080/simple
```

!!! tip "Pro Tip"

    To prevent re-setting the environment variable every time you open a new terminal session, add the above to your `.bashrc` or `.zshrc` file.

## Set up your PyPI server

Create the following script to start the server.

```bash title="run_pypi_server.sh"
#!/bin/bash

# Use the PYPI_PACKAGES_DIR environment variable or default to ~/packages
PACKAGES_DIR=${PYPI_PACKAGES_DIR:-~/packages}

# Check if the directory exists; if not, create it
if [ ! -d "$PACKAGES_DIR" ]; then
    echo "Creating $PACKAGES_DIR directory..."
    mkdir -p "$PACKAGES_DIR"
else
    echo "$PACKAGES_DIR already exists."
fi

# Define the port, default to 8080 if PYPI_PORT is not set or empty
PORT=${PYPI_PORT:-8080}

# Create ~/.pypirc if it does not already exist and add [pypi] credentials
PYPIRC_PATH=~/.pypirc

if [ ! -f "$PYPIRC_PATH" ]; then
    echo "Creating $PYPIRC_PATH with default credentials..."
    cat <<EOL > "$PYPIRC_PATH"
[pypi]
username = admin
password = admin
EOL
else
    echo "$PYPIRC_PATH already exists."
fi

# Run the docker command
echo "Running the Docker container on port $PORT with data directory $PACKAGES_DIR..."
docker run -p "$PORT":8080 -v "$PACKAGES_DIR:/data/packages" pypiserver/pypiserver:latest run -a . -P .
```

### What does the script do?

- Defines a packages directory (defaulting to ~/packages) and creates it if missing
- Sets a port (defaulting to 8080) and writes a default ~/.pypirc if not present
- Runs the pypiserver Docker container, mapping the directory and port
- See optional environment variables below for [more options](./#more-options)

!!! note "Docker Desktop is required"

    Docker Desktop is a tool for managing and running containers. Install Docker Desktop on [Mac](https://docs.docker.com/desktop/setup/install/mac-install/) or [Windows](https://docs.docker.com/desktop/setup/install/windows-install/). Once installed, you can run the following command to start the server.

## Run local PyPI server with Docker

With the above script in place, run the following command to start the server.

```bash
chmod +x run_pypi_server.sh
./run_pypi_server.sh
```

![Docker Container Running](../assets/docker-running-pypi-server.png)
/// caption
Docker container is running.
///

!!! tip

    You can can safely cancel the server by pressing ++ctrl+"c"++ in the terminal. If you want to start it again, simply hit the play button in Docker Desktop. 

    ![Docker Container Running In Browser](../assets/rerun-docker-container-manually.png)


Verify the server is running by visiting `http://localhost:8080` in your browser.

![Docker Container Running In Browser](../assets/pypiserver-running-in-browser-confirmation.png)
/// caption
PyPI server is running.
///

## More options

??? abstract "Optional environment variables"

    - `PYPI_PORT`: The port to run the server on. Defaults to 8080.
    - `PYPI_PACKAGES_DIR`: The directory where the packages are stored. Defaults to ~/packages.
    - `PYPI_USERNAME`: The username for the PyPI server. Defaults to admin.
    - `PYPI_PASSWORD`: The password for the PyPI server. Defaults to admin.
    - `MURMUR_EXTRAS_INDEX_URL`: The URL of the extras index. Defaults to the public pypi index (for dependencies).

- No Docker? [Use pip](https://github.com/pypiserver/pypiserver?tab=readme-ov-file#quickstart-installation-and-usage)
- **More info**: [:simple-github: pypiserver](https://github.com/pypiserver/pypiserver)

## Wrap up

- You can now install packages from your local PyPI server using `mur install`.
- **Next:** If you haven't already, [get started with Murmur's examples](../tutorial/getting-started-with-examples.md).
