import logging
import string
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any
import yaml
from ..utils.logging_config import configure_logging

configure_logging()
logger = logging.getLogger(__name__)

@dataclass
class AgentResponse:
    """Response object that wraps the result value with execution state.

    Attributes:
        value: The actual result value
        success: Whether the execution was successful
        error: Error message if execution failed
        state: Execution state information (e.g., messages processed)
    """

    value: Any
    success: bool = True
    error: str | None = None
    state: dict[str, Any] = None  # type: ignore

    def __post_init__(self) -> None:
        """Initialize default empty dict for state if None."""
        if self.state is None:
            self.state = {}

def _load_build_manifest(agent_name: str) -> dict[str, Any]:
    """Load the murmur-build.yaml manifest file for the agent.

    Args:
        agent_name: Name of the agent to load manifest for (required)

    Returns:
        Dict containing manifest data

    Raises:
        ValueError: If agent_name is not provided
        FileNotFoundError: If manifest file doesn't exist
        yaml.YAMLError: If manifest is invalid
    """
    if not agent_name:
        raise ValueError("agent_name is required")
        
    murmur_path = Path(__file__).parent.parent
    manifest_path = murmur_path / 'agents' / agent_name / 'murmur-build.yaml'
    logger.debug(f"Manifest path: {manifest_path}")
    
    try:
        with open(manifest_path, "r") as f:
            return yaml.safe_load(f)
    except FileNotFoundError:
        raise FileNotFoundError(
            f"No murmur-build.yaml found for agent: '{agent_name}'. "
            "Are you sure the agent is installed?"
        )
    except yaml.YAMLError as e:
        logger.error(f"Invalid YAML in manifest for agent {agent_name}: {e}")
        raise

class ActivateAgent:
    """Agent for executing tasks.

    This agent provides multiple interface methods (run, invoke, activate) that all
    delegate to the core execution logic. The native interface is through __call__.
    """

    def __init__(self, agent_name: str, instructions: str | list[str] | None = None) -> None:
        """Initialize the ActivateAgent.

        Args:
            agent_name: Name of the agent to load
            instructions: Custom instructions for the agent as string or list of strings

        Raises:
            ValueError: If the agent name is invalid or agent cannot be found
            yaml.YAMLError: If manifest is invalid
        """
        try:
            self._manifest = _load_build_manifest(agent_name)
            self._name = self._manifest["name"]
            self._type = self._manifest["type"]
            self._version = self._manifest["version"]
            self._description = self._manifest["description"]
        except FileNotFoundError as e:
            raise ValueError(str(e)) from e

        # Handle instructions
        if instructions is None:
            instructions = self._manifest.get("instructions", [])

        # Convert string to list if needed
        if isinstance(instructions, str):
            self._instructions = [instructions]
        elif isinstance(instructions, list):
            self._instructions = instructions
        else:
            self._instructions = None

    @property
    def manifest(self) -> dict[str, Any]:
        """Get the manifest data."""
        return self._manifest

    @property
    def name(self) -> str:
        """Get agent name from manifest."""
        return self._name

    @property
    def type(self) -> str:
        """Get agent type from manifest."""
        return self._type

    @property
    def version(self) -> str:
        """Get agent version from manifest."""
        return self._version

    @property
    def description(self) -> str:
        """Get agent description from manifest."""
        return self._description

    @property
    def instructions(self) -> list[str] | None:
        """Get the agent instructions.

        Returns:
            A copy of the instructions list if set, None otherwise.
        """
        return self._instructions.copy() if self._instructions is not None else None

    def __call__(self, messages: str | list[str], **kwargs: Any) -> AgentResponse:
        """Execute the agent.

        Args:
            messages: Single message string or list of message strings
            **kwargs: Variable keyword arguments for template formatting

        Returns:
            AgentResponse containing the execution result and state
        """
        return self._execute_messages(messages, **kwargs)

    def run(self, messages: str | list[str], **kwargs: Any) -> AgentResponse:
        """Run interface for executing the agent."""
        return self._execute_messages(messages, **kwargs)

    def invoke(self, messages: str | list[str], **kwargs: Any) -> AgentResponse:
        """Invoke interface for executing the agent."""
        return self._execute_messages(messages, **kwargs)

    def activate(self, messages: str | list[str], **kwargs: Any) -> AgentResponse:
        """Activate interface for executing the agent."""
        return self._execute_messages(messages, **kwargs)

    def _execute_messages(self, messages: str | list[str], **kwargs: Any) -> AgentResponse:
        """Core execution logic that all interface methods delegate to.

        Args:
            messages: Single message string or list of message strings
            **kwargs: Variable keyword arguments for template formatting

        Returns:
            AgentResponse containing the execution result and state

        Raises:
            ValueError: If messages are empty or invalid
        """
        if isinstance(messages, str):
            messages = [messages]

        if not messages:
            raise ValueError('Messages cannot be empty')

        try:
            # Format instructions with provided variables if instructions exist
            parsed_instructions = []
            if self._instructions:
                format_kwargs = defaultdict(str, kwargs)

                for instruction in self._instructions:
                    try:
                        parsed_instruction = string.Formatter().vformat(instruction, args=(), kwargs=format_kwargs)
                        parsed_instructions.append(parsed_instruction)
                    except ValueError as e:
                        # Extract template keys and handle manually
                        keys = [fname for _, fname, _, _ in string.Formatter().parse(instruction) if fname]
                        safe_kwargs = {k: str(format_kwargs[k]) if format_kwargs[k] is not None else '' for k in keys}
                        parsed_instruction = instruction
                        for key, value in safe_kwargs.items():
                            parsed_instruction = parsed_instruction.replace(f'{{{key}}}', value)
                        logger.debug(f'Invalid format string in instruction: {instruction}. Error: {e}')
                        parsed_instructions.append(parsed_instruction)

            result = '\n'.join(parsed_instructions).strip() if parsed_instructions else 'No further instructions.'
            return AgentResponse(
                value=result,
                success=True,
                state={'messages': messages, 'parsed_instructions': parsed_instructions, 'template_variables': kwargs},
            )
        except Exception as e:
            return AgentResponse(
                value=None,
                success=False,
                error=str(e),
                state={'messages': messages, 'parsed_instructions': None, 'template_variables': kwargs},
            )
