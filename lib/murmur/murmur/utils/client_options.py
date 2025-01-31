from pydantic import BaseModel, Field

from .enums import InstructionsMode


class ClientOptions(BaseModel):
    """Base configuration options for Murmur clients.

    Provides common configuration settings that can be inherited by specific client implementations.
    Handles instruction management, tool execution behavior, and model tool calling preferences.

    Attributes:
        instructions (InstructionsMode): Controls instruction handling strategy.
            Defaults to APPEND mode which combines provided and existing instructions.
            Use REPLACE mode to override existing with provided instructions.
    """

    instructions: InstructionsMode = Field(
        default=InstructionsMode.APPEND, description='How to handle provided instructions'
    )
