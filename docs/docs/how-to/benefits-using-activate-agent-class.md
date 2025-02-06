# Benefits of using ActivateAgent

In your orchestration you can call it like so:
```python
response = agent.invoke(messages=["In the beginning..."], var1="Adam", var2="Eve", var3="bye")
```
Note: agent.invoke, agent.run, agent.activate all do the same thing.

Also, var3 is ignored since it isn't referenced in the string template prompt.

Additionally you can request metadata of your agent
```python
name = agent.name                   # "friendly_agent"
type = agent.type                   # "agent"
version = agent.version             # "1.0.0"
description = agent.description     # "A friendly assistant that is keen to help."
instructions = agent.instructions   # "Remember {var1} and also {var2} in your response."
```

How to get the agent's response:
```python
my_agent_response = response.value
```

You can also call the state of the Agent
```python
agent_state = response.state
print(agent_state)
{
    "messages": ["In the beginning..."],
    "parsed_instructions": ["Remember Adam and also Eve in your response."],
    "template_variables": {"var1": "Adam", "var2": "Eve"}
}
```

You can also verify the responses like so:
```python
valid_response = response.success   # True or False
error_response = response.error     # String (error message)
```


