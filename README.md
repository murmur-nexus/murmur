![Swarm Logo](docs/docs/assets/repo-header.png)

<div align="center">
  <a href="https://github.com/murmur-nexus/murmur/actions/workflows/ci.yml"><img src="https://github.com/murmur-nexus/murmur/actions/workflows/ci.yml/badge.svg" alt="CI Pipeline"></a>
</div>

# Murmur

Murmur is an open-source framework for packaging, publishing and managing AI agents and tools. It's designed to play well with the most popular orchestration frameworks, like LangGraph, CrewAI and AutoGen.

## Quickstart

To get started quickly, follow our [Getting Started with the Examples](https://murmur-nexus.github.io/murmur/tutorial/getting-started-with-examples/) tutorial.


## Installation

```
pip install murmur-nexus
```
Add optional extras to install specific orchestration clients, e.g.
```
pip install murmur-nexus[langgraph]
```
*Options: `langgraph`, `swarm`*. 

## Why use Murmur?

Spend more time orchestrating and less time (re)building agents and tools.

As the world transitions towards agentic workflows, developers face the challenge to ship countless of agents and tools for orchestration. This is desired without reinventing the wheel, managing dependencies, or dealing with compatibility issues. Murmur aims to solve these problems by providing an aggregation layer for AI agents and tools compatible with the most popular orchestration frameworks. 

## Feature Highlights

- **Modular and Interoperable**  
  Import agents and tools as packages to decouple them from your orchestration. Manage versions effectively to facilitate seamless updates and scalability.

- **System-Agnostic**  
  Easily integrate with open-source or cloud-based LLM systems, and enterprise packaging servers. Ensuring maximum compatibility with orchestration frameworks like LangGraph, AG2 and CrewAI.

- **Easy Interface**  
  Just as `pip` transformed Python package management, `mur` CLI aims to standardize the way developers build, manage and publish specialized AI agents and tools.

## Code Sample
Bare-bone example using OpenAI's Swarm:

```python
from swarm import Swarm

from murmur_swarm import SwarmAgent
from murmur.agents import friendly_assistant
from murmur.tools import is_prime

# Instantiate Swarm client as you normally would
client = Swarm()

# Instantiate a Murmur agent with your tools
# Your agent and tools are decoupled, so you can import them as packages
agent = SwarmAgent(friendly_assistant, tools=[is_prime])

# Query and parse responses as you normally would
query = input("Prompt: ") # Example: "Is 23 prime?"
messages = [{"role": "user", "content": query}]
response = client.run(agent=agent, messages=messages)
print(response.messages[-1]["content"])
```
*See more [examples](https://github.com/murmur-nexus/murmur/tree/main/examples) in the repo.*

## 🚀 Community

Murmur is in its early stages, and we're building it with the developer community in mind. Your insights, ideas, and use cases will help shape its future. **Join us on this journey to simplify the complex and unleash the potential of agentic workflows.** 

---

**Feedback**  
Try Murmur, and let us know what you think. Star this repo 🌟 and join our [Discord community 💬](https://discord.gg/U9x96DCJ) to share your feedback and help us make agentic workflows accessible for everyone.