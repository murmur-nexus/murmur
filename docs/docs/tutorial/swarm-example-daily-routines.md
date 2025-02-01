# Swarm Example: Daily routines
By the end of this tutorial, you will have built a swarm of agents that help you with your daily routines. You'll be ready to create more advanced morning routine orchestration. ☕️

## Why use OpenAI Swarm? 🐝

OpenAI's Swarm is a educational orchestration framework that allows you to run a swarm of agents. We use it here to demonstrate how to define Murmur agents and tools and seamlessly integrate with Swarm.

- ✅ Perfect for learning fundamental agentic concepts
- ✅ Stateless architecture makes it easy to learn
- ✅ Ideal for quick chatbot experimentation
- ❌ Not suitable for complex flows
- ❌ Not for production


## Requirements

- Get [OpenAI API key](https://platform.openai.com/api-keys) 🔑 

## What we are building

![Daily Routines](../assets/graph-daily-routines.png)
/// caption
Graph of agents and tools.
///

### Get an update on the latest news

- Gives today's bitcoin rate for given a currency
- Gives today's weather for given a location
- Gives random white paper excerpt given a topic

### Flow

![Daily Routines Flow](../assets/graph-explainer-daily-routines.png)
/// caption
Daily Routines Flow.
///

1.	The friendly assistant agent forwards you to the daily routine agent based on time.
2.	The daily routine agent retrieves weather and Bitcoin prices.
3.	It then forwards you to the science paper explainer agent.
4.	The science paper explainer agent fetches the latest arXiv paper on your topic.
5.	It summarizes and explains the paper.
6.	You are then returned to the friendly assistant agent.

### Context Variables

Since Swarm is stateless, we will have to manage the state ourselves. Swarm allows us to define context variables that we will use to pretend we have a user session which we will feed into the agents.

```
Here is what you know about the user:
    - NAME: Joe
    - CURRENCY: USD
    - LOCATION: 34.0549° N, 118.2426° W
    - TEMPERATURE_UNIT: fahrenheit
    - TIMEZONE: America/Los_Angeles
    - SCIENCE_TOPIC: genai
```

## Walkthrough the code
To build the orchestration flow we will need to define agents, tools, context variables and handoff functionality.

### Import agents and instantiate Swarm
The strength of Murmur is that you can pull in existing agents and tools into your project. Here we are importing the necessary agents alongside with some Swarm specific options to help configure the Swarm client. We will also instantiate the Swarm client. This client is responsible for orchestrating the agents once we kick it off.

```python
from murmur.agents import daily_routine, friendly_assistant, science_paper_explainer
from murmur.clients.swarm import SwarmAgent, SwarmOptions

############################################################
# Instantiate Swarm 🐝
############################################################

client = Swarm()
```

### Define the tools
Tools do not need to be defined but can simply be imported, just like our agents. These tools are pre-defiuned functions that execute some logic. In these cases we are calling public APIs.

```python
from murmur.tools import get_arxiv_paper, get_bitcoin_exchange_rate, get_open_meteo_weather
```

### Define the context variables

#### System prompt
```python
system_prompt = f"""
    - Only greet the user once.
    - Help users with their routines, if any. 
    - Routines are time specific. 
    - Current time is {datetime.now(pytz.timezone('America/Los_Angeles'))}
    - Routines to choose from:
        1. Morning Routine
            1.1. Get the weather
            1.2. Get the Bitcoin exchange rate
            1.3. Get the science paper
        2. Afternoon Routine
            2.1. Get the weather
            2.3. Get the science paper
        3. Evening Routine
            3.2. Get the Bitcoin exchange rate
            3.3. Get the science paper
    - Important: Do not say goodbye.
"""
```

This is a list of instructions that will be used for the friendly assistant agent. These instructions are specifc to this project and will be used by the agent to determine which routine to call based on the time of day.

Additionally, we define "routines" to make the orchestration a little more dynamic. Suffice to say, when we say "routines" we do not mean [Swarm's concept of routines](https://cookbook.openai.com/examples/orchestrating_agents#routines). 

!!! note "System prompt" 
  
    Each imported agent has its own "system prompt". The above system prompt only defines orchestration-specific instructions that may not make sence when agents are used in other projects, which is why they are separated here. Alternatively, you could incorporate these instructions in your agent if preferred. 


#### User context
Here we want to pretend we have a user session. Typically this is coming from an authentication flow or database. In this example you can specify your own user details which will be used by the agents to determine with which parameters to call the tools. For example, the weather tool uses the `LOCATION` and `TEMPERATURE_UNIT`. 

```python
user_context = """
    Here is what you know about the user:
    - NAME: Joe
    - CURRENCY: USD
    - LOCATION: 34.0549° N, 118.2426° W
    - TEMPERATURE_UNIT: fahrenheit
    - TIMEZONE: America/Los_Angeles
    - SCIENCE_TOPIC: genai
"""
```

## Define agents
We instantiate each Swarm agent with the imported agent, tools and instructions. For example, here we define the second agent:

```python
daily_routine_agent = SwarmAgent(
    daily_routine, # imported agent
    instructions=instructions, # system prompt + user context
    tools=[get_open_meteo_weather, get_bitcoin_exchange_rate] # imported tools
)
```

And we add an additional function that "handsoff" to another agent. The way that it works with Swarm is that the `daily_routine` agent is able to choose between three tools. The third one being able to transfer the user to the `science_paper_explainer` agent. Read more about [Swarm's handoff functionality](https://cookbook.openai.com/examples/orchestrating_agents#handoffs).

```python
def transfer_to_science_paper_explainer_agent() -> SwarmAgent:
    """Transfer control to science paper explainer agent."""
    
    return science_paper_explainer_agent

daily_routine_agent.functions.append(transfer_to_science_paper_explainer_agent)
```

## Conclusion

In this tutorial, we walked through creating a daily routines orchestration using OpenAI's Swarm and Murmur. This toy example demonstrates how to combine Murmur's pre-built agents and tools with Swarm's orchestration capabilities to create a simple but functional agentic flow. We have purposely used public APIs so we wouldn't need any additional API keys. Now it is up to you to use your own APIs and tools and make it as complex as you want! 🚀

Can you add more interesting tools for your morning routine? ☕️ 🥐

## Next steps

- [Source code of this example](https://github.com/murmur-nexus/murmur/tree/main/examples/swarm/daily-routines)
- [Pull agents & tools from this example](https://github.com/murmur-nexus/murmur-example-artifacts/tree/main)
- [Build your own agents and tools](./getting-started-with-examples.md)
- [Host your own agents and tools](../how-to/setup-pypi-server.md)
- Star the [:simple-github: murmur](https://github.com/murmur-nexus/murmur) repo 🌟 and join our [:simple-discord: Discord](https://discord.gg/RGKCfD8HhC) to continue learning.
