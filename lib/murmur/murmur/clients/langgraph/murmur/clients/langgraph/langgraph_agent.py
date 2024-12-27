import logging

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import SystemMessage
from langchain_core.messages.base import BaseMessage

from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging

configure_logging()
logger = logging.getLogger(__name__)


class LangGraphAgent:
    """Agent for managing language graph operations.

    Handles model invocation with instructions and tools, ensuring proper message handling
    and tool binding.
    """

    def __init__(
        self, module, instructions: list[str] | None = None, tools: list = [], model: BaseChatModel | None = None
    ):
        """Initialize the LangGraphAgent."""
        if not isinstance(model, BaseChatModel):  # Simpler check, covers None case too
            raise TypeError('model must be an instance of BaseChatModel')

        self.name = module.__name__
        instructions_handler = InstructionsHandler()
        self.instructions = instructions_handler.get_instructions(module, instructions)
        self.model = model
        self.tools = tools

    def invoke(self, messages: list[BaseMessage]) -> BaseMessage:
        """Invoke the model with the provided messages."""
        if not messages:
            raise ValueError('Messages list cannot be empty')

        model_with_tools = self.model.bind_tools(self.tools)
        logger.debug(f'Invoking model with {len(messages)} messages')
        logger.debug(f'Instructions: {self.instructions}')

        all_messages = [SystemMessage(content=self.instructions)] + messages
        return model_with_tools.invoke(all_messages)
