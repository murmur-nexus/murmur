import sys

from langchain_core.messages import HumanMessage
from langchain_openai import ChatOpenAI
from langgraph.graph import START, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode, tools_condition

from murmur.agents import friendly_assistant
from murmur.clients.langgraph import LangGraphAgent
from murmur.tools import add, divide, multiply

# Make sure to expose OPENAI_API_KEY in the environment


def main():
    model = ChatOpenAI(model='gpt-4o', temperature=0)

    tools = [add, multiply, divide]
    agent = LangGraphAgent(friendly_assistant, model=model, tools=tools)

    def assistant_node(state: MessagesState):
        return {'messages': [agent.invoke(state['messages'])]}

    workflow = StateGraph(MessagesState)

    workflow.add_node('assistant', assistant_node)
    workflow.add_node('tools', ToolNode(tools))

    workflow.add_edge(START, 'assistant')
    workflow.add_conditional_edges('assistant', tools_condition)
    workflow.add_edge('tools', 'assistant')

    graph = workflow.compile()

    messages = [HumanMessage(content='Multiply 11 by 14. Add 3. Divide the output by 50')]
    messages = graph.invoke({'messages': messages})
    for m in messages['messages']:
        m.pretty_print()


if __name__ == '__main__':
    main()
    sys.exit()
