"""Enums used across the murmur package."""

from enum import Enum


class InstructionsMode(str, Enum):
    """Enum for instruction handling modes.

    Attributes:
        APPEND: Add provided instructions to found instructions
        REPLACE: Only use provided instructions, ignore found ones
    """

    APPEND = 'append'
    REPLACE = 'replace'


class ToolChoiceMode(str, Enum):
    """Enum for tool choice modes that control model tool calling behavior.

    Attributes:
        NONE: Model will not call any tools, only generates messages
        AUTO: Model can choose between generating messages or calling tools
        REQUIRED: Model must call one or more tools
    """

    NONE = 'none'
    AUTO = 'auto'
    REQUIRED = 'required'
