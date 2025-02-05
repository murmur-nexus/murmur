# Create an agent

## Create a new agent
It is recommended to have a dedicated directory for each agent. In your agent directory, run:

```bash
mur new agent my-awesome-agent
```

This will create a `murmur-build.yaml` file. Fill out the details of your agent and save the file.

### Custom code

#!SECTION Examples with ActivateAgent class helper

# from murmur_slim.build import ActivateAgent


# def my_instructions_function() -> list[str]:
#     prompt = """
#         Remember to say {var1} and also {var2} at the end of your response.
#     """
#     return prompt

# # Initialize the task execution agent using the generic ActivateAgent
# dynamic_assistant = ActivateAgent(
#     instructions=my_instructions_function()
# )


#!SECTION Examples for non ActiveAgents approach

from string import Template

# Option 1: Class with instance method (recommended)
# class DynamicAssistant:
#     base_instructions = [
#         "Remember to say ${var1} and also ${var2} at the end of your response."
#     ]
    
#     def instructions(self, **kwargs) -> list[str]:
#         """Process instructions with template variables.
        
#         Args:
#             **kwargs: Template variables to substitute in instructions
#         """
#         return [
#             Template(instr).safe_substitute(**kwargs)
#             for instr in self.base_instructions
#         ]

# # Create singleton instance
# dynamic_assistant = DynamicAssistant()


# Option 2: Class with static instructions
# class DynamicAssistant:
#     instructions = [
#         "Always end with a joke."
#     ]

# dynamic_assistant = DynamicAssistant()

# Option 3: Class with class method
# class DynamicAssistant:
#     @classmethod
#     def instructions(cls, **kwargs) -> list[str]:
#         templates = [
#             "Remember to tell something about ${var1}",
#             "Also mention ${var2}."
#         ]
#         return [Template(instr).safe_substitute(**kwargs) for instr in templates]

# dynamic_assistant = DynamicAssistant()

# Option 4: Module-level property
# class DynamicAssistant:
#     def __init__(self):
#         self._base_instructions = [
#             "Make a joke about ${var1}",
#             "Use ${var2} format in your response"
#         ]
    
#     def instructions(self, **kwargs) -> list[str]:
#         """Process instructions with template variables.
        
#         Args:
#             **kwargs: Template variables to substitute in instructions
#         """
#         return [
#             Template(instr).safe_substitute(**kwargs)
#             for instr in self._base_instructions
#         ]

# dynamic_assistant = DynamicAssistant()

## Buid your agent
Run the following command to build your agent:

```bash
mur build
```

## Publish an agent

## Install an agent

