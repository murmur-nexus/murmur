from datetime import datetime

import pytz
from swarm import Swarm
from utils import run_demo_loop

from murmur.agents import daily_routine, friendly_assistant, science_paper_explainer
from murmur.clients.swarm import SwarmAgent

############################################################
# Instantiate Swarm 🐝
############################################################

client = Swarm()

############################################################
# Tools
############################################################

from murmur.tools import get_arxiv_paper, get_bitcoin_exchange_rate, get_open_meteo_weather

############################################################
# Context Variables
############################################################

system_prompt = f"""
    - Only greet the user once.
    - Help users with their routines, if any. 
    - Routines are time specific. 
    - Important: Do not say goodbye.
    - Current time is {datetime.now(pytz.timezone('America/Los_Angeles'))}
"""

user_context = """
    Here is what you know about the user:
    - NAME: Joe
    - CURRENCY: USD
    - LOCATION: 34.0549° N, 118.2426° W
    - TEMPERATURE_UNIT: fahrenheit
    - TIMEZONE: America/Los_Angeles
    - SCIENCE_TOPIC: genai
"""

orchestration_context = """
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
"""

context_variables = {'system_prompt': system_prompt, 'user_context': user_context, 'routines': orchestration_context}

instructions = list(context_variables.values())

############################################################
# Agents
############################################################

friendly_assistant_agent = SwarmAgent(friendly_assistant, instructions=instructions)

daily_routine_agent = SwarmAgent(
    daily_routine, instructions=instructions, tools=[get_open_meteo_weather, get_bitcoin_exchange_rate]
)

science_paper_explainer_agent = SwarmAgent(science_paper_explainer, instructions=instructions, tools=[get_arxiv_paper])

############################################################
# Hand-off
############################################################


def transfer_to_daily_routine_agent():
    """Transfer daily routine requesting users."""
    return daily_routine_agent


def transfer_to_science_paper_explainer_agent():
    """Transfer science paper explainer requests."""
    return science_paper_explainer_agent


def transfer_to_friendly_assistant_agent():
    """Transfer science paper explainer requests."""
    return friendly_assistant_agent


friendly_assistant_agent.functions = [transfer_to_daily_routine_agent, transfer_to_science_paper_explainer_agent]

daily_routine_agent.functions.append(transfer_to_science_paper_explainer_agent)

science_paper_explainer_agent.functions.append(transfer_to_friendly_assistant_agent)

############################################################
# Loop
############################################################

if __name__ == '__main__':
    run_demo_loop(client, friendly_assistant_agent, context_variables=context_variables, debug=True)
