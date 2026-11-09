# Common subagent spawn strategy

Note: These are advisory

## Spawner
- Agent A spawns Agent B or additional agents to do independent work.
- Agent A continues doing separate work.
- Agent A uses 'wait' until Agent B finishes and the harness reports the result, or asks B directly if needed.
- This is basic delegation. Clearly specify the end goal for each subagent.

## Consultant
- Agent A spawns Agent B or additional agents that act as consultants to agent A.
  - Agent B may act as an adversary or focus only on certain aspects.
  - The subagent should have a different role or focus from Agent A.
- Agent A discusses with Agent B.
- Useful for more demanding tasks.

## Council

- Agent A starts N subagents to think about a topic. Each subagent thinks from a different perspective.
- For M rounds:
   - Subagents think and send their reports to Agent A.
   - Agent A compiles all their reports to a scratch file, word-by-word without any alteration.
   - Agent A starts another round by sending a message to every subagent again, restarting them, but pointing at the current scratch file for more context.
- Agent A then compiles the final subagent reports and forms a conclusion.
- Use a different scratch file for each round.
- Use for very, very delicate reasoning where different perspectives are useful.

## Work Queue
- Agent A uses the `$queue` skill to create a work-item queue and spawns N equivalent agents.
- Each subagent receives the absolute queue file path and a shared consumer assignment.
- Agent A reports the remaining waiting items through `progress` and waits until all agents are finished.
- This is useful for many work items and unnecessary for a small task.

## Pipeline graph
- Agent A uses the `$queue` skill to create a graph whose edges are queues and whose nodes are groups of parallel agents.
- For example:
  - Agent A creates three queues:
    - request queue
    - verify queue
    - result queue
  - It then creates three sets of agents:
    - request spawners create requests
    - workers take requests and push them to the verify queue, or skip directly to the result queue
    - verifiers take items from the verify queue and send them to results or back to requests for more work
  - Agent A takes from the result queue and reports aggregate waiting-item counts through `progress`.

## Fork Join

- Agent A has a large task or investigation. It first defines the scope of the task and how it can be broken up across N parallel independent agents. Consider triggering compaction if the remaining context is insufficient for all subagents' results before spawning subagents, after defining the tasks.
- Agent A spawns N independent agents, each given a subtask as specified before.
- N subagents independently execute their tasks. Each compiles a comprehensive report.
- Agent A waits until all subagents have completed.
- Agent A ruminates on the collective results returned by the subagents. Each subagent's history is available for reading in the current agent's $AGENT_DIR/subagent_name.
