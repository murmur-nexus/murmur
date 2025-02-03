from dataclasses import dataclass
from typing import Any, Union

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
    state: dict[str, Any] = None

class ActivateAgent:
    """Agent for executing tasks.
    
    This agent provides multiple interface methods (run, invoke, activate) that all
    delegate to the core execution logic. The native interface is through __call__.
    """
    
    def __init__(self, instructions: str | list[str] | None = None) -> None:
        """Initialize the ActivateAgent agent.
        
        Args:
            instructions: Custom instructions for the agent as string or list of strings
        """
        self._name: str = "task-execution"
        self._type: str = "agent"
        self._version: str = "1.0.0"
        self._description: str = "A task execution agent that helps execute tasks."
        
        # Convert string to list if needed
        if isinstance(instructions, str):
            self._instructions = [instructions]
        else:
            self._instructions = instructions

    @property
    def name(self) -> str:
        """Get the agent name."""
        return self._name

    @property
    def type(self) -> str:
        """Get the agent type."""
        return self._type

    @property
    def version(self) -> str:
        """Get the agent version."""
        return self._version

    @property
    def description(self) -> str:
        """Get the agent description."""
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
            
        if not messages or not any(msg.strip() for msg in messages):
            raise ValueError("Messages cannot be empty")
            
        try:
            # Format instructions with provided variables if instructions exist
            parsed_instructions = []
            if self._instructions:
                for instruction in self._instructions:
                    try:
                        parsed_instruction = instruction.format(**kwargs)
                        parsed_instructions.append(parsed_instruction)
                    except KeyError as e:
                        # Log missing variable but continue with original instruction
                        parsed_instructions.append(instruction)
            
            result = f"Processed with instructions: {parsed_instructions}" if parsed_instructions else "No instructions provided"
            return AgentResponse(
                value=result,
                success=True,
                state={
                    "messages": messages,
                    "parsed_instructions": parsed_instructions,
                    "template_variables": kwargs
                }
            )
        except Exception as e:
            return AgentResponse(
                value=None,
                success=False,
                error=str(e),
                state={
                    "messages": messages,
                    "parsed_instructions": None,
                    "template_variables": kwargs
                }
            )

# Create a singleton instance
activate_agent = ActivateAgent()