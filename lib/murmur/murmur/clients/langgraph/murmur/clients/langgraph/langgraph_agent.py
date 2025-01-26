import logging
from types import ModuleType

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import SystemMessage
from langchain_core.messages.base import BaseMessage

from murmur.utils.client_options import ClientOptions
from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging

configure_logging()
logger = logging.getLogger(__name__)


class LangGraphOptions(ClientOptions):
    """Configuration options specific to LangGraphAgent.

    Inherits common options from ClientOptions and adds LangGraph-specific options.
    Currently uses only common options, but can be extended with LangGraph-specific ones.
    """

    pass


class LangGraphAgent:
    """Agent for managing language graph operations with LangChain.

    This class provides an interface for running language models with custom instructions
    and tools in a LangGraph workflow. It handles proper message formatting, tool binding,
    and model invocation.

    Attributes:
        name (str): Name of the agent derived from the agent module
        instructions (str): Processed instructions for guiding model behavior
        model (BaseChatModel): LangChain chat model for generating responses
        tools (list): List of tool functions available to the model

    Raises:
        TypeError: If provided model is not a BaseChatModel instance
        ValueError: If messages list is empty during invocation
    """

    def __init__(
        self,
        agent: ModuleType,
        instructions: list[str] | None = None,
        tools: list = [],
        model: BaseChatModel | None = None,
        options: LangGraphOptions | None = None,
    ) -> None:
        """Initialize a new LangGraphAgent instance.

        Args:
            agent: Agent module containing base configuration
            instructions: Optional list of custom instructions to override defaults
            tools: List of tool functions to make available to the model
            model: LangChain chat model instance for generating responses

        Raises:
            TypeError: If model is not an instance of BaseChatModel
        """
        if not isinstance(model, BaseChatModel):
            raise TypeError('model must be an instance of BaseChatModel')

        self.name = agent.__name__
        self.options = options or LangGraphOptions()
        instructions_handler = InstructionsHandler()
        self.instructions = instructions_handler.get_instructions(
            module=agent, provided_instructions=instructions, instructions_mode=self.options.instructions
        )
        logger.debug(f'Generated instructions: {self.instructions[:100]}...')  # Log truncated preview

        self.model = model
        self.tools = tools

    def invoke(self, messages: list[BaseMessage]) -> BaseMessage:
        """Process messages through the model with tools and instructions.

        Takes a list of messages, prepends system instructions, binds available tools
        to the model, and returns the model's response.

        Args:
            messages: List of messages to process through the model

        Returns:
            BaseMessage: Model's response message

        Raises:
            ValueError: If messages list is empty
        """
        if not messages:
            raise ValueError('Messages list cannot be empty')

        bound_model = self.model.bind_tools(
            self.tools, parallel_tool_calls=self.options.parallel_tool_execution, tool_choice=self.options.tool_choice
        )

        logger.debug(f'Invoking model with {len(messages)} messages')
        logger.debug(f'Instructions: {self.instructions}')

        all_messages = [SystemMessage(content=self.instructions)] + messages
        return bound_model.invoke(all_messages)
