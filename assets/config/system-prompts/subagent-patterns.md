# Common subagent spawn strategy
Note: These are advisory
## Spawner
- Agent A spawn Agent B or more agent to do indepandent work. - Agent A continue doing separate work. - Agent A uses 'wait' until Agent B send result or ask B directly if needed. - This is basic delegation, please clearly specify role or scope.
## Consultant
- Agent A spawn Agent B or more agent that act as consultant to agent A.
  - Agent B may act like adversary or focuses on certain aspec only.
  - The subagent should have diferent role or focus than Agent A.
- Agent A discuss with Agent B. - Useful for more demanding task.
## Council
- Agent A starts N subagent to think about a topic. Each subagent think from different perspective. - For M round:
   - subagents think and send their report to Agent A.
   - Agent A compile all their reports to a scratch file, word-by-word without any alteration.
   - Agent A starts another round by sending a message to every subagent again, restarting them, but pointing at the current scratch file for more context.
- Agent A then compile the final subagent reports and form a conclusion. - Use different scratch file for different round. - Use for very-very delicate reasoning.
## Work Queue
- Agent A creates a work-item queue and spawns N equivalent agents. - Each subagent is instructed to keep taking work items from its direct parent's queue. - Agent A waits until all agents are finished. - This is useful for many work items and unnecessary for a small task.
## Pipeline graph
- Agent A creates a graph whose edges are queues and whose nodes are groups of parallel agents. - For example:
  - Agent A creates three queues:
    - request queue
    - verify queue
    - result queue
  - It then creates three sets of agents:
    - request spawners create requests
    - workers take requests and push them to the verify queue, or skip directly to the result queue
    - verifiers take items from the verify queue and send them to results or back to requests for more work
  - Agent A takes from the result queue.