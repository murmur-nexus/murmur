# Custom agent logic

There are several ways to apply custom logic to your agent. We'll go through the most common ways to do this. Ultimately, you can do whatever you want in your agent. As long as the exported variable has the name of the agent and its value is an callable object returning either a string or a list of strings.

For example:

```python
class DynamicAssistant:
    instructions = [
        "Always end with a joke."
    ]

dynamic_assistant = DynamicAssistant()
```
Would be the same if you specify the instructions in the `murmur-build.yaml` build manifest file.

## Dynamic instructions
Here are some dynamic instructions examples using template string variables.

### Option 1: Class with instance method (recommended)

```python
from string import Template

class DynamicAssistant:
    base_instructions = [
        "Remember to say ${var1} and also ${var2} at the end of your response."
    ]
    
    def instructions(self, **kwargs) -> list[str]:
        """Process instructions with template variables.
        
        Args:
            **kwargs: Template variables to substitute in instructions
        """
        return [
            Template(instr).safe_substitute(**kwargs)
            for instr in self.base_instructions
        ]

# Create singleton instance
dynamic_assistant = DynamicAssistant()
```

### Option 2: Class with class method

```python
from string import Template

class DynamicAssistant:
    @classmethod
    def instructions(cls, **kwargs) -> list[str]:
        templates = [
            "Remember to tell something about ${var1}",
            "Also mention ${var2}."
        ]
        return [Template(instr).safe_substitute(**kwargs) for instr in templates]

dynamic_assistant = DynamicAssistant()
```

### Option 3: Module-level property

```python
from string import Template

class DynamicAssistant:
    def __init__(self):
        self._base_instructions = [
            "Make a joke about ${var1}",
            "Use ${var2} format in your response"
        ]
    
    def instructions(self, **kwargs) -> list[str]:
        """Process instructions with template variables.
        
        Args:
            **kwargs: Template variables to substitute in instructions
        """
        return [
            Template(instr).safe_substitute(**kwargs)
            for instr in self._base_instructions
        ]

dynamic_assistant = DynamicAssistant()
```
