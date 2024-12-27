# Tutorial: Getting Started with the Examples
By the end of this tutorial, you’ll have built and published your own agent and tools, installed Murmur, and confirmed everything works by running the provided examples. You’ll be ready to create more advanced agents and tools using the same approach.

## Prerequisites
!!! warning "Do this first!"

    [How to: Setup your own PyPI server](../how-to/setup-pypi-server.md). Use it to host your own agents and tools.

## Install Murmur

1.	(Optional) Create and activate a python virtual environment:

```bash
mkdir murmur-artifacts
cd murmur-artifacts
python -m venv venv
source venv/bin/activate
```

2. Install `murmur-nexus` and `mur-cli`, both are essential for running the examples. 

```bash
pip install murmur-nexus[langgraph,swarm] mur-cli
```

!!! note ""

    - **`murmur-nexus`**: Murmur enables the use of published agents and tools within your orchestration code.
    - **`mur-cli`**: Mur is a command-line interface for creating and managing agents and tools.

Verify the installation by running `mur --help`.

3. Done. To the moon! 🚀

!!! bug "Issues?"

    If during the installation process you experience any issues, please join our [:simple-discord: Discord](https://discord.gg/ZFvE8jr9) and ask for help.

## Publish an Agent
1. Clone the [:simple-github: native-artifacts](https://github.com/murmur-nexus/native-artifacts) repo into your workspace directory.

```bash
git clone https://github.com/murmur-nexus/native-artifacts.git
```

2. Build the `friendly-assistant` agent. Optionally edit the `murmur-build.yaml` file first.

```bash
cd native-artifacts/agents/friendly-assistant
mur build
```

You should see something like this:
![Docker Container Running](../assets/terminal-mur-build-friendly-assistant.png)
/// caption
Built for agent completed.
///

3.	Then in inside the created package directory run:

```bash
cd friendly_assistant
mur publish
```

!!! warning "Connection refused?"

    If you get a `Connection refused` error, it means either your PyPI server is not running or you didn't set the `MURMUR_INDEX_URL` environment variable in your current terminal session.

    ```bash
    export MURMUR_INDEX_URL=http://localhost:8080/simple
    ```
    
    To prevent re-setting the environment variable every time you open a new terminal session, add the above to your `.bashrc` or `.zshrc` file.

4.	You’ve published your first agent 🎉.

## Publish Tools
1. Build the `add` tool. Optionally edit the `murmur-build.yaml` file first. In the root project directory where `native-artifacts` resides (`cd ../../../../`) do:

```bash
cd native-artifacts/tools/add
mur build
```

2.	Then in inside the created package directory run:

```bash
cd add
mur publish
```

3.	You’ve published your first tool 🔥.

4.	Repeat step 1 to 3 for publishing tools for `multiply`, `divide`, and `is_prime`. These are the minimum  required tools for testing the basic examples. 


## Install Agents and Tools
1. Clone the [:simple-github: murmur](https://github.com/murmur-nexus/murmur) repo into your project directory (`/murmur-artifacts`).

```bash
git clone https://github.com/murmur-nexus/murmur.git
```

2.	cd into the `examples/basic` directory.

```bash
cd murmur/examples/basic
```

3.	Open `murmur.yaml` and adjust names/versions if needed. Here is what the file should look like:

``` { .yaml .annotate title=examples/murmur.yaml }
name: examples-basic
version: 1.0.0
agents:
  - name: friendly-assistant
    version: 1.0.0
tools:
  - name: add
    version: 1.0.0
  - name: multiply
    version: 1.0.0
  - name: divide
    version: 1.0.0
  - name: is_prime
    version: 1.0.0 # (1)
```

1.  It's recommended to put a strict version for maximum control. 

---

Almost there! You just need to install requirements for using the examples.

4. Install the requirements.txt file. 

```bash
pip install -r ../requirements.txt
```

5. Create `OPENAI_API_KEY` environment variable. Get [OpenAI API key](https://platform.openai.com/api-keys).

```bash
export OPENAI_API_KEY=sk-proj-jTOzfoS...
```

6.	Install all the agents and tools from `murmur.yaml`:

```bash
mur install
```
You should see something like this: ==Successfully installed all artifacts==.

5.	Execute an example. E.g. for LangGraph:

```bash
python langgraph_math.py
```

You just orchestrated your first agent and tools with Murmur! 🎉

## Wrap Up
- You can now run the examples and build your own agents and tools.
- Star the [:simple-github: murmur](https://github.com/murmur-nexus/murmur) repo 🌟 and join our [:simple-discord: Discord](https://discord.gg/U9x96DCJ) to continue learning.
- Submit any [feature requests or bugs](https://github.com/murmur-nexus/murmur/issues); we’d love your feedback.
