from pydantic import BaseModel, Field

from .enums import InstructionsMode, ToolChoiceMode


class ClientOptions(BaseModel):
    """Base configuration options shared across all Murmur clients.

    This class contains configuration options that are applicable to all clients.
    Client-specific options should inherit from this class.

    Attributes:
        instructions: How to handle provided instructions relative to found instructions.
            'append' (default) - Add provided instructions to found instructions
            'replace' - Only use provided instructions, ignore found ones
        parallel_tool_execution: Whether to allow multiple tool calls to execute in parallel.
            True (default) - Allow parallel tool execution
            False - Execute tools sequentially
        tool_choice: Controls tool calling behavior of the model.
            'none' - Model will not call any tools, only generates messages
            'auto' (default) - Model can choose between generating messages or calling tools
            'required' - Model must call one or more tools
            dict - Force model to call a specific tool by name
    """

    instructions: InstructionsMode = Field(
        default=InstructionsMode.APPEND, description='How to handle provided instructions'
    )
    parallel_tool_execution: bool = Field(
        default=True, description='Whether to allow multiple tool calls to execute in parallel'
    )
    tool_choice: ToolChoiceMode | dict = Field(
        default=ToolChoiceMode.AUTO,
        description='Controls whether and how the model uses tools'
    )
