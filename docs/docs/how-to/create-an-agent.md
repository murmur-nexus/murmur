# Create an agent

The simplest way to create an agent is to use the `mur new agent` command. This will create a new agent directory with the necessary files. We'll go through the process of creating an agent in this section and show you how to apply custom logic to your agent.

## New Agent
It is recommended to have a dedicated directory for each agent. In your agent directory, run:

```bash
mur new agent my-custom-agent
```

This will create two files. 

1. `murmur-build.yaml`; the build manifest. This tells Murmur how to package your agent.
2. `src/main.py`; the agent's logic. This logic is executed when your agent is invoked.

*murmur-build.yaml:*

```yaml
name: my-custom-agent
type: agent
version: 0.0.1
description: 
instructions:
  - You are a helpful assistant.
metadata:
  author: 
```

*src/main.py:*

```python
from murmur_slim.build import ActivateAgent

my_custom_agent = ActivateAgent()
```

The `ActivateAgent` class is a base class that provides a simple interface for your agent. It has some [benefits build in](./benefits-of-using-the-activate-agent-class.md), such as state and metadata retrieval. If you still would like to use your own logic, you may find these [custom logic patterns](./apply-custom-logic-to-agents.md) useful.


## Build your agent
Run the following command to build your agent:

```bash
mur build
```

## Publish an agent
Run the following command to publish your agent:

```bash
mur publish
```

## Install an agent
First go to your project where you have `murmur.yaml` in your root. The `murmur.yaml` file is the orchestration file that defines your project and your agents and tools. It should look something like this:

*murmur.yaml*
```yaml
name: my-orchestration-project
version: 1.0.0
agents:
  - name: my-custom-agent
    version: 0.0.1
```

Then, run the following command to install your agent(s) (and tools):

```bash
mur install
```



