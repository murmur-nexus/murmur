import logging

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
    """Agent for managing language graph operations.

    Handles model invocation with instructions and tools, ensuring proper message handling
    and tool binding.
    """

    def __init__(
        self,
        module: type,
        instructions: list[str] | None = None,
        tools: list = [],
        model: BaseChatModel | None = None,
        options: LangGraphOptions | None = None,
    ):
        """Initialize the LangGraphAgent.

        Args:
            module: The module containing instructions.
            instructions: Optional list of instruction strings.
            tools: List of tools to bind to the model.
            model: The language model to use.
            options: Configuration options for the agent.

        Raises:
            TypeError: If model is not an instance of BaseChatModel.
        """
        if not isinstance(model, BaseChatModel):
            raise TypeError('model must be an instance of BaseChatModel')

        self.name = module.__name__
        self.options = options or LangGraphOptions()

        instructions_handler = InstructionsHandler()
        self.instructions = instructions_handler.get_instructions(
            module=module, provided_instructions=instructions, instructions_mode=self.options.instructions
        )
        logger.debug(f'Generated instructions: {self.instructions[:100]}...')  # Log truncated preview

        self.model = model
        self.tools = tools

    def invoke(self, messages: list[BaseMessage]) -> BaseMessage:
        """Invoke the model with the provided messages."""
        if not messages:
            raise ValueError('Messages list cannot be empty')

        bound_model = self.model.bind_tools(
            self.tools, parallel_tool_calls=self.options.parallel_tool_execution, tool_choice=self.options.tool_choice
        )

        logger.debug(f'Invoking model with {len(messages)} messages')
        logger.debug(f'Instructions: {self.instructions}')

        all_messages = [SystemMessage(content=self.instructions)] + messages
        return bound_model.invoke(all_messages)
