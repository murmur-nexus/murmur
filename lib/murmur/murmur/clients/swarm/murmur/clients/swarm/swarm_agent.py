import logging
from types import ModuleType

from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging
from swarm import Agent

# Configure logging
configure_logging()
logger = logging.getLogger(__name__)


class SwarmAgent(Agent):
    """SwarmAgent class that extends the base Agent class.

    This class is responsible for initializing a swarm agent with the provided agent module,
    instructions, and tools. It uses the InstructionsHandler to fetch the final instructions
    for the agent.

    Attributes:
        agent: The agent module from which the agent is created.
        instructions (list[str] | None): A list of instructions or None.
        tools (list): A list of tools to be used by the agent.
    """

    def __init__(self, agent: ModuleType, instructions: list[str] | None = None, tools: list = []) -> None:
        """Initialize the SwarmAgent.

        Args:
            agent: The agent module from which the agent is created
            instructions: Optional list of instruction strings
            tools: List of tool functions for the agent

        Raises:
            TypeError: If agent is not a valid module
        """
        agent_name = agent.__name__
        logger.debug(f'Initializing SwarmAgent with name: {agent_name}')

        instructions_handler = InstructionsHandler()
        final_instructions = instructions_handler.get_instructions(agent, instructions)
        logger.debug(f'Generated instructions: {final_instructions[:100]}...')  # Log truncated preview

        super().__init__(name=agent_name, instructions=final_instructions, functions=tools)
