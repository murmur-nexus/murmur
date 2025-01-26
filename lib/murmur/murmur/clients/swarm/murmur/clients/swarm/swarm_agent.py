import logging

from murmur.utils.client_options import ClientOptions
from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging
from swarm import Agent

# Configure logging
configure_logging()
logger = logging.getLogger(__name__)


class SwarmOptions(ClientOptions):
    """Configuration options specific to SwarmAgent.

    Inherits common options from ClientOptions and adds Swarm-specific options.
    Currently uses only common options, but can be extended with Swarm-specific ones.
    """

    pass


class SwarmAgent(Agent):
    """SwarmAgent class that extends the base Agent class.

    This class is responsible for initializing a swarm agent with the provided module,
    instructions, and tools. It uses the InstructionsHandler to fetch the final instructions
    for the agent.

    Attributes:
        module: The module from which the agent is created.
        instructions (list[str] | None): A list of instructions or None.
        tools (list): A list of tools to be used by the agent.
        options (SwarmOptions): Configuration options for the agent.
    """

    def __init__(
        self, module: type, instructions: list[str] | None = None, tools: list = [], options: SwarmOptions | None = None
    ) -> None:
        """Initialize the SwarmAgent.

        Args:
            module: The module from which the agent is created
            instructions: Optional list of instruction strings
            tools: List of tool functions for the agent
            options: Configuration options for the agent

        Raises:
            TypeError: If module is not a valid type or module
        """
        agent_name = module.__name__
        logger.debug(f'Initializing SwarmAgent with name: {agent_name}')

        # Initialize options with defaults if not provided
        options = options or SwarmOptions()

        instructions_handler = InstructionsHandler()
        final_instructions = instructions_handler.get_instructions(
            module=module, 
            provided_instructions=instructions, 
            instructions_mode=options.instructions
        )
        logger.debug(f'Generated instructions: {final_instructions[:100]}...')  # Log truncated preview

        super().__init__(
            name=agent_name,
            instructions=final_instructions,
            functions=tools,
            parallel_tool_calls=options.parallel_tool_execution,
            tool_choice=options.tool_choice
        )
