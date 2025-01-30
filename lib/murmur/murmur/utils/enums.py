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
