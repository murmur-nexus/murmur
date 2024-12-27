from swarm import Swarm

from murmur.agents import friendly_assistant
from murmur.clients.swarm import SwarmAgent
from murmur.tools import is_prime

# Expose OPENAI_API_KEY with OpenAI's API key in your environment

client = Swarm()

agent = SwarmAgent(friendly_assistant, tools=[is_prime])

query = input('Prompt: ')  # Example: "Is 23 prime?"

messages = [{'role': 'user', 'content': query}]
response = client.run(agent=agent, messages=messages)
print(response.messages[-1]['content'])
