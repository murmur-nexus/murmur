from murmur.build import ActivateAgent


def my_instructions_function() -> list[str]:
    prompt = """
        Remember {var1} and also {var2} in your response.
    """
    return prompt

# Initialize my_custom_agent using the ActivateAgent class
my_custom_agent = ActivateAgent(
    instructions=my_instructions_function()
)
