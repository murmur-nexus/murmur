import logging
from types import ModuleType
from typing import Any

from pydantic import Field, model_validator

from murmur.utils.client_options import ClientOptions
from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging
from swarm import Agent

configure_logging()
logger = logging.getLogger(__name__)


class SwarmOptions(ClientOptions):
    """Configuration options specific to SwarmAgent.

    Inherits common options from ClientOptions and extends them with Swarm-specific settings.
    Adapts to match Swarm's option types based on their supported language model configurations.

    Attributes:
        instructions (InstructionsMode): Controls instruction handling strategy.
            Inherited from ClientOptions.
        parallel_tool_calls (bool): Whether to allow multiple tool calls to execute
            in parallel. Defaults to True if not explicitly set.
        tool_choice (str | None): Controls how the model selects and uses tools.
            Defaults to None, transformed to "auto" if not explicitly set.
    """

    parallel_tool_calls: bool | None = Field(
        default=None, description='Whether to allow multiple tool calls to execute in parallel'
    )
    tool_choice: str | None = Field(default=None, description='Controls whether and how the model uses tools')

    @model_validator(mode='after')
    def transform_none_values(self) -> 'SwarmOptions':
        """Transform None values to their appropriate defaults after validation."""
        # Always transform parallel_tool_calls to True if None
        if self.parallel_tool_calls is None:
            self.parallel_tool_calls = True

        # Transform tool_choice to "auto" if None
        if self.tool_choice is None:
            self.tool_choice = 'auto'
        return self

    def get_bind_tools_kwargs(self) -> dict[str, Any]:
        """Get kwargs for bind_tools with proper handling of defaults and None values.

        Returns:
            Dictionary of non-None arguments to pass to bind_tools
        """
        # Only include Swarm-specific fields, excluding parent class fields
        swarm_fields = {'parallel_tool_calls': self.parallel_tool_calls, 'tool_choice': self.tool_choice}
        return {k: v for k, v in swarm_fields.items() if v is not None}


class SwarmAgent(Agent):
    """SwarmAgent class that extends the base Agent class.

    This class is responsible for initializing a swarm agent with the provided agent module,
    instructions, and tools. It uses the InstructionsHandler to fetch the final instructions
    for the agent.

    Attributes:
        agent: The agent module from which the agent is created.
        instructions (list[str] | None): A list of instructions or None.
        tools (list): A list of tools to be used by the agent.
        options (SwarmOptions): Configuration options for the agent.
    """

    def __init__(
        self,
        agent: ModuleType,
        instructions: list[str] | None = None,
        tools: list = [],
        options: SwarmOptions | None = None,
    ) -> None:
        """Initialize the SwarmAgent.

        Args:
            agent: The agent module from which the agent is created
            instructions: Optional list of instruction strings
            tools: List of tool functions for the agent
            options: Configuration options for the agent

        Raises:
            TypeError: If agent is not a valid module
        """
        agent_name = agent.__name__
        logger.debug(f'Initializing SwarmAgent with name: {agent_name}')

        # Initialize options with defaults if not provided
        options = options or SwarmOptions()

        instructions_handler = InstructionsHandler()
        final_instructions = instructions_handler.get_instructions(
            module=agent, provided_instructions=instructions, instructions_mode=options.instructions
        )
        logger.debug(f'Client Options: {options.get_bind_tools_kwargs()}')
        logger.debug(f'Generated instructions: {final_instructions[:100]}...')  # Log truncated preview

        super().__init__(
            name=agent_name, instructions=final_instructions, functions=tools, **options.get_bind_tools_kwargs()
        )
