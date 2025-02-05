from murmur.build import ActivateAgent


def my_instructions_function() -> list[str]:
    prompt = """
        Remember {var1} and also {var2} in your response.
    """
    return prompt

# Initialize the task execution agent using the generic ActivateAgent
task_execution = ActivateAgent(
    instructions=my_instructions_function()
)
