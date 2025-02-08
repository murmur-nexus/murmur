import logging
import string
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Union
import sys

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


def _load_manifest(agent_name: str | None = None) -> dict[str, Any]:
    """Load the murmur-build.yaml manifest file for the importing agent.

    Args:
        agent_name: Optional explicit agent name to load manifest for

    Returns:
        Dict containing manifest data

    Raises:
        FileNotFoundError: If manifest file doesn't exist with explicit agent_name
        yaml.YAMLError: If manifest is invalid
        RuntimeError: If called outside an agent module without agent_name
    """
    murmur_path = Path(__file__).parent.parent

    # If agent_name is provided, use it or fail
    if agent_name is not None:
        manifest_path = murmur_path / 'agents' / agent_name / 'murmur-build.yaml'
        try:
            with open(manifest_path, "r") as f:
                return yaml.safe_load(f)
        except FileNotFoundError:
            raise FileNotFoundError(f"No murmur-build.yaml found for agent: {agent_name}")
        except yaml.YAMLError as e:
            logger.error(f"Invalid YAML in manifest for agent {agent_name}: {e}")
            raise
    
    # Only use frame traversal if no agent_name provided
    frame = sys._getframe(1)
    while frame:
        # Traverse up the frame hierarchy until we find who is creating the ActivateAgent
        module_name = frame.f_globals['__name__']
        if module_name.startswith('murmur.agents.'):
            detected_agent = module_name.split('.')[2]
            manifest_path = murmur_path / 'agents' / detected_agent / 'murmur-build.yaml'
            
            try:
                with open(manifest_path, "r") as f:
                    return yaml.safe_load(f)
            except yaml.YAMLError as e:
                logger.error(f"Invalid YAML in manifest: {e}")
                raise
            
        frame = frame.f_back
    
    raise RuntimeError(
        "Could not detect agent module. Ensure ActivateAgent is instantiated from an agent module "
        "or provide an explicit agent_name."
    )


class ActivateAgent:
    """Agent for executing tasks.

    This agent provides multiple interface methods (run, invoke, activate) that all
    delegate to the core execution logic. The native interface is through __call__.
    """

    def __init__(self, agent_name: str | None = None, instructions: str | list[str] | None = None) -> None:
        """Initialize the ActivateAgent agent.

        Args:
            agent_name: Optional explicit name of the agent to load
            instructions: Custom instructions for the agent as string or list of strings

        Raises:
            FileNotFoundError: If manifest file cannot be found
            yaml.YAMLError: If manifest is invalid
            RuntimeError: If called outside an agent module
        """
        self._name: str | None = None
        self._type: str | None = None
        self._version: str | None = None
        self._description: str | None = None
        self._manifest: dict[str, Any] | None = None
        
        # Pass explicit agent_name to _load_manifest if provided
        if instructions is None:
            self._manifest = _load_manifest(agent_name)
            instructions = self.manifest.get("instructions", [])

        # Convert string to list if needed
        self._instructions: list[str] | None
        if isinstance(instructions, str):
            self._instructions = [instructions]
        elif isinstance(instructions, list):
            self._instructions = instructions
        else:
            self._instructions = None

    @property
    def manifest(self) -> dict[str, Any]:
        """Lazy load manifest when needed."""
        if self._manifest is None:
            self._manifest = _load_manifest()
        return self._manifest

    @property
    def name(self) -> str:
        """Get agent name from manifest."""
        if self._name is None:
            self._name = self.manifest["name"]
        return self._name

    @property
    def type(self) -> str:
        """Get agent type from manifest."""
        if self._type is None:
            self._type = self.manifest["type"]
        return self._type

    @property
    def version(self) -> str:
        """Get agent version from manifest."""
        if self._version is None:
            self._version = self.manifest["version"]
        return self._version

    @property
    def description(self) -> str:
        """Get agent description from manifest."""
        if self._description is None:
            self._description = self.manifest["description"]
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

    def _execute_messages(self, messages: Union[str, list[str]], **kwargs: Any) -> AgentResponse:
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
                # Returns empty string for missing keys
                format_kwargs = defaultdict(str, kwargs)

                for instruction in self._instructions:
                    try:
                        parsed_instruction = string.Formatter().vformat(instruction, args=(), kwargs=format_kwargs)
                        parsed_instructions.append(parsed_instruction)
                    except ValueError as e:
                        # Extract template keys
                        keys = [fname for _, fname, _, _ in string.Formatter().parse(instruction) if fname]

                        # Convert values to strings, use empty string for None
                        safe_kwargs = {k: str(format_kwargs[k]) if format_kwargs[k] is not None else '' for k in keys}

                        # Replace template vars manually
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

    def _load_manifest(self) -> None:
        """Load manifest and set all properties."""
        manifest = self.manifest
        self._name = manifest["name"]
        self._type = manifest["type"]
        self._version = manifest["version"]
        self._description = manifest["description"]
