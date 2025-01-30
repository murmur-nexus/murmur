import logging
from types import ModuleType
from typing import Any, Literal

from langchain_core.globals import set_debug
from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import SystemMessage
from langchain_core.messages.base import BaseMessage
from pydantic import Field, model_validator

from murmur.utils.client_options import ClientOptions
from murmur.utils.instructions_handler import InstructionsHandler
from murmur.utils.logging_config import configure_logging

configure_logging()
logger = logging.getLogger(__name__)

if logger.getEffectiveLevel() <= logging.DEBUG:
    set_debug(True)


class LangGraphOptions(ClientOptions):
    """Configuration options specific to LangGraphAgent.

    Inherits common options from ClientOptions and extends them with LangGraph-specific settings.
    Adapts to match LangGraph's option types based on their supported language model configurations.

    Attributes:
        instructions (InstructionsMode): Controls instruction handling strategy.
            Inherited from ClientOptions.
        parallel_tool_calls (bool | None): Whether to allow multiple tool calls to execute
            in parallel. None is transformed to False.
        tool_choice (dict[str, str] | Literal['any', 'auto'] | str | None): Controls how
            the model selects and uses tools. None is allowed and passed through.
    """

    parallel_tool_calls: bool | None = Field(
        default=None, description='Whether to allow multiple tool calls to execute in parallel'
    )
    tool_choice: dict[str, str] | Literal['any', 'auto'] | str | None = Field(
        default=None, description='Controls whether and how the model uses tools'
    )

    @model_validator(mode='after')
    def transform_none_values(self) -> 'LangGraphOptions':
        """Transform None values to their appropriate defaults after validation."""
        # Only transform if the field was explicitly set
        if 'parallel_tool_calls' in self.__pydantic_fields_set__:
            self.parallel_tool_calls = True if self.parallel_tool_calls is None else self.parallel_tool_calls

        # tool_choice can remain None if set to None
        return self

    def get_bind_tools_kwargs(self) -> dict[str, Any]:
        """Get kwargs for bind_tools with proper handling of defaults and None values.

        Returns:
            Dictionary of non-None arguments to pass to bind_tools
        """
        # Only include LangGraph-specific fields, excluding parent class fields
        langgraph_fields = {'parallel_tool_calls': self.parallel_tool_calls, 'tool_choice': self.tool_choice}
        return {k: v for k, v in langgraph_fields.items() if v is not None}


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
            options: Configuration options for customizing agent behavior and tool usage

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

        bound_model = self.model.bind_tools(self.tools, **self.options.get_bind_tools_kwargs())

        logger.debug(f'Invoking model with {len(messages)} messages')
        logger.debug(f'Instructions: {self.instructions}')

        all_messages = [SystemMessage(content=self.instructions)] + messages
        return bound_model.invoke(all_messages)
