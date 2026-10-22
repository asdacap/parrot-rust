# Design philosophy

Ported from the [Parrot .NET README](https://github.com/asdacap/parrotdotnet/blob/28e206c43829aa366e31bdeb0dd52711e3108d88/README.md)
at commit `28e206c43829aa366e31bdeb0dd52711e3108d88`. The principles and hypotheses
below apply to the Rust port; they do not assert implementation parity. The
model-alias link points to this repository's predefined configuration.

## Agent scope and context: design principles

Agents have limited context and cognitive capacity. Each agent should have a
bounded concern and a limited view of collaborators and resources, rather than
being presented with the whole system. These principles guide delegation and
tool-interface design; they are not a claim that every tool already enforces
them.

- **Keep resource discovery local.** An agent should only be presented with
  resources and collaborators that it created or that its direct parent created.
  Tool interfaces should expose this local surrounding, not a global inventory.
  This applies to queues, agent messaging (`agent_send`), and status tools, for
  example. Accessing another resource when its address is explicitly supplied
  does not violate this principle, provided the applicable permissions allow it.
  This is a context-management boundary, not a replacement for access control.
- **Conserve the parent's attention.** A parent may coordinate many children but
  still has limited cognitive capacity. Children should limit upward
  communication to useful results, blockers, and decisions requiring the parent,
  rather than streaming routine progress or raw working context. Avoid
  unnecessary messages to the parent; each one consumes attention. A final
  report should be self-contained and detailed enough that the parent does not
  need to ask the child for clarification, which would add another communication
  round. Include relevant outcomes, decisions and their rationale, validation,
  limitations, and outstanding issues. A long final answer is appropriate when
  needed; preferably compile a detailed report into a Markdown file and return
  its address with a concise summary. Conserve attention by making the handoff
  complete, not by omitting information the parent needs.
- **Delegate narrower scope.** A child should have a narrower scope than its
  parent and receive a more specific task or requirement. Scope includes not only
  task size, but also level of abstraction, decision responsibility, and
  ambiguity. Greater ambiguity means broader scope; greater specificity means
  narrower scope. Ambiguity and task size are independent dimensions, and both
  count: making a task smaller does not by itself make it a suitably scoped
  delegation if it leaves the parent's broad, unresolved decisions to the child.
- **Respect the lower scope bound.** A task also tends to have a lower scope
  bound: work can be too detailed for the agent's assigned role. An architect
  should delegate coding to a subagent or automate it through a script rather
  than taking on implementation directly. Delegation should keep the agent's
  concern bounded from below as well as above.

## LLM tiers and aliases: design hypotheses and goals

Providers offer several model tiers at different prices. If we want to reduce
cost, the obvious starting point is to use a cheaper model wherever possible.
But how low should we go? A lower token price does not necessarily mean a lower
cost for getting the work done. A smaller model may spend more tokens reaching
an answer, and if it makes a mistake, someone has to notice, explain the problem,
and try again. Enough of that churn can cost more than using a stronger model
from the start, while also taking longer. Parrot's [model aliases](../assets/config/predefined_config.runtime.yaml)
are meant to help make that choice: we are optimizing for cost, but it is the
cost of completing the task that matters.

The harder part is understanding what we are buying with the higher tier. Our
working hypothesis is that bigger models bring more practical wisdom and a
wider perspective, not just an ability to solve harder isolated problems.
Quantifiable measurements do not always do that justice. A medium-tier model
might score better by adding more checks, while a higher-tier model knows that
those checks are not needed in practice. The latter can look less thorough even
when it has made the better decision about what the work actually needs.

In that sense, stronger models seem to have more robust intuitive, or system-1,
thinking. Their work can look more ordinary or average, but these models are
supposed to work with humans. Knowing when a conventional answer is enough,
when to leave something alone, and which concerns deserve attention is part of
being a good collaborator. We want that judgment, not extra work merely because
it is easier to count.

This is also why delegating to a smaller model requires a more specific task.
Smaller models can focus deeply on something while losing sight of the wider
picture. The answer is not necessarily to give them less work; it is to leave
them fewer unresolved decisions. A task has both quantity and ambiguity, but
within a model's depth capability, ambiguity and judgment are what matter when
choosing a lower tier, rather than quantity. A large amount of well-defined work
may be a good fit, while a small question that requires weighing the whole
design may not be.

A smaller model can still go deep within a narrow problem when the surrounding
decisions have already been made, but reaching the same depth can take more
work. Our hypothesis is that a stronger model's better intuition often lets it
reach similar depth with far fewer tokens. A smaller model may need to think
through the problem, write out a report, and check several possibilities before
deciding on a solution. A bigger model is more like an experienced expert: it
recognizes a familiar situation, catches the relevant details naturally, and
knows what to do without having to reason through every step explicitly. It is
like the way an expert seems to already know the answer from having seen the
pattern before.

That token advantage does not mean the stronger model actually costs less.
Luna is a useful example: its token price is so low that doing more thinking
can still be much cheaper in money. In that case, the penalty is largely latency.
We should not confuse fewer tokens with lower monetary cost, or assume that the
possibility of a stronger model being cheaper makes it the cheaper choice.

There is also a limit to how deep a smaller model can go, regardless of how long
it thinks. More time does not indefinitely compensate for capability. Narrowing
the scope and settling the surrounding decisions can make a task suitable for
a lower tier, but cannot make a problem within that scope arbitrarily tractable.
If the required depth exceeds what the lower-tier model can handle, it must not
be chosen, however cheap its tokens are. Depth is therefore a capability bound
as well as a question of efficiency. Independently of depth, a stronger model
may also be needed to step back, consider other perspectives, and judge whether
the local work is the right thing to do at all.

That is the reasoning behind the intended alias roles. `low_llm` handles explicit
execution and bounded evidence gathering; `medium_llm` takes on settled component
work that needs local judgment. `high_llm` is for work where ambiguity and wider
perspective matter, while `xhigh_llm` is for strategic or architectural decisions
whose consequences are broad and whose mistakes may be hard to detect. These
are working hypotheses for assigning responsibility economically, not fixed
limits on how much work each model can do or guarantees of automatic routing.
