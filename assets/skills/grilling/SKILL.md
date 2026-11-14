---
name: grilling
description: Grill the user relentlessly about a plan, decision, or idea. Use when the user wants to stress-test their thinking, or uses any 'grill' trigger phrases.
---

Interview the user relentlessly until you reach a shared understanding. Map this as a **design tree**: every decision branches into the decisions that hang off it.

Work the tree in **rounds**. The **frontier** is every decision whose prerequisites are already settled: the questions you can ask _now_ without guessing at answers you haven't heard yet. Ask the whole frontier in one round: number each question and give your recommended answer. Then wait for the user's answers before the next round.

Format a round like so:

```
❓ **Q1** - **<question title>**: <question body, might be multiple paragraphs, including multiple choices>

➡️ <your recommended answer>

---

❓ **Q2** - **<question title>**: <question body, might be multiple paragraphs, including multiple choices>

➡️ <your recommended answer>
```

Each round the user answers reshapes the tree: settled decisions push the frontier outward and unblock questions that depended on them. Recompute the frontier and ask the next round. A question whose answer depends on another question still open in this round belongs to a _later_ round, not this one.

Find _facts_ yourself instead of asking the user for facts you can look up. If a question needs a fact from the environment, such as the filesystem or tools, dispatch a sub-agent to find it. Ask questions that do not depend on that investigation now. Wait for the investigation result before you ask dependent questions. The user makes the _decisions_. Ask the user to make each decision and wait for the answer.

The session is done when the frontier is empty: every branch of the design tree visited, nothing left silently assumed. Do not act on it until the user confirms you have reached a shared understanding.

## Attribution

Copied from the `grilling` skill in [mattpocock/skills](https://github.com/mattpocock/skills/blob/main/skills/productivity/grilling/SKILL.md) by Matt Pocock. MIT licensed, see `LICENSE.txt` in this directory.
